//! Key and mouse report rows shared by the child-facing encoders here and the
//! host input parser in `shepr-termio`, so both read the same spellings.

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

// Only keys emitted by shepr belong here. Host-only kitty keys stay in the
// parser; accepting them does not promise that pane encoding supports them.
pub struct FunctionalKey {
    pub code: KeyCode,
    pub number: u8,
    pub final_byte: char,
    pub legacy: &'static str,
    pub aliases: &'static [&'static str],
    pub kitty_codepoint: u32,
}

// Expand the same rows into a direct key dispatch and the reverse lookup data.
// Encoding runs per keypress and does not scan the table.
macro_rules! functional_keys {
    ($( $key:pat => ($code:expr, $number:expr, $final:expr, $legacy:expr, $aliases:expr, $kitty:expr), )*) => {
        pub static FUNCTIONAL_KEYS: &[FunctionalKey] = &[
            $(FunctionalKey {
                code: $code, number: $number, final_byte: $final,
                legacy: $legacy, aliases: $aliases, kitty_codepoint: $kitty,
            },)*
        ];
        pub fn functional_key(code: KeyCode) -> Option<FunctionalKey> {
            match code {
                $($key => Some(FunctionalKey {
                    code: $code, number: $number, final_byte: $final,
                    legacy: $legacy, aliases: $aliases, kitty_codepoint: $kitty,
                }),)*
                _ => None,
            }
        }
    };
}

functional_keys! {
    KeyCode::Up => (KeyCode::Up, 1, 'A', "\x1b[A", &["\x1bOA"], 57419),
    KeyCode::Down => (KeyCode::Down, 1, 'B', "\x1b[B", &["\x1bOB"], 57420),
    KeyCode::Right => (KeyCode::Right, 1, 'C', "\x1b[C", &["\x1bOC"], 57418),
    KeyCode::Left => (KeyCode::Left, 1, 'D', "\x1b[D", &["\x1bOD"], 57417),
    KeyCode::Home => (KeyCode::Home, 1, 'H', "\x1b[H", &["\x1bOH", "\x1b[1~", "\x1b[7~"], 57423),
    KeyCode::End => (KeyCode::End, 1, 'F', "\x1b[F", &["\x1bOF", "\x1b[4~", "\x1b[8~"], 57424),
    KeyCode::Insert => (KeyCode::Insert, 2, '~', "\x1b[2~", &[], 57425),
    KeyCode::Delete => (KeyCode::Delete, 3, '~', "\x1b[3~", &[], 57426),
    KeyCode::PageUp => (KeyCode::PageUp, 5, '~', "\x1b[5~", &[], 57421),
    KeyCode::PageDown => (KeyCode::PageDown, 6, '~', "\x1b[6~", &[], 57422),
    KeyCode::F(1) => (KeyCode::F(1), 1, 'P', "\x1bOP", &["\x1b[11~"], 57364),
    KeyCode::F(2) => (KeyCode::F(2), 1, 'Q', "\x1bOQ", &["\x1b[12~"], 57365),
    KeyCode::F(3) => (KeyCode::F(3), 1, 'R', "\x1bOR", &["\x1b[13~"], 57366),
    KeyCode::F(4) => (KeyCode::F(4), 1, 'S', "\x1bOS", &["\x1b[14~"], 57367),
    KeyCode::F(5) => (KeyCode::F(5), 15, '~', "\x1b[15~", &[], 57368),
    KeyCode::F(6) => (KeyCode::F(6), 17, '~', "\x1b[17~", &[], 57369),
    KeyCode::F(7) => (KeyCode::F(7), 18, '~', "\x1b[18~", &[], 57370),
    KeyCode::F(8) => (KeyCode::F(8), 19, '~', "\x1b[19~", &[], 57371),
    KeyCode::F(9) => (KeyCode::F(9), 20, '~', "\x1b[20~", &[], 57372),
    KeyCode::F(10) => (KeyCode::F(10), 21, '~', "\x1b[21~", &[], 57373),
    KeyCode::F(11) => (KeyCode::F(11), 23, '~', "\x1b[23~", &[], 57374),
    KeyCode::F(12) => (KeyCode::F(12), 24, '~', "\x1b[24~", &[], 57375),
}

pub fn modified_key(number: &str, final_byte: char) -> Option<KeyCode> {
    FUNCTIONAL_KEYS.iter().find_map(|key| {
        let matches = if final_byte == '~' {
            key.legacy
                .strip_prefix("\x1b[")
                .and_then(|s| s.strip_suffix('~'))
                == Some(number)
                || key.aliases.iter().any(|s| {
                    s.strip_prefix("\x1b[").and_then(|s| s.strip_suffix('~')) == Some(number)
                }) && matches!(key.code, KeyCode::F(_))
        } else {
            number == "1" && key.final_byte == final_byte
        };
        matches.then_some(key.code)
    })
}

const MODIFIER_BITS: [(KeyModifiers, u8); 6] = [
    (KeyModifiers::SHIFT, 1),
    (KeyModifiers::ALT, 2),
    (KeyModifiers::CONTROL, 4),
    (KeyModifiers::SUPER, 8),
    (KeyModifiers::HYPER, 16),
    (KeyModifiers::META, 32),
];

pub fn modifier_bits(mods: KeyModifiers, kitty: bool) -> u32 {
    MODIFIER_BITS[..if kitty { 6 } else { 3 }]
        .iter()
        .fold(0, |bits, &(flag, bit)| {
            bits | if mods.contains(flag) {
                u32::from(bit)
            } else {
                0
            }
        })
}

// Lock-state bits are intentionally dropped for agent and shell panes.
pub fn modifiers_from_bits(bits: u8) -> KeyModifiers {
    MODIFIER_BITS
        .iter()
        .fold(KeyModifiers::empty(), |mods, &(flag, bit)| {
            mods | if bits & bit != 0 {
                flag
            } else {
                KeyModifiers::empty()
            }
        })
}

// Only mouse forms both the parser and the encoder use belong in these tables.
// The parser's extended buttons (8 and up) and their drags stay in
// `parse_mouse_cb`: crossterm has no value for them, so the encoder never
// produces them and there is nothing to share.
const MOUSE_BUTTONS: [(MouseButton, u8); 3] = [
    (MouseButton::Left, 0),
    (MouseButton::Middle, 1),
    (MouseButton::Right, 2),
];

// The values are decoded button numbers; their low two bits are stored with the 0x40 scroll bit.
const MOUSE_SCROLLS: [(MouseEventKind, u8); 4] = [
    (MouseEventKind::ScrollUp, 4),
    (MouseEventKind::ScrollDown, 5),
    (MouseEventKind::ScrollLeft, 6),
    (MouseEventKind::ScrollRight, 7),
];

// The xterm mouse report control byte: button field, modifier bits, drag
// (motion) bit and the scroll and extended-button high bits.
pub const MOUSE_BUTTON_RELEASE: u8 = 3; // limits-exempt: xterm mouse report encoding
pub const MOUSE_DRAG_OFFSET: u16 = 32; // limits-exempt: xterm mouse report encoding
pub const MOUSE_DRAG_BIT: u8 = 0b0010_0000; // limits-exempt: xterm mouse report encoding
pub const MOUSE_SCROLL_BASE: u8 = 0b0100_0000; // limits-exempt: xterm mouse report encoding
pub const MOUSE_BUTTON_FIELD_MASK: u8 = 0b0000_0011; // limits-exempt: xterm mouse report encoding
pub const MOUSE_EXTENDED_BUTTON_FIELD_MASK: u8 = 0b1100_0000; // limits-exempt: xterm mouse report encoding
pub const MOUSE_EXTENDED_BUTTON_SHIFT: u32 = 4; // limits-exempt: xterm mouse report encoding
pub const MOUSE_MODIFIER_SHIFT: u32 = 2; // limits-exempt: xterm mouse report encoding

pub fn mouse_button_code(button: MouseButton) -> Option<u16> {
    MOUSE_BUTTONS
        .iter()
        .find_map(|(known_button, code)| (*known_button == button).then_some(u16::from(*code)))
}

pub fn mouse_button_from_code(code: u8) -> Option<MouseButton> {
    MOUSE_BUTTONS
        .iter()
        .find_map(|(button, known_code)| (*known_code == code).then_some(*button))
}

pub fn mouse_scroll_code(kind: MouseEventKind) -> Option<u16> {
    MOUSE_SCROLLS.iter().find_map(|(known_kind, code)| {
        (*known_kind == kind).then_some(u16::from(
            MOUSE_SCROLL_BASE | (*code & MOUSE_BUTTON_FIELD_MASK),
        ))
    })
}

pub fn mouse_scroll_from_code(code: u8) -> Option<MouseEventKind> {
    MOUSE_SCROLLS
        .iter()
        .find_map(|(kind, known_code)| (*known_code == code).then_some(*kind))
}

pub fn mouse_modifier_bits(modifiers: KeyModifiers) -> u16 {
    // Only the three xterm modifier bits are taken, so the shifted value is
    // at most 28 and the conversion cannot fail.
    u16::try_from(modifier_bits(modifiers, false) << MOUSE_MODIFIER_SHIFT).unwrap_or_default()
}

pub fn mouse_modifiers_from_bits(control_byte: u8) -> KeyModifiers {
    modifiers_from_bits((control_byte >> MOUSE_MODIFIER_SHIFT) & 0b0000_0111)
}

// Control bytes are many-to-one. The first spelling is the parser's canonical
// character; aliases are accepted only by the encoder. Enter, Tab, Escape and
// DEL take precedence over these identities in legacy input parsing.
const CONTROL_CHARS: [(u8, &str); 7] = [
    (0, " @2"),
    (27, "[3"),
    (28, "\\4"),
    (29, "]5"),
    (30, "^6"),
    (31, "_/7-"),
    (127, "?8"),
];

pub fn control_byte(ch: char) -> Option<u8> {
    let upper = ch.to_ascii_uppercase();
    if upper.is_ascii_uppercase() {
        return Some(upper as u8 - b'A' + 1);
    }
    CONTROL_CHARS
        .iter()
        .find_map(|&(byte, aliases)| aliases.contains(ch).then_some(byte))
}

pub fn control_char(byte: u32) -> Option<char> {
    if (1..=26).contains(&byte) {
        return char::from_u32(byte + 96);
    }
    CONTROL_CHARS.iter().find_map(|&(value, aliases)| {
        (byte == u32::from(value) && byte != 127)
            .then(|| aliases.chars().next())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KittyKeyboardFlags;
    use crate::key::{KeyboardProtocol, TerminalKey, encode_terminal_key};

    #[test]
    fn modifier_bits_roundtrip_and_xterm_filters_extended_bits() {
        for bits in 0..=255 {
            let mods = modifiers_from_bits(bits);
            assert_eq!(modifier_bits(mods, true), u32::from(bits & 63));
            assert_eq!(modifier_bits(mods, false), u32::from(bits & 7));
        }
    }

    #[test]
    fn control_aliases_encode_and_canonical_forms_decode() {
        for &(byte, aliases) in &CONTROL_CHARS {
            for ch in aliases.chars() {
                assert_eq!(control_byte(ch), Some(byte));
            }
            if byte != 127 {
                assert_eq!(control_char(u32::from(byte)), aliases.chars().next());
            }
        }
        for ch in 'a'..='z' {
            let byte = control_byte(ch).expect("control letter");
            assert_eq!(control_char(u32::from(byte)), Some(ch));
            assert_eq!(control_byte(ch.to_ascii_uppercase()), Some(byte));
        }
    }

    #[test]
    fn unsupported_encoder_keys_stay_unsupported() {
        for code in [KeyCode::F(13), KeyCode::CapsLock] {
            assert!(functional_key(code).is_none());
            assert!(
                encode_terminal_key(
                    TerminalKey::new(code, KeyModifiers::empty()),
                    KeyboardProtocol::from_flags(KittyKeyboardFlags::REPORT_ALL_KEYS)
                )
                .is_empty()
            );
        }
        assert!(modified_key("1", '~').is_none());
        assert!(modified_key("7", '~').is_none());
    }
}
