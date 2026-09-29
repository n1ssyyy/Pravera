//! Turning what the window system reports into what the wire carries.
//!
//! iced names keys by *position* — `KeyA` is the key where a US keyboard has
//! `A`, whatever the local layout prints on it — and the protocol carries HID
//! usages, which mean exactly the same thing. So this is a rename, not a
//! translation, and that is the whole point: the person's layout is applied
//! once, on the host, by the host.
//!
//! What is *not* here is any attempt to work out which character a key press
//! should produce. Trying to do that at this end is how a remote desktop ends
//! up typing `qwerty` when someone types `azerty`.
//!
//! ## Why the composed text is thrown away
//!
//! iced also reports the character a key press produced under the *local*
//! layout, and it is tempting to forward it. It must not be. A dead key is the
//! case that settles it: pressing `\u{27}` then `e` on a US-International
//! layout produces one press with the text `\u{e9}`, which is
//! indistinguishable from an ordinary letter by anything this side can
//! measure. Forward it and every accented character is typed twice; forward it
//! only "sometimes" and the rule is a guess. Send the two key positions
//! instead and the host's own layout composes exactly the same character,
//! which is the arrangement the whole design rests on.
//!
//! The one thing this genuinely cannot carry is an input method — Japanese,
//! Chinese, Korean — where the committed text stands for no key positions at
//! all. `InputEvent::Text` exists on the wire for that, and nothing produces
//! it yet: the shader widget the session is drawn on declares no input-method
//! support, so the platform never opens an IME over it. Making that work is
//! its own piece of work, not a line in this table.

use iced::keyboard::key::{Code, Physical};
use pravera_proto::KeyCode;

/// The HID usage for a physical key, if the protocol has one.
///
/// `None` for keys with no usage on page 0x07 — media keys, browser keys, the
/// power button. One unmapped key is one key; nothing else stops working.
pub fn usage(physical: Physical) -> Option<KeyCode> {
    let Physical::Code(code) = physical else {
        return None;
    };
    Some(KeyCode(match code {
        Code::KeyA => 0x04,
        Code::KeyB => 0x05,
        Code::KeyC => 0x06,
        Code::KeyD => 0x07,
        Code::KeyE => 0x08,
        Code::KeyF => 0x09,
        Code::KeyG => 0x0A,
        Code::KeyH => 0x0B,
        Code::KeyI => 0x0C,
        Code::KeyJ => 0x0D,
        Code::KeyK => 0x0E,
        Code::KeyL => 0x0F,
        Code::KeyM => 0x10,
        Code::KeyN => 0x11,
        Code::KeyO => 0x12,
        Code::KeyP => 0x13,
        Code::KeyQ => 0x14,
        Code::KeyR => 0x15,
        Code::KeyS => 0x16,
        Code::KeyT => 0x17,
        Code::KeyU => 0x18,
        Code::KeyV => 0x19,
        Code::KeyW => 0x1A,
        Code::KeyX => 0x1B,
        Code::KeyY => 0x1C,
        Code::KeyZ => 0x1D,

        Code::Digit1 => 0x1E,
        Code::Digit2 => 0x1F,
        Code::Digit3 => 0x20,
        Code::Digit4 => 0x21,
        Code::Digit5 => 0x22,
        Code::Digit6 => 0x23,
        Code::Digit7 => 0x24,
        Code::Digit8 => 0x25,
        Code::Digit9 => 0x26,
        Code::Digit0 => 0x27,

        Code::Enter => 0x28,
        Code::Escape => 0x29,
        Code::Backspace => 0x2A,
        Code::Tab => 0x2B,
        Code::Space => 0x2C,
        Code::Minus => 0x2D,
        Code::Equal => 0x2E,
        Code::BracketLeft => 0x2F,
        Code::BracketRight => 0x30,
        Code::Backslash => 0x31,
        Code::Semicolon => 0x33,
        Code::Quote => 0x34,
        Code::Backquote => 0x35,
        Code::Comma => 0x36,
        Code::Period => 0x37,
        Code::Slash => 0x38,
        Code::CapsLock => 0x39,

        Code::F1 => 0x3A,
        Code::F2 => 0x3B,
        Code::F3 => 0x3C,
        Code::F4 => 0x3D,
        Code::F5 => 0x3E,
        Code::F6 => 0x3F,
        Code::F7 => 0x40,
        Code::F8 => 0x41,
        Code::F9 => 0x42,
        Code::F10 => 0x43,
        Code::F11 => 0x44,
        Code::F12 => 0x45,

        Code::PrintScreen => 0x46,
        Code::ScrollLock => 0x47,
        Code::Pause => 0x48,
        Code::Insert => 0x49,
        Code::Home => 0x4A,
        Code::PageUp => 0x4B,
        Code::Delete => 0x4C,
        Code::End => 0x4D,
        Code::PageDown => 0x4E,
        Code::ArrowRight => 0x4F,
        Code::ArrowLeft => 0x50,
        Code::ArrowDown => 0x51,
        Code::ArrowUp => 0x52,

        Code::NumLock => 0x53,
        Code::NumpadDivide => 0x54,
        Code::NumpadMultiply => 0x55,
        Code::NumpadSubtract => 0x56,
        Code::NumpadAdd => 0x57,
        Code::NumpadEnter => 0x58,
        Code::Numpad1 => 0x59,
        Code::Numpad2 => 0x5A,
        Code::Numpad3 => 0x5B,
        Code::Numpad4 => 0x5C,
        Code::Numpad5 => 0x5D,
        Code::Numpad6 => 0x5E,
        Code::Numpad7 => 0x5F,
        Code::Numpad8 => 0x60,
        Code::Numpad9 => 0x61,
        Code::Numpad0 => 0x62,
        Code::NumpadDecimal => 0x63,

        // The key ISO keyboards add beside the left shift. Note that it is
        // *not* `Backslash`: `IntlBackslash` is a different physical position
        // that happens to be printed the same on some layouts.
        Code::IntlBackslash => 0x64,
        Code::ContextMenu => 0x65,
        Code::NumpadEqual => 0x67,

        Code::F13 => 0x68,
        Code::F14 => 0x69,
        Code::F15 => 0x6A,
        Code::F16 => 0x6B,
        Code::F17 => 0x6C,
        Code::F18 => 0x6D,
        Code::F19 => 0x6E,
        Code::F20 => 0x6F,
        Code::F21 => 0x70,
        Code::F22 => 0x71,
        Code::F23 => 0x72,
        Code::F24 => 0x73,

        Code::ControlLeft => 0xE0,
        Code::ShiftLeft => 0xE1,
        Code::AltLeft => 0xE2,
        Code::SuperLeft => 0xE3,
        Code::ControlRight => 0xE4,
        Code::ShiftRight => 0xE5,
        Code::AltRight => 0xE6,
        Code::SuperRight => 0xE7,

        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::NativeCode;

    #[test]
    fn a_key_position_becomes_the_usage_that_names_the_same_position() {
        // The letters are the ones that expose a table built by counting: HID
        // orders them alphabetically and the keyboard does not.
        assert_eq!(usage(Physical::Code(Code::KeyA)), Some(KeyCode(0x04)));
        assert_eq!(usage(Physical::Code(Code::KeyQ)), Some(KeyCode(0x14)));
        assert_eq!(usage(Physical::Code(Code::KeyZ)), Some(KeyCode(0x1D)));
        assert_eq!(usage(Physical::Code(Code::Digit1)), Some(KeyCode(0x1E)));
        // Zero sits after nine, not before one.
        assert_eq!(usage(Physical::Code(Code::Digit0)), Some(KeyCode(0x27)));
    }

    #[test]
    fn the_two_halves_of_a_modifier_are_different_keys() {
        // AltGr is right Alt. Collapsing the pair breaks every accented
        // character on a European layout, silently.
        assert_ne!(
            usage(Physical::Code(Code::AltLeft)),
            usage(Physical::Code(Code::AltRight))
        );
        assert_eq!(usage(Physical::Code(Code::AltRight)), Some(KeyCode(0xE6)));
        assert_ne!(
            usage(Physical::Code(Code::ControlLeft)),
            usage(Physical::Code(Code::ControlRight))
        );
    }

    #[test]
    fn the_arrows_are_not_the_keypad() {
        assert_eq!(usage(Physical::Code(Code::ArrowUp)), Some(KeyCode(0x52)));
        assert_eq!(usage(Physical::Code(Code::Numpad8)), Some(KeyCode(0x60)));
        assert_eq!(usage(Physical::Code(Code::Enter)), Some(KeyCode(0x28)));
        assert_eq!(
            usage(Physical::Code(Code::NumpadEnter)),
            Some(KeyCode(0x58))
        );
    }

    #[test]
    fn the_iso_extra_key_is_not_the_backslash_it_resembles() {
        // Two different physical positions. Mapping them together makes one of
        // them press the other, on every ISO keyboard.
        assert_eq!(usage(Physical::Code(Code::Backslash)), Some(KeyCode(0x31)));
        assert_eq!(
            usage(Physical::Code(Code::IntlBackslash)),
            Some(KeyCode(0x64))
        );
    }

    #[test]
    fn a_key_with_no_usage_is_reported_rather_than_guessed() {
        assert_eq!(usage(Physical::Code(Code::MediaPlayPause)), None);
        assert_eq!(usage(Physical::Code(Code::BrowserBack)), None);
        assert_eq!(usage(Physical::Code(Code::Power)), None);
    }

    #[test]
    fn a_key_the_window_system_could_not_name_is_reported_rather_than_guessed() {
        // `Physical::Unidentified` carries a platform scancode and nothing
        // portable. Mapping it would mean picking a HID usage from a number
        // that means something different on each operating system.
        assert_eq!(
            usage(Physical::Unidentified(NativeCode::Unidentified)),
            None
        );
        assert_eq!(
            usage(Physical::Unidentified(NativeCode::Windows(0x45))),
            None
        );
        assert_eq!(usage(Physical::Unidentified(NativeCode::Xkb(9))), None);
    }

    #[test]
    fn every_key_maps_to_a_usage_the_host_can_actually_press() {
        // The two tables are written independently, and a usage this one emits
        // that `pravera-input` has no scan code for is a key that silently
        // does nothing. Pause is the one known hole, handled by virtual key on
        // the host rather than by scan code.
        for code in [
            Code::KeyA,
            Code::KeyZ,
            Code::Digit0,
            Code::Enter,
            Code::Escape,
            Code::Space,
            Code::Tab,
            Code::F1,
            Code::F12,
            Code::F24,
            Code::ArrowUp,
            Code::Home,
            Code::Delete,
            Code::NumpadEnter,
            Code::NumpadDecimal,
            Code::ContextMenu,
            Code::IntlBackslash,
            Code::AltRight,
            Code::SuperRight,
            Code::PrintScreen,
        ] {
            let usage = usage(Physical::Code(code)).unwrap_or_else(|| panic!("{code:?}"));
            assert!(
                pravera_input::scan_code(usage.0).is_some(),
                "{code:?} maps to usage {:#06x}, which the host cannot press",
                usage.0
            );
        }
    }
}
