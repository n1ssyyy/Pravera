//! HID usage codes to PC scan codes.
//!
//! ## Why scan codes and not virtual keys
//!
//! A remote desktop has two keyboards and two layouts, and only one of them
//! matters. If a French client presses the key where a US keyboard has `Q`,
//! what should reach a US host? The answer that surprises nobody is: whatever
//! that *physical key position* produces on the host, exactly as if the person
//! were sitting at it.
//!
//! Scan codes describe key positions. Virtual keys describe meanings, and a
//! meaning has already had a layout applied to it — so sending virtual keys
//! applies the client's layout, then lets the host apply its own on top, and
//! the result is a keyboard that types different letters than the ones pressed.
//!
//! HID usages are what the wire carries because they are what USB keyboards
//! themselves report: positions, before any layout. This table is the last
//! step, turning a position into the number the platform recognises for it.
//!
//! Typing a specific *character* regardless of layout is a different problem
//! with a different answer — `InputEvent::Text`, which goes through Unicode
//! injection and never touches this table.

/// A key as the PC keyboard controller reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCode {
    /// The set-1 scan code, without any prefix.
    pub code: u16,
    /// Whether the key is one of the duplicated ones the controller
    /// distinguishes with an `0xE0` prefix — the arrow cluster, the right-hand
    /// modifiers, keypad enter and divide.
    ///
    /// Getting this wrong does not fail: it silently presses the *other* key of
    /// the pair. Right Alt becomes left Alt, Home becomes keypad 7, and the
    /// arrows become the numeric keypad.
    pub extended: bool,
}

impl ScanCode {
    const fn plain(code: u16) -> ScanCode {
        ScanCode {
            code,
            extended: false,
        }
    }

    const fn ext(code: u16) -> ScanCode {
        ScanCode {
            code,
            extended: true,
        }
    }
}

/// The scan code for a HID usage on page 0x07, if there is one.
///
/// `None` for usages this table does not cover — media keys, the power button,
/// international keys with no PC equivalent, and the large unassigned ranges.
/// One unmapped key is one key; the rest of the keyboard keeps working.
pub const fn scan_code(usage: u16) -> Option<ScanCode> {
    Some(match usage {
        // Letters, in HID order, which is alphabetical rather than positional.
        0x04 => ScanCode::plain(0x1E), // A
        0x05 => ScanCode::plain(0x30), // B
        0x06 => ScanCode::plain(0x2E), // C
        0x07 => ScanCode::plain(0x20), // D
        0x08 => ScanCode::plain(0x12), // E
        0x09 => ScanCode::plain(0x21), // F
        0x0A => ScanCode::plain(0x22), // G
        0x0B => ScanCode::plain(0x23), // H
        0x0C => ScanCode::plain(0x17), // I
        0x0D => ScanCode::plain(0x24), // J
        0x0E => ScanCode::plain(0x25), // K
        0x0F => ScanCode::plain(0x26), // L
        0x10 => ScanCode::plain(0x32), // M
        0x11 => ScanCode::plain(0x31), // N
        0x12 => ScanCode::plain(0x18), // O
        0x13 => ScanCode::plain(0x19), // P
        0x14 => ScanCode::plain(0x10), // Q
        0x15 => ScanCode::plain(0x13), // R
        0x16 => ScanCode::plain(0x1F), // S
        0x17 => ScanCode::plain(0x14), // T
        0x18 => ScanCode::plain(0x16), // U
        0x19 => ScanCode::plain(0x2F), // V
        0x1A => ScanCode::plain(0x11), // W
        0x1B => ScanCode::plain(0x2D), // X
        0x1C => ScanCode::plain(0x15), // Y
        0x1D => ScanCode::plain(0x2C), // Z

        // Digit row.
        0x1E => ScanCode::plain(0x02), // 1
        0x1F => ScanCode::plain(0x03), // 2
        0x20 => ScanCode::plain(0x04), // 3
        0x21 => ScanCode::plain(0x05), // 4
        0x22 => ScanCode::plain(0x06), // 5
        0x23 => ScanCode::plain(0x07), // 6
        0x24 => ScanCode::plain(0x08), // 7
        0x25 => ScanCode::plain(0x09), // 8
        0x26 => ScanCode::plain(0x0A), // 9
        0x27 => ScanCode::plain(0x0B), // 0

        0x28 => ScanCode::plain(0x1C), // Enter
        0x29 => ScanCode::plain(0x01), // Escape
        0x2A => ScanCode::plain(0x0E), // Backspace
        0x2B => ScanCode::plain(0x0F), // Tab
        0x2C => ScanCode::plain(0x39), // Space
        0x2D => ScanCode::plain(0x0C), // - _
        0x2E => ScanCode::plain(0x0D), // = +
        0x2F => ScanCode::plain(0x1A), // [ {
        0x30 => ScanCode::plain(0x1B), // ] }
        0x31 => ScanCode::plain(0x2B), // \ |
        // The key ISO keyboards put where ANSI keyboards put backslash. The
        // controller reports the same code for both, which is why this looks
        // like a duplicate and is not one.
        0x32 => ScanCode::plain(0x2B),
        0x33 => ScanCode::plain(0x27), // ; :
        0x34 => ScanCode::plain(0x28), // ' "
        0x35 => ScanCode::plain(0x29), // ` ~
        0x36 => ScanCode::plain(0x33), // , <
        0x37 => ScanCode::plain(0x34), // . >
        0x38 => ScanCode::plain(0x35), // / ?
        0x39 => ScanCode::plain(0x3A), // Caps Lock

        // Function row.
        0x3A => ScanCode::plain(0x3B), // F1
        0x3B => ScanCode::plain(0x3C),
        0x3C => ScanCode::plain(0x3D),
        0x3D => ScanCode::plain(0x3E),
        0x3E => ScanCode::plain(0x3F),
        0x3F => ScanCode::plain(0x40),
        0x40 => ScanCode::plain(0x41),
        0x41 => ScanCode::plain(0x42),
        0x42 => ScanCode::plain(0x43),
        0x43 => ScanCode::plain(0x44), // F10
        0x44 => ScanCode::plain(0x57), // F11
        0x45 => ScanCode::plain(0x58), // F12

        // Print Screen's full make sequence is `E0 2A E0 37`; the `E0 2A` half
        // only cancels a held shift, so `E0 37` alone presses the key.
        0x46 => ScanCode::ext(0x37),
        0x47 => ScanCode::plain(0x46), // Scroll Lock
        // Pause is deliberately absent. Its set-1 sequence is
        // `E1 1D 45 E1 9D C5` — the only key using the `0xE1` prefix, which
        // `ScanCode` cannot express and `SendInput` cannot send. The `45` in
        // the middle is *Num Lock's* code, so mapping Pause to `0x45` here
        // would toggle Num Lock every time someone pressed Pause and look
        // entirely correct while doing it. The Windows backend presses it by
        // virtual key instead; see `win32::VIRTUAL_ONLY`.

        // The navigation cluster. Every one of these shares a code with a
        // keypad key and is told apart only by the prefix.
        0x49 => ScanCode::ext(0x52), // Insert
        0x4A => ScanCode::ext(0x47), // Home
        0x4B => ScanCode::ext(0x49), // Page Up
        0x4C => ScanCode::ext(0x53), // Delete
        0x4D => ScanCode::ext(0x4F), // End
        0x4E => ScanCode::ext(0x51), // Page Down
        0x4F => ScanCode::ext(0x4D), // Right
        0x50 => ScanCode::ext(0x4B), // Left
        0x51 => ScanCode::ext(0x50), // Down
        0x52 => ScanCode::ext(0x48), // Up

        // Keypad.
        0x53 => ScanCode::plain(0x45), // Num Lock
        0x54 => ScanCode::ext(0x35),   // KP /
        0x55 => ScanCode::plain(0x37), // KP *
        0x56 => ScanCode::plain(0x4A), // KP -
        0x57 => ScanCode::plain(0x4E), // KP +
        0x58 => ScanCode::ext(0x1C),   // KP Enter
        0x59 => ScanCode::plain(0x4F), // KP 1
        0x5A => ScanCode::plain(0x50), // KP 2
        0x5B => ScanCode::plain(0x51), // KP 3
        0x5C => ScanCode::plain(0x4B), // KP 4
        0x5D => ScanCode::plain(0x4C), // KP 5
        0x5E => ScanCode::plain(0x4D), // KP 6
        0x5F => ScanCode::plain(0x47), // KP 7
        0x60 => ScanCode::plain(0x48), // KP 8
        0x61 => ScanCode::plain(0x49), // KP 9
        0x62 => ScanCode::plain(0x52), // KP 0
        0x63 => ScanCode::plain(0x53), // KP .

        0x64 => ScanCode::plain(0x56), // ISO extra key, left of Z
        0x65 => ScanCode::ext(0x5D),   // Application / context menu
        0x67 => ScanCode::plain(0x59), // KP =

        // F13 upwards. Rare on hardware, common on remapped keyboards.
        0x68 => ScanCode::plain(0x64),
        0x69 => ScanCode::plain(0x65),
        0x6A => ScanCode::plain(0x66),
        0x6B => ScanCode::plain(0x67),
        0x6C => ScanCode::plain(0x68),
        0x6D => ScanCode::plain(0x69),
        0x6E => ScanCode::plain(0x6A),
        0x6F => ScanCode::plain(0x6B),
        0x70 => ScanCode::plain(0x6C),
        0x71 => ScanCode::plain(0x6D),
        0x72 => ScanCode::plain(0x6E),
        0x73 => ScanCode::plain(0x76), // F24

        // Modifiers. The right-hand ones are the prefixed twins of the left.
        0xE0 => ScanCode::plain(0x1D), // Left Ctrl
        0xE1 => ScanCode::plain(0x2A), // Left Shift
        0xE2 => ScanCode::plain(0x38), // Left Alt
        0xE3 => ScanCode::ext(0x5B),   // Left GUI
        0xE4 => ScanCode::ext(0x1D),   // Right Ctrl
        0xE5 => ScanCode::plain(0x36), // Right Shift
        0xE6 => ScanCode::ext(0x38),   // Right Alt / AltGr
        0xE7 => ScanCode::ext(0x5C),   // Right GUI

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_letters_land_where_a_us_keyboard_puts_them() {
        // HID orders letters alphabetically; the keyboard does not. A table
        // built by counting rather than looking things up gets this wrong.
        assert_eq!(scan_code(0x04), Some(ScanCode::plain(0x1E)), "A");
        assert_eq!(scan_code(0x14), Some(ScanCode::plain(0x10)), "Q");
        assert_eq!(scan_code(0x1A), Some(ScanCode::plain(0x11)), "W");
        assert_eq!(scan_code(0x1D), Some(ScanCode::plain(0x2C)), "Z");
    }

    #[test]
    fn the_arrow_keys_are_extended_and_the_keypad_digits_are_not() {
        // These share codes. Without the prefix the arrows type numbers, which
        // is the single most noticeable way this table can be wrong.
        assert_eq!(scan_code(0x52), Some(ScanCode::ext(0x48)), "Up");
        assert_eq!(scan_code(0x60), Some(ScanCode::plain(0x48)), "KP 8");

        assert_eq!(scan_code(0x50), Some(ScanCode::ext(0x4B)), "Left");
        assert_eq!(scan_code(0x5C), Some(ScanCode::plain(0x4B)), "KP 4");

        assert_eq!(scan_code(0x4C), Some(ScanCode::ext(0x53)), "Delete");
        assert_eq!(scan_code(0x63), Some(ScanCode::plain(0x53)), "KP .");
    }

    #[test]
    fn the_right_hand_modifiers_are_not_the_left_hand_ones() {
        // AltGr is right Alt. Sending left Alt instead breaks every accented
        // character on a European layout, and does it silently.
        assert_eq!(scan_code(0xE2), Some(ScanCode::plain(0x38)), "Left Alt");
        assert_eq!(scan_code(0xE6), Some(ScanCode::ext(0x38)), "Right Alt");

        assert_eq!(scan_code(0xE0), Some(ScanCode::plain(0x1D)), "Left Ctrl");
        assert_eq!(scan_code(0xE4), Some(ScanCode::ext(0x1D)), "Right Ctrl");

        // Shift is the exception: the two halves have genuinely different
        // codes and neither is prefixed.
        assert_eq!(scan_code(0xE1), Some(ScanCode::plain(0x2A)), "Left Shift");
        assert_eq!(scan_code(0xE5), Some(ScanCode::plain(0x36)), "Right Shift");
    }

    #[test]
    fn keypad_enter_is_told_apart_from_the_main_enter() {
        assert_eq!(scan_code(0x28), Some(ScanCode::plain(0x1C)), "Enter");
        assert_eq!(scan_code(0x58), Some(ScanCode::ext(0x1C)), "KP Enter");
    }

    #[test]
    fn every_key_a_full_size_keyboard_has_is_mapped() {
        // The contiguous run from A to the context-menu key covers a 104-key
        // board. Pause (0x48) is the one deliberate hole: it has no single
        // set-1 code, and the backend presses it by virtual key.
        for usage in 0x04..=0x65u16 {
            if usage == 0x48 {
                continue;
            }
            assert!(
                scan_code(usage).is_some(),
                "HID usage {usage:#04x} has no scan code"
            );
        }
        for usage in 0xE0..=0xE7u16 {
            assert!(
                scan_code(usage).is_some(),
                "modifier {usage:#04x} is missing"
            );
        }
    }

    #[test]
    fn usages_outside_the_table_are_reported_rather_than_guessed() {
        assert_eq!(scan_code(0x00), None, "reserved");
        assert_eq!(scan_code(0x66), None, "power");
        assert_eq!(scan_code(0xFFFF), None);
        // Media keys live on a different usage page entirely.
        assert_eq!(scan_code(0x00E9), None);
    }

    #[test]
    fn pause_does_not_secretly_toggle_num_lock() {
        // The bug this prevents is invisible from the client: press Pause,
        // watch the host's Num Lock light change, and nothing anywhere says
        // the wrong key was sent.
        assert_eq!(scan_code(0x48), None, "Pause");
        assert_eq!(scan_code(0x53), Some(ScanCode::plain(0x45)), "Num Lock");
    }

    #[test]
    fn no_two_positions_claim_the_same_code_and_prefix() {
        // Two usages mapping to one physical key means one of them is a typo
        // and presses the wrong key forever. The ISO/ANSI backslash pair is
        // the one real exception: the controller genuinely reports one code
        // for both positions.
        let mut seen: Vec<(u16, ScanCode)> = Vec::new();
        for usage in 0x00..=0xFFu16 {
            let Some(code) = scan_code(usage) else {
                continue;
            };
            if let Some((other, _)) = seen.iter().find(|(_, c)| *c == code) {
                let backslash_pair = matches!((*other, usage), (0x31, 0x32));
                assert!(
                    backslash_pair,
                    "{usage:#04x} and {other:#04x} both map to {code:?}"
                );
            }
            seen.push((usage, code));
        }
    }
}
