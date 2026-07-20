//! Simple name-to-Windows-virtual-key-code helper.
//!
//! The parser is intentionally lenient: it accepts common gaming/action labels
//! ("W", "Space", "LShift", "ctrl", etc.) and returns the matching VK code.
//! Unknown labels return `None` so callers can decide how to handle them.

/// Look up the Windows virtual-key code for a human-readable key name.
///
/// Matching is case-insensitive and trims surrounding whitespace. Single
/// characters are treated as letter/digit keys; longer strings are matched
/// against common aliases.
pub fn vk(name: &str) -> Option<u16> {
    let s = name.trim();
    if s.is_empty() {
        return None;
    }

    // Single ASCII letters or digits.
    if s.len() == 1 {
        let c = s.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return Some((c.to_ascii_uppercase() as u32 - 'A' as u32 + 0x41) as u16);
        }
        if c.is_ascii_digit() {
            return Some((c as u32 - '0' as u32 + 0x30) as u16);
        }
    }

    // Function keys F1-F24.
    if s.len() >= 2 && s.to_ascii_uppercase().starts_with('F') {
        let num_part = &s[1..];
        if let Ok(n) = num_part.parse::<u16>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1);
            }
        }
    }

    Some(match s.to_ascii_lowercase().as_str() {
        "space" | "spc" => 0x20,
        "return" | "enter" => 0x0D,
        "tab" => 0x09,
        "back" | "backspace" => 0x08,
        "esc" | "escape" => 0x1B,
        "pause" => 0x13,
        "caps" | "capslock" | "caps_lock" => 0x14,
        "numlock" => 0x90,
        "scroll" | "scrolllock" => 0x91,
        "print" | "printscreen" | "prtsc" | "snapshot" => 0x2C,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pgup" | "pageup" => 0x21,
        "pgdn" | "pagedown" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "shift" | "lshift" | "leftshift" | "left_shift" => 0xA0,
        "rshift" | "rightshift" | "right_shift" => 0xA1,
        "ctrl" | "lctrl" | "leftctrl" | "control" | "lcontrol" | "left_control" => 0xA2,
        "rctrl" | "rightctrl" | "rcontrol" | "right_control" => 0xA3,
        "alt" | "lalt" | "leftalt" => 0xA4,
        "ralt" | "rightalt" => 0xA5,
        "win" | "lwin" | "leftwin" => 0x5B,
        "rwin" | "rightwin" => 0x5C,
        "apps" | "menu" => 0x5D,
        "lmb" | "mouse1" | "leftbutton" => 0x01,
        "rmb" | "mouse2" | "rightbutton" => 0x02,
        "mmb" | "mouse3" | "middlebutton" => 0x04,
        "x1" | "mouse4" => 0x05,
        "x2" | "mouse5" => 0x06,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits() {
        assert_eq!(vk("a"), Some(0x41));
        assert_eq!(vk("Z"), Some(0x5A));
        assert_eq!(vk("5"), Some(0x35));
    }

    #[test]
    fn named_keys() {
        assert_eq!(vk("Space"), Some(0x20));
        assert_eq!(vk("LShift"), Some(0xA0));
        assert_eq!(vk("ctrl"), Some(0xA2));
        assert_eq!(vk("Left"), Some(0x25));
        assert_eq!(vk("F2"), Some(0x71));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(vk("NotAKey"), None);
    }
}
