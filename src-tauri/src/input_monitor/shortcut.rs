//! Classifies only the configured recording chord as control input. Modifier
//! prefixes remain pending until the chord matches or become ordinary activity.

pub(super) struct ShortcutFilter {
    key: u32,
    modifiers: u8,
    held: u16,
    key_held: bool,
    consumed: bool,
}

impl ShortcutFilter {
    pub(super) fn parse(value: &str) -> Option<Self> {
        let shortcut = crate::parse_shortcut(value).ok()?.into_string();
        let mut parts: Vec<_> = shortcut.split('+').collect();
        let key = virtual_key(parts.pop()?)?;
        let mut modifiers = 0;
        for part in parts {
            modifiers |= match part {
                "shift" => 1,
                "control" => 2,
                "alt" => 4,
                "super" => 8,
                _ => return None,
            };
        }
        Some(Self {
            key,
            modifiers,
            held: 0,
            key_held: false,
            consumed: false,
        })
    }

    fn modifiers(&self) -> u8 {
        let mut mask = 0;
        for group in 0..4 {
            if self.held & (3 << (group * 2)) != 0 {
                mask |= 1 << group;
            }
        }
        mask
    }

    pub(super) fn pending(&self) -> bool {
        self.held != 0 || self.key_held
    }

    /// Called once before installing hooks so the start chord's remaining
    /// releases are attributed to that already-pressed chord.
    pub(super) fn initialize(&mut self, from_shortcut: bool, is_down: impl Fn(u32) -> bool) {
        for (index, vk) in [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C]
            .into_iter()
            .enumerate()
        {
            if is_down(vk) {
                self.held |= 1 << index;
            }
        }
        self.key_held = is_down(self.key);
        self.consumed = (self.key_held && self.modifiers() == self.modifiers)
            || (from_shortcut && self.pending() && self.modifiers() & !self.modifiers == 0);
    }

    pub(super) fn event(&mut self, vk: u32, down: bool) -> bool {
        let modifier = match vk {
            0x10 | 0xA0 => Some(0),
            0xA1 => Some(1),
            0x11 | 0xA2 => Some(2),
            0xA3 => Some(3),
            0x12 | 0xA4 => Some(4),
            0xA5 => Some(5),
            0x5B => Some(6),
            0x5C => Some(7),
            _ => None,
        };
        if let Some(index) = modifier {
            let was_held = self.held & (1 << index) != 0;
            if down && !was_held {
                self.consumed = false;
            }
            if down {
                self.held |= 1 << index;
            } else {
                self.held &= !(1 << index);
            }
            let activity = if down {
                self.modifiers() & !self.modifiers != 0
            } else {
                was_held && !self.consumed
            };
            if !self.pending() {
                self.consumed = false;
            }
            return activity;
        }
        if !down {
            if vk == self.key {
                self.key_held = false;
            }
            if !self.pending() {
                self.consumed = false;
            }
            return false;
        }
        if vk == self.key && self.modifiers() == self.modifiers {
            self.key_held = true;
            self.consumed = true;
            false
        } else {
            self.consumed = false;
            true
        }
    }
}

// Same virtual-key interpretation as the registered Windows global shortcut.
fn virtual_key(code: &str) -> Option<u32> {
    if let Some(letter) = code.strip_prefix("Key").filter(|s| s.len() == 1) {
        return Some(u32::from(letter.as_bytes()[0]));
    }
    for (prefix, base, max) in [("Digit", 0x30, 9), ("Numpad", 0x60, 9), ("F", 0x6F, 24)] {
        if let Some(number) = code
            .strip_prefix(prefix)
            .and_then(|s| s.parse::<u32>().ok())
        {
            if number <= max && (prefix != "F" || number > 0) {
                return Some(base + number);
            }
        }
    }
    Some(match code {
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Enter" | "NumpadEnter" => 0x0D,
        "Pause" | "MediaPause" => 0x13,
        "CapsLock" => 0x14,
        "Escape" => 0x1B,
        "Space" => 0x20,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "End" => 0x23,
        "Home" => 0x24,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "PrintScreen" => 0x2C,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "NumpadEqual" => 0x45,
        "NumpadMultiply" => 0x6A,
        "NumpadAdd" => 0x6B,
        "NumpadSubtract" => 0x6D,
        "NumpadDecimal" => 0x6E,
        "NumpadDivide" => 0x6F,
        "NumLock" => 0x90,
        "ScrollLock" => 0x91,
        "AudioVolumeMute" => 0xAD,
        "AudioVolumeDown" => 0xAE,
        "AudioVolumeUp" => 0xAF,
        "MediaTrackNext" => 0xB0,
        "MediaTrackPrevious" => 0xB1,
        "MediaStop" => 0xB2,
        "MediaPlayPause" => 0xB3,
        "MediaPlay" => 0xFA,
        "Semicolon" => 0xBA,
        "Equal" => 0xBB,
        "Comma" => 0xBC,
        "Minus" => 0xBD,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "BracketLeft" => 0xDB,
        "Backslash" => 0xDC,
        "BracketRight" => 0xDD,
        "Quote" => 0xDE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_chord_is_consumed_and_pending_ends_after_release() {
        let mut filter = ShortcutFilter::parse("Ctrl+Shift+Space").unwrap();
        for vk in [0xA3, 0xA0, 0x20] {
            assert!(!filter.event(vk, true));
        }
        assert!(filter.pending());
        for vk in [0xA3, 0x20, 0xA0] {
            assert!(!filter.event(vk, false));
        }
        assert!(!filter.pending());
        assert!(!filter.event(0xA2, true));
        assert!(filter.event(0xA2, false));
        assert!(filter.event(0x20, true));
    }

    #[test]
    fn unrelated_keys_extra_modifiers_and_bare_alt_remain_activity() {
        let mut filter = ShortcutFilter::parse("Ctrl+Shift+Space").unwrap();
        filter.event(0xA2, true);
        filter.event(0xA0, true);
        assert!(filter.event(0x41, true));
        assert!(filter.event(0xA4, true));
        assert!(filter.event(0x20, true));
        let mut alt = ShortcutFilter::parse("Alt+Space").unwrap();
        assert!(!alt.event(0xA4, true));
        assert!(alt.event(0xA4, false));
    }

    #[test]
    fn initial_modifier_releases_require_known_start_chord_origin() {
        for from_shortcut in [false, true] {
            let mut filter = ShortcutFilter::parse("Ctrl+Shift+Space").unwrap();
            filter.initialize(from_shortcut, |vk| vk == 0xA2);
            assert_eq!(filter.event(0xA2, false), !from_shortcut);
            assert!(!filter.pending());
            filter.event(0xA2, true);
            assert!(filter.event(0xA2, false));
        }
        let mut filter = ShortcutFilter::parse("Ctrl+Shift+Space").unwrap();
        filter.initialize(true, |_| false);
        assert!(!filter.event(0xA2, true));
        assert!(filter.event(0xA2, false));
        filter.initialize(true, |vk| vk == 0xA2);
        assert!(!filter.event(0xA0, true));
        assert!(filter.event(0xA0, false));
    }
}
