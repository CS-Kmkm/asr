//! Eager, lossless-enough materialization of the Windows clipboard.
//!
//! Clipboard ownership is deliberately represented by raw handles here: the
//! handles are copied while the clipboard is open, then transferred one by
//! one only after `SetClipboardData` succeeds.

use super::{
    ClipboardExclusion, ClipboardStamp, ClipboardWrite, InjectionError, TargetWindow, WriteError,
};
use std::mem::size_of;
use std::ptr;
use std::thread;
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, GlobalFree, ERROR_SUCCESS, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CopyEnhMetaFileW, DeleteEnhMetaFile, DeleteMetaFile, DeleteObject, HENHMETAFILE, HGDIOBJ,
    HMETAFILE,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::OleDuplicateData;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, HWND_MESSAGE, WNDCLASSW,
};

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_OEMTEXT: u32 = 7;
const CF_DIB: u32 = 8;
const CF_PALETTE: u32 = 9;
const CF_UNICODETEXT: u32 = 13;
const CF_DIBV5: u32 = 17;
const CF_LOCALE: u32 = 16;
const CF_ENHMETAFILE: u32 = 14;
const CF_HDROP: u32 = 15;
const CF_OWNERDISPLAY: u32 = 0x0080;
const CF_PRIVATEFIRST: u32 = 0x0200;
const CF_PRIVATELAST: u32 = 0x02ff;
const MAX_FORMATS: usize = 128;
const MAX_HGLOBAL: usize = 64 * 1024 * 1024;
/// Waits between attempts to open a clipboard that another process (a
/// clipboard manager, rdpclip) holds open. About 155 ms in total, so a brief
/// lock is not mistaken for a clipboard that cannot be preserved.
const OPEN_BACKOFF_MS: [u64; 6] = [5, 10, 20, 30, 40, 50];

#[derive(Debug)]
pub(super) struct Snapshot {
    entries: Vec<Entry>,
}

#[derive(Debug)]
struct Entry {
    format: u32,
    data: OwnedData,
}

#[derive(Debug)]
enum OwnedData {
    Hglobal(HGLOBAL),
    Gdi { handle: HGDIOBJ, kind: GdiKind },
    EnhMeta(HENHMETAFILE),
}

#[derive(Clone, Copy, Debug)]
enum GdiKind {
    Object,
    MetaFilePict,
}

impl Drop for OwnedData {
    fn drop(&mut self) {
        unsafe {
            match self {
                Self::Hglobal(handle) => {
                    if !handle.0.is_null() {
                        let _ = GlobalFree(*handle);
                    }
                }
                Self::Gdi { handle, kind } => match kind {
                    GdiKind::Object => {
                        let _ = DeleteObject(*handle);
                    }
                    GdiKind::MetaFilePict => {
                        let block = HGLOBAL(handle.0);
                        let pointer = GlobalLock(block)
                            as *mut windows::Win32::System::DataExchange::METAFILEPICT;
                        if !pointer.is_null() {
                            let metafile: HMETAFILE = (*pointer).hMF;
                            if !metafile.0.is_null() {
                                let _ = DeleteMetaFile(metafile);
                            }
                            let _ = GlobalUnlock(block);
                        }
                        let _ = GlobalFree(HGLOBAL(handle.0));
                    }
                },
                Self::EnhMeta(handle) => {
                    if !handle.0.is_null() {
                        let _ = DeleteEnhMetaFile(*handle);
                    }
                }
            }
        }
    }
}

struct ClipboardGuard {
    owner: HWND,
    open: bool,
    destroy_owner: bool,
}

impl ClipboardGuard {
    fn close(&mut self) -> Result<(), InjectionError> {
        unsafe { CloseClipboard() }.map_err(|_| InjectionError::ClipboardUnavailable)?;
        self.open = false;
        Ok(())
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            if self.open {
                let _ = CloseClipboard();
            }
            if self.destroy_owner {
                let _ = DestroyWindow(self.owner);
            }
        }
    }
}

unsafe extern "system" fn owner_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    if delayed::handle_message(hwnd, msg, wparam) {
        return windows::Win32::Foundation::LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

mod delayed;

fn owner_window() -> Result<HWND, InjectionError> {
    static CLASS: &[u16] = &[
        b'L' as u16,
        b'V' as u16,
        b'I' as u16,
        b'_' as u16,
        b'C' as u16,
        b'l' as u16,
        b'i' as u16,
        b'p' as u16,
        b'_' as u16,
        b'O' as u16,
        b'w' as u16,
        b'n' as u16,
        0,
    ];
    static REGISTERED: std::sync::OnceLock<Result<(), ()>> = std::sync::OnceLock::new();
    unsafe {
        if REGISTERED
            .get_or_init(|| {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(owner_proc),
                    lpszClassName: PCWSTR(CLASS.as_ptr()),
                    ..Default::default()
                };
                if RegisterClassW(&class) == 0 {
                    // A prior registration by this process is harmless.
                    let error = windows::Win32::Foundation::GetLastError();
                    if error.0 != 1410 {
                        return Err(());
                    }
                }
                Ok(())
            })
            .is_err()
        {
            return Err(InjectionError::ClipboardUnavailable);
        }
        CreateWindowExW(
            Default::default(),
            PCWSTR(CLASS.as_ptr()),
            PCWSTR(CLASS.as_ptr()),
            Default::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            None,
            None,
        )
        .map_err(|_| InjectionError::ClipboardUnavailable)
    }
}

fn open_clipboard() -> Result<ClipboardGuard, InjectionError> {
    open_with_owner(owner_window()?, true)
}

fn open_with_owner(owner: HWND, destroy_owner: bool) -> Result<ClipboardGuard, InjectionError> {
    for delay in std::iter::once(0).chain(OPEN_BACKOFF_MS) {
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return Ok(ClipboardGuard {
                owner,
                open: true,
                destroy_owner,
            });
        }
    }
    if destroy_owner {
        unsafe {
            let _ = DestroyWindow(owner);
        }
    }
    Err(InjectionError::ClipboardUnavailable)
}

fn is_hglobal_format(format: u32) -> bool {
    matches!(
        format,
        CF_TEXT | CF_OEMTEXT | CF_UNICODETEXT | CF_LOCALE | CF_DIB | CF_DIBV5 | CF_HDROP
    ) || format >= 0xc000
}

fn duplicate_hglobal(handle: HANDLE) -> Result<OwnedData, InjectionError> {
    let source = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(source) };
    if size == 0 || size > MAX_HGLOBAL {
        return Err(InjectionError::ClipboardUnavailable);
    }
    let allocation = unsafe { GlobalAlloc(GMEM_MOVEABLE, size) }
        .map_err(|_| InjectionError::ClipboardUnavailable)?;
    let source_ptr = unsafe { GlobalLock(source) } as *const u8;
    let destination = unsafe { GlobalLock(allocation) } as *mut u8;
    if source_ptr.is_null() || destination.is_null() {
        unsafe {
            if !destination.is_null() {
                let _ = GlobalUnlock(allocation);
            }
            if !source_ptr.is_null() {
                let _ = GlobalUnlock(source);
            }
            let _ = GlobalFree(allocation);
        }
        return Err(InjectionError::ClipboardUnavailable);
    }
    unsafe {
        ptr::copy_nonoverlapping(source_ptr, destination, size);
        let _ = GlobalUnlock(allocation);
        let _ = GlobalUnlock(source);
    }
    Ok(OwnedData::Hglobal(allocation))
}

fn duplicate_entry(format: u32, handle: HANDLE) -> Result<OwnedData, InjectionError> {
    if is_hglobal_format(format) {
        return duplicate_hglobal(handle);
    }
    match format {
        CF_BITMAP | CF_PALETTE => {
            let copy = unsafe {
                OleDuplicateData(
                    handle,
                    windows::Win32::System::Ole::CLIPBOARD_FORMAT(format as u16),
                    GMEM_MOVEABLE,
                )
            };
            if copy.0.is_null() {
                Err(InjectionError::ClipboardUnavailable)
            } else {
                Ok(OwnedData::Gdi {
                    handle: HGDIOBJ(copy.0),
                    kind: GdiKind::Object,
                })
            }
        }
        CF_METAFILEPICT => {
            let copy = unsafe {
                OleDuplicateData(
                    handle,
                    windows::Win32::System::Ole::CLIPBOARD_FORMAT(format as u16),
                    GMEM_MOVEABLE,
                )
            };
            if copy.0.is_null() {
                Err(InjectionError::ClipboardUnavailable)
            } else {
                Ok(OwnedData::Gdi {
                    handle: HGDIOBJ(copy.0),
                    kind: GdiKind::MetaFilePict,
                })
            }
        }
        CF_ENHMETAFILE => {
            let copy = unsafe {
                CopyEnhMetaFileW(
                    windows::Win32::Graphics::Gdi::HENHMETAFILE(handle.0),
                    PCWSTR::null(),
                )
            };
            if copy.0.is_null() {
                Err(InjectionError::ClipboardUnavailable)
            } else {
                Ok(OwnedData::EnhMeta(copy))
            }
        }
        _ => Err(InjectionError::ClipboardUnavailable),
    }
}

fn set_entry(entry: &mut Entry) -> Result<(), InjectionError> {
    let handle = match &entry.data {
        OwnedData::Hglobal(value) => HANDLE(value.0),
        OwnedData::Gdi { handle, .. } => HANDLE(handle.0),
        OwnedData::EnhMeta(handle) => HANDLE(handle.0),
    };
    unsafe { SetClipboardData(entry.format, handle) }
        .map_err(|_| InjectionError::ClipboardUnavailable)?;
    // The clipboard owns the handle now; prevent Drop from freeing it.
    match &mut entry.data {
        OwnedData::Hglobal(handle) => *handle = HGLOBAL::default(),
        OwnedData::Gdi { handle, .. } => *handle = HGDIOBJ::default(),
        OwnedData::EnhMeta(handle) => *handle = HENHMETAFILE::default(),
    }
    Ok(())
}

fn registered_format(name: &str, cache: &std::sync::OnceLock<Option<u32>>) -> Option<u32> {
    *cache.get_or_init(|| {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let value = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
        (value != 0).then_some(value)
    })
}

fn marker(name: &str) -> Option<u32> {
    // These names are static and registration is process-wide; keeping one
    // cache per marker avoids repeatedly asking the shell to register atoms.
    match name {
        "ExcludeClipboardContentFromMonitorProcessing" => {
            static C: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
            registered_format(name, &C)
        }
        "CanIncludeInClipboardHistory" => {
            static C: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
            registered_format(name, &C)
        }
        "CanUploadToCloudClipboard" => {
            static C: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
            registered_format(name, &C)
        }
        _ => None,
    }
}

fn set_marker(format: u32, value: Option<u32>) -> bool {
    let bytes = value.map_or(1, |_| size_of::<u32>());
    let allocation = match unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) } {
        Ok(value) => value,
        Err(_) => return false,
    };
    let pointer = unsafe { GlobalLock(allocation) } as *mut u8;
    if pointer.is_null() {
        unsafe {
            let _ = GlobalFree(allocation);
        }
        return false;
    }
    unsafe {
        if let Some(number) = value {
            *(pointer as *mut u32) = number;
        } else {
            *pointer = 0;
        }
        let _ = GlobalUnlock(allocation);
    }
    if unsafe { SetClipboardData(format, HANDLE(allocation.0)) }.is_err() {
        unsafe {
            let _ = GlobalFree(allocation);
        }
        false
    } else {
        true
    }
}

/// Registered marker formats that keep clipboard content out of Win+V
/// history, the cloud clipboard, and clipboard monitors, with their values.
const HISTORY_MARKERS: [(&str, Option<u32>); 3] = [
    ("ExcludeClipboardContentFromMonitorProcessing", None),
    ("CanIncludeInClipboardHistory", Some(0)),
    ("CanUploadToCloudClipboard", Some(0)),
];

fn apply_history_exclusion() -> bool {
    apply_history_exclusion_except(&[])
}

/// Sets every history marker whose format is not in `present`. A marker
/// whose format cannot be registered or set makes the result false.
fn apply_history_exclusion_except(present: &[u32]) -> bool {
    let mut ok = true;
    for (name, value) in HISTORY_MARKERS {
        ok &= marker(name)
            .is_some_and(|format| !needs_marker(format, present) || set_marker(format, value));
    }
    ok
}

/// A marker already on the clipboard (restored from a snapshot) keeps the
/// owner's value; setting the same format twice would replace it.
fn needs_marker(format: u32, present: &[u32]) -> bool {
    !present.contains(&format)
}

fn can_add_restore_markers(format_count: usize) -> bool {
    (1..=MAX_FORMATS - HISTORY_MARKERS.len()).contains(&format_count)
}

/// A clipboard that stays locked is `ClipboardUnavailable`; one that opens
/// but cannot be copied is `ClipboardUnpreservable`.
#[cfg(test)]
pub(super) fn snapshot() -> Result<Snapshot, InjectionError> {
    let _guard = open_clipboard()?;
    snapshot_opened()
}

fn snapshot_opened() -> Result<Snapshot, InjectionError> {
    let mut entries = Vec::new();
    let mut format = unsafe { EnumClipboardFormats(0) };
    while format != 0 {
        if entries.len() >= MAX_FORMATS
            || format == CF_OWNERDISPLAY
            || (CF_PRIVATEFIRST..=CF_PRIVATELAST).contains(&format)
        {
            return Err(InjectionError::ClipboardUnpreservable);
        }
        let handle = unsafe { GetClipboardData(format) }
            .map_err(|_| InjectionError::ClipboardUnpreservable)?;
        let data =
            duplicate_entry(format, handle).map_err(|_| InjectionError::ClipboardUnpreservable)?;
        entries.push(Entry { format, data });
        format = unsafe { EnumClipboardFormats(format) };
    }
    if unsafe { GetLastError() } != ERROR_SUCCESS {
        return Err(InjectionError::ClipboardUnpreservable);
    }
    Ok(Snapshot { entries })
}

pub(super) fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Hold the clipboard open across capture and replacement. After closing,
/// only accept the sequence if the same window still owns the clipboard.
pub(super) fn write(
    text: &str,
    exclusion: ClipboardExclusion,
    preserve: bool,
) -> Result<ClipboardWrite<Snapshot>, WriteError<Snapshot>> {
    let payload = Payload::Eager(unicode_entry(text).map_err(WriteError::Unowned)?);
    publish(payload, exclusion, preserve)
}

pub(super) fn write_terminal(
    text: &str,
    target: &TargetWindow,
) -> Result<ClipboardWrite<Snapshot>, WriteError<Snapshot>> {
    let exclusion = ClipboardExclusion::ExcludeFromHistory;
    match delayed::Receipt::start(text, target) {
        Some(receipt) => publish(Payload::Delayed(receipt), exclusion, true),
        None => write(text, exclusion, true),
    }
}

enum Payload {
    Eager(Entry),
    Delayed(delayed::Receipt),
}

fn publish(
    mut payload: Payload,
    exclusion: ClipboardExclusion,
    preserve: bool,
) -> Result<ClipboardWrite<Snapshot>, WriteError<Snapshot>> {
    // Allocate the Unicode handle before clearing the user's clipboard.
    let mut guard = match &payload {
        Payload::Eager(_) => open_clipboard(),
        Payload::Delayed(receipt) => open_with_owner(receipt.owner(), false),
    }
    .map_err(WriteError::Unowned)?;
    let previous = if preserve {
        Some(
            snapshot_opened()
                .map_err(|_| WriteError::Unowned(InjectionError::ClipboardProtected))?,
        )
    } else {
        None
    };
    unsafe { EmptyClipboard() }
        .map_err(|_| WriteError::Unowned(InjectionError::ClipboardUnavailable))?;
    let result =
        if exclusion == ClipboardExclusion::ExcludeFromHistory && !apply_history_exclusion() {
            Err(InjectionError::ClipboardUnavailable)
        } else {
            match &mut payload {
                Payload::Eager(entry) => set_entry(entry),
                Payload::Delayed(receipt) => {
                    // A successful delayed registration also returns NULL.
                    unsafe {
                        let _ = SetClipboardData(CF_UNICODETEXT, None);
                        IsClipboardFormatAvailable(CF_UNICODETEXT)
                            .map_err(|_| InjectionError::ClipboardUnavailable)
                            .map(|()| receipt.registered())
                    }
                }
            }
        };
    // Close materializes synthesized formats. Read the sequence before the
    // owner check, while the unique owner window is still alive.
    guard.close().map_err(|_| WriteError::CloseFailed)?;
    let sequence = sequence();
    if !unsafe { GetClipboardOwner() }.is_ok_and(|owner| owner == guard.owner) {
        return Err(WriteError::Unowned(InjectionError::ClipboardChanged));
    }
    let mut write = ClipboardWrite::eager(sequence, previous);
    if let Payload::Delayed(receipt) = payload {
        write.receipt = Some(Box::new(receipt));
    }
    match result {
        Ok(()) => Ok(write),
        Err(error) => Err(WriteError::AfterEmpty { error, write }),
    }
}

fn unicode_entry(text: &str) -> Result<Entry, InjectionError> {
    if text.contains('\0') {
        return Err(InjectionError::BackendFailure(
            "clipboard text contains an embedded NUL",
        ));
    }
    let utf16: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let allocation = unsafe { GlobalAlloc(GMEM_MOVEABLE, utf16.len() * size_of::<u16>()) }
        .map_err(|_| InjectionError::ClipboardUnavailable)?;
    let data = OwnedData::Hglobal(allocation);
    let destination = unsafe { GlobalLock(allocation) } as *mut u16;
    if destination.is_null() {
        return Err(InjectionError::ClipboardUnavailable);
    }
    unsafe {
        ptr::copy_nonoverlapping(utf16.as_ptr(), destination, utf16.len());
        let _ = GlobalUnlock(allocation);
    }
    Ok(Entry {
        format: CF_UNICODETEXT,
        data,
    })
}

#[cfg(test)]
pub(super) fn restore(snapshot: Snapshot, expected_sequence: u32) -> Result<bool, InjectionError> {
    restore_owned(
        snapshot,
        ClipboardStamp {
            sequence: expected_sequence,
            owner: None,
        },
    )
}

pub(super) fn restore_owned(
    mut snapshot: Snapshot,
    expected: ClipboardStamp,
) -> Result<bool, InjectionError> {
    let mut guard = open_clipboard()?;
    if sequence() != expected.sequence
        || expected.owner.is_some_and(|owner| {
            !unsafe { GetClipboardOwner() }.is_ok_and(|current| current.0 as isize == owner)
        })
    {
        guard.close()?;
        return Ok(false);
    }
    unsafe { EmptyClipboard() }.map_err(|_| InjectionError::ClipboardUnavailable)?;
    // Include marker publication in the work completed before closing.
    let result = restore_entries(
        &mut snapshot.entries,
        set_entry,
        |entries| {
            // Preserve an empty clipboard and leave room under the format cap.
            if can_add_restore_markers(entries.len()) {
                let present: Vec<u32> = entries.iter().map(|entry| entry.format).collect();
                if !apply_history_exclusion_except(&present) {
                    return Err(InjectionError::ClipboardUnavailable);
                }
            }
            Ok(())
        },
        || guard.close(),
    );
    result.map(|()| true)
}

fn restore_entries<T>(
    entries: &mut [T],
    mut publish: impl FnMut(&mut T) -> Result<(), InjectionError>,
    finish_publication: impl FnOnce(&[T]) -> Result<(), InjectionError>,
    close: impl FnOnce() -> Result<(), InjectionError>,
) -> Result<(), InjectionError> {
    let mut result = Ok(());
    for entry in entries.iter_mut() {
        result = result.and(publish(entry));
    }
    result.and(finish_publication(entries)).and(close())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_attempts_later_formats_and_reports_publication_or_close_failure() {
        for (failed_format, failed_close) in [(true, false), (false, true), (false, false)] {
            let calls = std::cell::RefCell::new(Vec::new());
            let result = restore_entries(
                &mut [1, 2, 3],
                |format| {
                    calls.borrow_mut().push(*format);
                    if *format == 2 && failed_format {
                        return Err(InjectionError::ClipboardUnavailable);
                    }
                    Ok(())
                },
                |_| {
                    calls.borrow_mut().push(4);
                    Ok(())
                },
                || {
                    calls.borrow_mut().push(5);
                    if failed_close {
                        return Err(InjectionError::ClipboardUnavailable);
                    }
                    Ok(())
                },
            );
            assert_eq!(*calls.borrow(), [1, 2, 3, 4, 5]);
            assert_eq!(result.is_err(), failed_format || failed_close);
        }
    }

    #[test]
    fn embedded_nul_cannot_be_published_as_truncated_unicode_text() {
        assert!(matches!(
            unicode_entry("prefix\0suffix"),
            Err(InjectionError::BackendFailure(_))
        ));
    }

    #[test]
    fn a_locked_clipboard_is_retried_for_a_bounded_time() {
        let total: u64 = OPEN_BACKOFF_MS.iter().sum();
        assert!((100..=200).contains(&total), "{total} ms");
        assert!(OPEN_BACKOFF_MS.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn restore_adds_only_history_markers_the_snapshot_lacks() {
        let present = [CF_UNICODETEXT, 0xC123];
        assert!(!needs_marker(0xC123, &present));
        assert!(needs_marker(0xC124, &present));
        assert!(needs_marker(0xC124, &[]));
        assert_eq!(HISTORY_MARKERS.len(), 3);
        assert!(!can_add_restore_markers(0));
        assert!(can_add_restore_markers(MAX_FORMATS - 3));
        assert!(!can_add_restore_markers(MAX_FORMATS - 2));
    }

    #[test]
    #[ignore = "changes the real clipboard and requires an interactive Windows desktop"]
    fn native_clipboard_write_and_restore_round_trip() {
        let write = write(
            "local voice input clipboard round-trip",
            ClipboardExclusion::ExcludeFromHistory,
            true,
        )
        .expect("write the test clipboard payload");
        assert!(restore(write.previous.unwrap(), write.sequence)
            .expect("restore the original clipboard"));
    }
}
