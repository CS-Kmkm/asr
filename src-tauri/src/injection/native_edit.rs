//! Win32 Edit controls that expose no UI Automation text range.
//!
//! The control's text and selection are read and edited with window messages,
//! which the system marshals across processes for standard controls. Edits use
//! WM_PASTE and WM_CLEAR rather than keystrokes, so they never pass through an
//! IME composition the control's thread may hold. Positions are UTF-16 units of
//! the raw text, whose CRLF line breaks `normalize_text` turns into LF. Target
//! text is inspected transiently and never logged.

use super::{normalize_text, InjectionError, TargetText, TargetWindow};
use std::ffi::c_void;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, IsWindowUnicode, SendMessageTimeoutW, SEND_MESSAGE_TIMEOUT_FLAGS,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_CLEAR, WM_GETTEXT, WM_GETTEXTLENGTH, WM_PASTE,
};

const EM_GETSEL: u32 = 0x00B0;
const EM_SETSEL: u32 = 0x00B1;
const MESSAGE_TIMEOUT_MS: u32 = 200;
/// Pasting runs the control's own paste handling, which may take longer.
const EDIT_TIMEOUT_MS: u32 = 2000;
/// EM_GETSEL packs both positions into 16-bit words, so longer text cannot be
/// addressed safely and is treated as unreadable.
const MAX_UNITS: usize = 0xFFFF;
/// Distinguishes a window-handle identity from a UI Automation runtime id.
const IDENTITY_MARKER: i32 = 0x4E45_4454;

fn unavailable() -> InjectionError {
    InjectionError::BackendFailure("the focused control does not expose a verifiable text range")
}

fn control(target: &TargetWindow) -> HWND {
    HWND(target.control_handle as *mut c_void)
}

/// Standard Win32 Edit controls, including Windows Forms text boxes, which
/// superclass them. RichEdit counts positions differently and is excluded.
/// So are ANSI windows: WM_GETTEXT is converted to UTF-16 for us, but their
/// EM_GETSEL/EM_SETSEL positions may count bytes of a multibyte code page,
/// which would select a different range than the one read.
pub(super) fn is_native_edit(target: &TargetWindow) -> bool {
    if !unsafe { IsWindowUnicode(control(target)) }.as_bool() {
        return false;
    }
    let mut class_name = [0u16; 128];
    let length = unsafe { GetClassNameW(control(target), &mut class_name) };
    if length <= 0 {
        return false;
    }
    let class_name = String::from_utf16_lossy(&class_name[..length as usize]);
    class_name.eq_ignore_ascii_case("Edit")
        || class_name
            .to_ascii_uppercase()
            .starts_with("WINDOWSFORMS10.EDIT.")
}

fn send(window: HWND, message: u32, wparam: usize, lparam: isize) -> Option<usize> {
    send_with_timeout(window, message, wparam, lparam, MESSAGE_TIMEOUT_MS)
}

fn send_with_timeout(
    window: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
    timeout_ms: u32,
) -> Option<usize> {
    let mut result = 0usize;
    let sent = unsafe {
        SendMessageTimeoutW(
            window,
            message,
            WPARAM(wparam),
            LPARAM(lparam),
            SEND_MESSAGE_TIMEOUT_FLAGS(SMTO_ABORTIFHUNG.0 | SMTO_BLOCK.0),
            timeout_ms,
            Some(&mut result),
        )
    };
    (sent.0 != 0).then_some(result)
}

/// Raw UTF-16 text and selection `(start, end)` of a native Edit control.
fn read_raw(target: &TargetWindow) -> Result<(Vec<u16>, usize, usize), InjectionError> {
    if !is_native_edit(target) {
        return Err(unavailable());
    }
    let window = control(target);
    let length = send(window, WM_GETTEXTLENGTH, 0, 0).ok_or_else(unavailable)?;
    if length > MAX_UNITS {
        return Err(unavailable());
    }
    let mut buffer = vec![0u16; length + 1];
    let copied = send(
        window,
        WM_GETTEXT,
        buffer.len(),
        buffer.as_mut_ptr() as isize,
    )
    .ok_or_else(unavailable)?;
    if copied > length {
        return Err(unavailable());
    }
    buffer.truncate(copied);
    let selection = send(window, EM_GETSEL, 0, 0).ok_or_else(unavailable)?;
    if selection > u32::MAX as usize || selection as u32 == u32::MAX {
        return Err(unavailable());
    }
    Ok((buffer, selection & 0xFFFF, (selection >> 16) & 0xFFFF))
}

pub(super) fn read(target: &TargetWindow) -> Result<TargetText, InjectionError> {
    let (units, start, end) = read_raw(target)?;
    let (before, selected, after) = split_state(&units, start, end).ok_or_else(unavailable)?;
    let handle = target.control_handle as i64;
    Ok(TargetText {
        identity: vec![IDENTITY_MARKER, handle as i32, (handle >> 32) as i32],
        before,
        selected,
        after,
    })
}

/// Selects the `text` that ends at the caret when the control still holds
/// `expected`. The batch layer confirms the selection before replacing it.
pub(super) fn select_recent(
    target: &TargetWindow,
    expected: &TargetText,
    text: &str,
) -> Result<bool, InjectionError> {
    let (units, start, end) = read_raw(target)?;
    let Some((before, selected, after)) = split_state(&units, start, end) else {
        return Ok(false);
    };
    if start != end
        || before != expected.before
        || selected != expected.selected
        || after != expected.after
    {
        return Ok(false);
    }
    let Some(from) = recent_start(&units, end, text) else {
        return Ok(false);
    };
    send(control(target), EM_SETSEL, from, end as isize).ok_or_else(unavailable)?;
    Ok(true)
}

/// Pastes the clipboard over the selection. A timed-out message may still be
/// processed later, so the paste always counts as possibly queued and is
/// confirmed by reading the control, never retried.
pub(super) fn paste(target: &TargetWindow) -> bool {
    let _ = send_with_timeout(control(target), WM_PASTE, 0, 0, EDIT_TIMEOUT_MS);
    true
}

/// Deletes the selection.
pub(super) fn clear_selection(target: &TargetWindow) -> bool {
    send_with_timeout(control(target), WM_CLEAR, 0, 0, EDIT_TIMEOUT_MS).is_some()
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

/// A position inside a surrogate pair or a CRLF is not a text boundary.
fn is_boundary(units: &[u16], at: usize) -> bool {
    at == 0
        || at >= units.len()
        || !(is_high_surrogate(units[at - 1]) && is_low_surrogate(units[at])
            || units[at - 1] == u16::from(b'\r') && units[at] == u16::from(b'\n'))
}

/// Normalized text before, inside and after the selection.
fn split_state(units: &[u16], start: usize, end: usize) -> Option<(String, String, String)> {
    if start > end || end > units.len() || !is_boundary(units, start) || !is_boundary(units, end) {
        return None;
    }
    let part = |range: &[u16]| {
        String::from_utf16(range)
            .ok()
            .map(|text| normalize_text(&text))
    };
    Some((
        part(&units[..start])?,
        part(&units[start..end])?,
        part(&units[end..])?,
    ))
}

/// Start of the raw range ending at `caret` that reads as `text` once
/// normalized, stepping over surrogate pairs and CRLF as single characters.
fn recent_start(units: &[u16], caret: usize, text: &str) -> Option<usize> {
    let wanted = normalize_text(text);
    if wanted.is_empty() || caret > units.len() || !is_boundary(units, caret) {
        return None;
    }
    let mut start = caret;
    while start > 0 {
        start -= 1;
        while !is_boundary(units, start) {
            start -= 1;
        }
        let candidate = normalize_text(&String::from_utf16(&units[start..caret]).ok()?);
        if candidate == wanted {
            return Some(start);
        }
        if !wanted.ends_with(&candidate) {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn selection_splits_on_utf16_positions_and_normalizes_line_breaks() {
        let text = units("a\r\n📝b");
        // "a" CR LF high low "b": the caret after the emoji is position 5.
        assert_eq!(
            split_state(&text, 5, 6),
            Some(("a\n📝".into(), "b".into(), String::new()))
        );
        assert_eq!(split_state(&text, 0, 0).unwrap().2, "a\n📝b");
        // Inside CRLF, inside the surrogate pair, or out of range.
        assert_eq!(split_state(&text, 2, 2), None);
        assert_eq!(split_state(&text, 4, 4), None);
        assert_eq!(split_state(&text, 6, 5), None);
        assert_eq!(split_state(&text, 0, 7), None);
    }

    #[test]
    fn recent_text_is_found_back_from_the_caret() {
        let text = units("prefix 仮の\r\n文章📝 suffix");
        let caret = text.len() - " suffix".len();
        let start = recent_start(&text, caret, "仮の\n文章📝").unwrap();
        assert_eq!(
            String::from_utf16(&text[start..caret]).unwrap(),
            "仮の\r\n文章📝"
        );
        assert_eq!(recent_start(&text, caret, "仮の\r\n文章📝"), Some(start));
        // Text that does not end at the caret, or is not there at all.
        assert_eq!(recent_start(&text, caret, "prefix"), None);
        assert_eq!(recent_start(&text, caret, "missing 仮の\n文章📝"), None);
        assert_eq!(recent_start(&text, caret, ""), None);
    }
}
