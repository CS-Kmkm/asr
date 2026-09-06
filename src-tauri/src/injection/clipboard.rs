//! Eager, lossless-enough materialization of the Windows clipboard.
//!
//! Clipboard ownership is deliberately represented by raw handles here: the
//! handles are copied while the clipboard is open, then transferred one by
//! one only after `SetClipboardData` succeeds.

use super::{ClipboardExclusion, InjectionError};
use std::mem::size_of;
use std::ptr;
use std::thread;
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CopyEnhMetaFileW, DeleteEnhMetaFile, DeleteMetaFile, DeleteObject, HENHMETAFILE, HGDIOBJ,
    HMETAFILE,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
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
const RETRIES: usize = 10;

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
    _owner: HWND,
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
            let _ = DestroyWindow(self._owner);
        }
    }
}

unsafe extern "system" fn owner_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

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
    let owner = owner_window()?;
    for _ in 0..RETRIES {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return Ok(ClipboardGuard { _owner: owner });
        }
        thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        let _ = DestroyWindow(owner);
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
    entry.data = match std::mem::replace(&mut entry.data, OwnedData::Hglobal(HGLOBAL::default())) {
        OwnedData::Hglobal(_) => OwnedData::Hglobal(HGLOBAL::default()),
        OwnedData::Gdi { .. } => OwnedData::Gdi {
            handle: HGDIOBJ::default(),
            kind: GdiKind::Object,
        },
        OwnedData::EnhMeta(_) => OwnedData::EnhMeta(HENHMETAFILE::default()),
    };
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

fn apply_history_exclusion() -> bool {
    let mut ok = true;
    ok &= marker("ExcludeClipboardContentFromMonitorProcessing")
        .is_some_and(|format| set_marker(format, None));
    ok &= marker("CanIncludeInClipboardHistory").is_some_and(|format| set_marker(format, Some(0)));
    ok &= marker("CanUploadToCloudClipboard").is_some_and(|format| set_marker(format, Some(0)));
    ok
}

pub(super) fn snapshot() -> Result<Snapshot, InjectionError> {
    let _guard = open_clipboard()?;
    let mut entries = Vec::new();
    let mut format = unsafe { EnumClipboardFormats(0) };
    while format != 0 {
        if entries.len() >= MAX_FORMATS
            || format == CF_OWNERDISPLAY
            || (CF_PRIVATEFIRST..=CF_PRIVATELAST).contains(&format)
        {
            return Err(InjectionError::ClipboardUnavailable);
        }
        let handle = unsafe { GetClipboardData(format) }
            .map_err(|_| InjectionError::ClipboardUnavailable)?;
        let data = duplicate_entry(format, handle)?;
        entries.push(Entry { format, data });
        format = unsafe { EnumClipboardFormats(format) };
    }
    Ok(Snapshot { entries })
}

pub(super) fn write(text: &str, exclusion: ClipboardExclusion) -> Result<u32, InjectionError> {
    let _guard = open_clipboard()?;
    unsafe { EmptyClipboard() }.map_err(|_| InjectionError::ClipboardUnavailable)?;
    if exclusion == ClipboardExclusion::ExcludeFromHistory && !apply_history_exclusion() {
        return Err(InjectionError::ClipboardUnavailable);
    }
    let utf16: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let allocation = unsafe { GlobalAlloc(GMEM_MOVEABLE, utf16.len() * size_of::<u16>()) }
        .map_err(|_| InjectionError::ClipboardUnavailable)?;
    let destination = unsafe { GlobalLock(allocation) } as *mut u16;
    if destination.is_null() {
        unsafe {
            let _ = GlobalFree(allocation);
        }
        return Err(InjectionError::ClipboardUnavailable);
    }
    unsafe {
        ptr::copy_nonoverlapping(utf16.as_ptr(), destination, utf16.len());
        let _ = GlobalUnlock(allocation);
    }
    unsafe { SetClipboardData(CF_UNICODETEXT, HANDLE(allocation.0)) }.map_err(|_| {
        unsafe {
            let _ = GlobalFree(allocation);
        };
        InjectionError::ClipboardUnavailable
    })?;
    Ok(unsafe { GetClipboardSequenceNumber() })
}

pub(super) fn restore(
    mut snapshot: Snapshot,
    expected_sequence: u32,
) -> Result<bool, InjectionError> {
    let _guard = open_clipboard()?;
    if unsafe { GetClipboardSequenceNumber() } != expected_sequence {
        return Ok(false);
    }
    unsafe { EmptyClipboard() }.map_err(|_| InjectionError::ClipboardUnavailable)?;
    for entry in &mut snapshot.entries {
        set_entry(entry)?;
    }
    Ok(true)
}
