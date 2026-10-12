//! Platform-independent decisions about the keys this app injects. The
//! Windows backend turns them into `SendInput` events.

use crate::types::AppContext;

const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_INSERT: u16 = 0x2D;
const VK_V: u16 = 0x56;

/// The keystroke that asks the target to paste the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PasteChord {
    CtrlV,
    /// For terminals and editors that read Ctrl+V as a control character.
    ShiftInsert,
}

impl PasteChord {
    /// Virtual-key codes of the modifier and the key, pressed in this order
    /// and released in reverse.
    pub(super) fn keys(self) -> (u16, u16) {
        match self {
            Self::CtrlV => (VK_CONTROL, VK_V),
            Self::ShiftInsert => (VK_SHIFT, VK_INSERT),
        }
    }
}

/// What `SendInput` of a chord's four events (modifier down, key down, key
/// up, modifier up) left to do, given how many it accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChordSent {
    /// The paste key's key-down was queued, so the paste may happen.
    pub(super) paste_queued: bool,
    /// Index of the first event still to send to release the keys pressed so
    /// far. The paste key-down is never sent again.
    pub(super) release_from: Option<usize>,
}

pub(super) fn chord_sent(sent: u32) -> ChordSent {
    ChordSent {
        paste_queued: sent >= 2,
        release_from: match sent {
            // Only the modifier went down: release it alone.
            1 => Some(3),
            // The key went down: release it, then the modifier.
            2 => Some(2),
            // The key is up: release the modifier.
            3 => Some(3),
            _ => None,
        },
    }
}

/// Executable stems (as normalized by `app_context`) whose Ctrl+V does not
/// paste: mintty and PuTTY send it to the shell as ^V, and Emacs reads it as
/// `scroll-up`. Shift+Insert pastes the Windows clipboard in all three.
const SHIFT_INSERT_APPS: [&str; 3] = ["mintty", "putty", "emacs"];

/// The paste chord for the target's executable. An unknown app, or a target
/// whose process lookup failed, gets Ctrl+V.
pub(super) fn paste_chord(context: Option<&AppContext>) -> PasteChord {
    match context.and_then(|context| context.app_key.as_deref()) {
        Some(stem) if SHIFT_INSERT_APPS.contains(&stem) => PasteChord::ShiftInsert,
        _ => PasteChord::CtrlV,
    }
}

/// Keys whose scan code carries the E0 prefix on a standard keyboard. They
/// need `KEYEVENTF_EXTENDEDKEY`, or the target sees the numeric-keypad key
/// with the same scan code (Insert would arrive as keypad 0 / Ins, which
/// NumLock and Shift can turn into a digit).
pub(super) fn is_extended_key(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 // Page Up, Page Down, End, Home, arrows
            | 0x2C // Print Screen
            | 0x2D // Insert
            | 0x2E // Delete
            | 0x5B..=0x5D // left and right Windows keys, Applications
            | 0x6F // numeric-keypad divide
            | 0x90 // Num Lock
            | 0xA3 // right Ctrl
            | 0xA5 // right Alt
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_context::from_executable_path;

    fn chord_for(path: Option<&str>) -> PasteChord {
        paste_chord(from_executable_path(path).as_ref())
    }

    #[test]
    fn known_terminals_and_editors_paste_with_shift_insert() {
        for path in [
            r"C:\Program Files\Git\usr\bin\mintty.exe",
            r"C:\Program Files\PuTTY\putty.exe",
            r"C:\Program Files\Emacs\bin\emacs.exe",
        ] {
            assert_eq!(chord_for(Some(path)), PasteChord::ShiftInsert, "{path}");
        }
        assert_eq!(PasteChord::ShiftInsert.keys(), (VK_SHIFT, VK_INSERT));
    }

    #[test]
    fn the_table_matches_normalized_stems() {
        assert_eq!(
            chord_for(Some(r"C:\msys64\usr\bin\MinTTY.EXE")),
            PasteChord::ShiftInsert
        );
        assert_eq!(chord_for(Some("PUTTY")), PasteChord::ShiftInsert);
        // Only whole stems match.
        assert_eq!(chord_for(Some(r"C:\Tools\puttygen.exe")), PasteChord::CtrlV);
        assert_eq!(chord_for(Some(r"C:\Tools\runemacs.exe")), PasteChord::CtrlV);
    }

    #[test]
    fn unknown_apps_and_failed_lookups_paste_with_ctrl_v() {
        assert_eq!(
            chord_for(Some(r"C:\Windows\System32\notepad.exe")),
            PasteChord::CtrlV
        );
        assert_eq!(
            chord_for(Some(
                r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\wt.exe"
            )),
            PasteChord::CtrlV
        );
        // An identified executable whose stem cannot be keyed.
        assert_eq!(chord_for(Some(r"C:\Tools\メモ.exe")), PasteChord::CtrlV);
        // The process lookup failed.
        assert_eq!(chord_for(None), PasteChord::CtrlV);
        assert_eq!(paste_chord(None), PasteChord::CtrlV);
        assert_eq!(PasteChord::CtrlV.keys(), (VK_CONTROL, VK_V));
    }

    #[test]
    fn a_partial_chord_releases_only_pressed_keys_and_never_replays_the_paste() {
        let sent = |paste_queued, release_from| ChordSent {
            paste_queued,
            release_from,
        };
        // Nothing was queued.
        assert_eq!(chord_sent(0), sent(false, None));
        // Only the modifier went down: no paste, release the modifier.
        assert_eq!(chord_sent(1), sent(false, Some(3)));
        // The paste key went down: the paste may happen.
        assert_eq!(chord_sent(2), sent(true, Some(2)));
        assert_eq!(chord_sent(3), sent(true, Some(3)));
        assert_eq!(chord_sent(4), sent(true, None));
    }

    #[test]
    fn insert_is_extended_and_the_other_injected_keys_are_not() {
        assert!(is_extended_key(VK_INSERT));
        assert!(is_extended_key(0x2E)); // Delete
        assert!(is_extended_key(0x25)); // Left arrow
        assert!(is_extended_key(0xA3)); // Right Ctrl
        const VK_BACK: u16 = 0x08;
        const VK_LCONTROL: u16 = 0xA2;
        for vk in [VK_SHIFT, VK_CONTROL, VK_V, VK_BACK, VK_LCONTROL] {
            assert!(!is_extended_key(vk), "{vk:#x}");
        }
        // Numeric-keypad 0 shares Insert's scan code but is not extended.
        assert!(!is_extended_key(0x60));
    }
}
