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

    // --- ASCII letter range -------------------------------------------------

    #[test]
    fn full_ascii_letter_range() {
        assert_eq!(vk("A"), Some(0x41));
        assert_eq!(vk("B"), Some(0x42));
        assert_eq!(vk("M"), Some(0x4D));
        assert_eq!(vk("z"), Some(0x5A));
        // Lowercase maps to the same VK code as uppercase.
        assert_eq!(vk("a"), vk("A"));
        assert_eq!(vk("q"), Some(0x51));
    }

    #[test]
    fn full_ascii_digit_range() {
        for d in 0..=9u32 {
            let name = d.to_string();
            assert_eq!(vk(&name), Some(0x30 + d as u16), "digit {}", name);
        }
    }

    #[test]
    fn single_non_alphanumeric_returns_none() {
        // Punctuation / symbols are not mapped.
        assert_eq!(vk("!"), None);
        assert_eq!(vk("@"), None);
        assert_eq!(vk("-"), None);
        assert_eq!(vk("+"), None);
        assert_eq!(vk("/"), None);
    }

    // --- Whitespace / empty handling ---------------------------------------

    #[test]
    fn empty_string_returns_none() {
        assert_eq!(vk(""), None);
    }

    #[test]
    fn whitespace_only_returns_none() {
        assert_eq!(vk("   "), None);
        assert_eq!(vk("\t"), None);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(vk("  space  "), Some(0x20));
        assert_eq!(vk("\tA\t"), Some(0x41));
        assert_eq!(vk(" F1 "), Some(0x70));
    }

    // --- Case-insensitivity -------------------------------------------------

    #[test]
    fn case_insensitive_named_keys() {
        assert_eq!(vk("SPACE"), Some(0x20));
        assert_eq!(vk("space"), Some(0x20));
        assert_eq!(vk("SpAcE"), Some(0x20));
        assert_eq!(vk("ENTER"), Some(0x0D));
        assert_eq!(vk("enter"), Some(0x0D));
        assert_eq!(vk("LSHIFT"), Some(0xA0));
        assert_eq!(vk("lshift"), Some(0xA0));
    }

    // --- Function keys F1-F24 ----------------------------------------------

    #[test]
    fn function_key_range() {
        assert_eq!(vk("F1"), Some(0x70));
        assert_eq!(vk("F12"), Some(0x7B));
        assert_eq!(vk("F24"), Some(0x87));
        // Lowercase f prefix also works.
        assert_eq!(vk("f1"), Some(0x70));
        assert_eq!(vk("f24"), Some(0x87));
    }

    #[test]
    fn function_key_out_of_range_returns_none() {
        assert_eq!(vk("F0"), None);
        assert_eq!(vk("F25"), None);
        assert_eq!(vk("F100"), None);
        // Non-numeric suffix after F prefix.
        assert_eq!(vk("Fxx"), None);
    }

    #[test]
    fn f_prefix_only_treated_as_function_key_when_followed_by_digits() {
        // "F" alone is a single letter -> letter F.
        assert_eq!(vk("F"), Some(0x46));
        // "Faux" is not a function key; falls through to named-key lookup -> None.
        assert_eq!(vk("Faux"), None);
    }

    // --- Named keys: full alias coverage -----------------------------------

    #[test]
    fn space_aliases() {
        assert_eq!(vk("space"), Some(0x20));
        assert_eq!(vk("spc"), Some(0x20));
    }

    #[test]
    fn enter_aliases() {
        assert_eq!(vk("return"), Some(0x0D));
        assert_eq!(vk("enter"), Some(0x0D));
    }

    #[test]
    fn back_and_esc() {
        assert_eq!(vk("back"), Some(0x08));
        assert_eq!(vk("backspace"), Some(0x08));
        assert_eq!(vk("esc"), Some(0x1B));
        assert_eq!(vk("escape"), Some(0x1B));
    }

    #[test]
    fn tab_pause_caps() {
        assert_eq!(vk("tab"), Some(0x09));
        assert_eq!(vk("pause"), Some(0x13));
        assert_eq!(vk("caps"), Some(0x14));
        assert_eq!(vk("capslock"), Some(0x14));
        assert_eq!(vk("caps_lock"), Some(0x14));
    }

    #[test]
    fn lock_keys() {
        assert_eq!(vk("numlock"), Some(0x90));
        assert_eq!(vk("scroll"), Some(0x91));
        assert_eq!(vk("scrolllock"), Some(0x91));
    }

    #[test]
    fn print_screen_aliases() {
        assert_eq!(vk("print"), Some(0x2C));
        assert_eq!(vk("printscreen"), Some(0x2C));
        assert_eq!(vk("prtsc"), Some(0x2C));
        assert_eq!(vk("snapshot"), Some(0x2C));
    }

    #[test]
    fn insert_delete_home_end() {
        assert_eq!(vk("insert"), Some(0x2D));
        assert_eq!(vk("ins"), Some(0x2D));
        assert_eq!(vk("delete"), Some(0x2E));
        assert_eq!(vk("del"), Some(0x2E));
        assert_eq!(vk("home"), Some(0x24));
        assert_eq!(vk("end"), Some(0x23));
    }

    #[test]
    fn page_up_down_aliases() {
        assert_eq!(vk("pgup"), Some(0x21));
        assert_eq!(vk("pageup"), Some(0x21));
        assert_eq!(vk("pgdn"), Some(0x22));
        assert_eq!(vk("pagedown"), Some(0x22));
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(vk("up"), Some(0x26));
        assert_eq!(vk("down"), Some(0x28));
        assert_eq!(vk("left"), Some(0x25));
        assert_eq!(vk("right"), Some(0x27));
    }

    #[test]
    fn shift_modifiers() {
        assert_eq!(vk("shift"), Some(0xA0));
        assert_eq!(vk("lshift"), Some(0xA0));
        assert_eq!(vk("leftshift"), Some(0xA0));
        assert_eq!(vk("left_shift"), Some(0xA0));
        assert_eq!(vk("rshift"), Some(0xA1));
        assert_eq!(vk("rightshift"), Some(0xA1));
        assert_eq!(vk("right_shift"), Some(0xA1));
    }

    #[test]
    fn ctrl_modifiers() {
        assert_eq!(vk("ctrl"), Some(0xA2));
        assert_eq!(vk("lctrl"), Some(0xA2));
        assert_eq!(vk("leftctrl"), Some(0xA2));
        assert_eq!(vk("control"), Some(0xA2));
        assert_eq!(vk("lcontrol"), Some(0xA2));
        assert_eq!(vk("left_control"), Some(0xA2));
        assert_eq!(vk("rctrl"), Some(0xA3));
        assert_eq!(vk("rightctrl"), Some(0xA3));
        assert_eq!(vk("rcontrol"), Some(0xA3));
        assert_eq!(vk("right_control"), Some(0xA3));
    }

    #[test]
    fn alt_modifiers() {
        assert_eq!(vk("alt"), Some(0xA4));
        assert_eq!(vk("lalt"), Some(0xA4));
        assert_eq!(vk("leftalt"), Some(0xA4));
        assert_eq!(vk("ralt"), Some(0xA5));
        assert_eq!(vk("rightalt"), Some(0xA5));
    }

    #[test]
    fn win_and_apps_keys() {
        assert_eq!(vk("win"), Some(0x5B));
        assert_eq!(vk("lwin"), Some(0x5B));
        assert_eq!(vk("leftwin"), Some(0x5B));
        assert_eq!(vk("rwin"), Some(0x5C));
        assert_eq!(vk("rightwin"), Some(0x5C));
        assert_eq!(vk("apps"), Some(0x5D));
        assert_eq!(vk("menu"), Some(0x5D));
    }

    #[test]
    fn mouse_button_aliases() {
        assert_eq!(vk("lmb"), Some(0x01));
        assert_eq!(vk("mouse1"), Some(0x01));
        assert_eq!(vk("leftbutton"), Some(0x01));
        assert_eq!(vk("rmb"), Some(0x02));
        assert_eq!(vk("mouse2"), Some(0x02));
        assert_eq!(vk("rightbutton"), Some(0x02));
        assert_eq!(vk("mmb"), Some(0x04));
        assert_eq!(vk("mouse3"), Some(0x04));
        assert_eq!(vk("middlebutton"), Some(0x04));
        assert_eq!(vk("x1"), Some(0x05));
        assert_eq!(vk("mouse4"), Some(0x05));
        assert_eq!(vk("x2"), Some(0x06));
        assert_eq!(vk("mouse5"), Some(0x06));
    }

    // --- Unknown / edge cases ----------------------------------------------

    #[test]
    fn unknown_named_keys_return_none() {
        assert_eq!(vk("foo"), None);
        assert_eq!(vk("qwerty"), None);
        assert_eq!(vk("numpad"), None);
    }

    #[test]
    fn single_letter_takes_precedence_over_f_prefix_check() {
        // Single char "f" is a letter, not a function key.
        assert_eq!(vk("f"), Some(0x46));
    }
}
