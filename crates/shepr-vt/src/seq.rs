//! Shared VT spellings. Display builders write into the caller's existing buffer.
use std::fmt;

use crate::{ColorQueryTarget, RgbColor};

pub const FOCUS_GAINED: &[u8] = b"\x1b[I";
pub const FOCUS_LOST: &[u8] = b"\x1b[O";
pub const COLOR_SCHEME_QUERY: &str = "\x1b[?996n";
pub const COLOR_SCHEME_DARK: &[u8] = b"\x1b[?997;1n";
pub const COLOR_SCHEME_LIGHT: &[u8] = b"\x1b[?997;2n";
pub const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
pub const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy)]
pub enum ColorSlot {
    Foreground,
    Background,
    Underline,
}
#[derive(Clone, Copy)]
pub enum SgrColor {
    Default,
    Named(u8),
    Indexed(u8),
    Rgb(RgbColor),
}

pub struct ColorParam(pub ColorSlot, pub SgrColor);
impl fmt::Display for ColorParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.0 {
            ColorSlot::Foreground => 38,
            ColorSlot::Background => 48,
            ColorSlot::Underline => 58,
        };
        match self.1 {
            SgrColor::Default => write!(f, "{}", prefix + 1),
            SgrColor::Named(index) if index < 16 && !matches!(self.0, ColorSlot::Underline) => {
                let base = match self.0 {
                    ColorSlot::Foreground => 30,
                    _ => 40,
                };
                write!(
                    f,
                    "{}",
                    base + u16::from(index % 8) + if index >= 8 { 60 } else { 0 }
                )
            }
            SgrColor::Named(index) | SgrColor::Indexed(index) => write!(f, "{prefix};5;{index}"),
            SgrColor::Rgb(rgb) => write!(f, "{prefix};2;{};{};{}", rgb.r, rgb.g, rgb.b),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ReplyForm {
    Bel,
    St,
}
impl ReplyForm {
    pub const fn terminator(self) -> &'static str {
        match self {
            Self::Bel => "\x07",
            Self::St => "\x1b\\",
        }
    }
}

pub struct ColorReply(pub ColorQueryTarget, pub RgbColor, pub ReplyForm);
impl fmt::Display for ColorReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\x1b]")?;
        match self.0 {
            ColorQueryTarget::Palette(index) => write!(f, "4;{index}")?,
            ColorQueryTarget::Foreground => f.write_str("10")?,
            ColorQueryTarget::Background => f.write_str("11")?,
            ColorQueryTarget::Cursor => f.write_str("12")?,
        }
        let rgb = self.1;
        write!(
            f,
            ";rgb:{:04x}/{:04x}/{:04x}{}",
            u16::from(rgb.r) * 257,
            u16::from(rgb.g) * 257,
            u16::from(rgb.b) * 257,
            self.2.terminator()
        )
    }
}

pub fn parse_rgb_color(value: &str) -> Option<RgbColor> {
    if let Some(rgb) = value.strip_prefix("rgb:") {
        let mut parts = rgb.split('/');
        let color = RgbColor {
            r: parse_hex_component(parts.next()?)?,
            g: parse_hex_component(parts.next()?)?,
            b: parse_hex_component(parts.next()?)?,
        };
        return if parts.next().is_none() {
            Some(color)
        } else {
            None
        };
    }

    if let Some(hex) = value.strip_prefix('#') {
        if !hex.is_ascii() {
            return None;
        }
        let digits = hex.len() / 3;
        if !matches!(digits, 1..=4) || hex.len() != digits * 3 {
            return None;
        }
        return Some(RgbColor {
            r: parse_hash_component(&hex[..digits])?,
            g: parse_hash_component(&hex[digits..digits * 2])?,
            b: parse_hash_component(&hex[digits * 2..])?,
        });
    }

    None
}

fn parse_hash_component(component: &str) -> Option<u8> {
    if component.is_empty()
        || component.len() > 4
        || !component.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    // XParseColor treats # components as their most significant bits, unlike rgb:.
    let high_byte = (value << (16 - component.len() * 4)) >> 8;
    u8::try_from(high_byte).ok()
}

pub fn parse_hex_component(component: &str) -> Option<u8> {
    if component.is_empty()
        || component.len() > 4
        || !component.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (component.len() * 4)) - 1;
    // Result is a value scaled into 0..=255, so this never truncates.
    Some(u8::try_from((value * 255 + (max / 2)) / max).unwrap_or(u8::MAX))
}

pub struct DecSet(pub crate::DecMode, pub bool);
impl fmt::Display for DecSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\x1b[?{}{}",
            self.0.number(),
            if self.1 { 'h' } else { 'l' }
        )
    }
}

pub struct KittyPush(pub u16);
impl fmt::Display for KittyPush {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\x1b[>{}u", self.0)
    }
}

pub fn focus_report(sequence: &[u8]) -> Option<crate::FocusEvent> {
    if sequence == FOCUS_GAINED {
        Some(crate::FocusEvent::Gained)
    } else if sequence == FOCUS_LOST {
        Some(crate::FocusEvent::Lost)
    } else {
        None
    }
}

pub fn is_color_scheme_query(params: &[u8], final_byte: u8) -> bool {
    final_byte == b'n' && params == &COLOR_SCHEME_QUERY.as_bytes()[2..COLOR_SCHEME_QUERY.len() - 1]
}

pub struct ColorQuerySequence(pub ColorQueryTarget, pub ReplyForm);
impl fmt::Display for ColorQuerySequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\x1b]")?;
        match self.0 {
            ColorQueryTarget::Palette(index) => write!(f, "4;{index}")?,
            ColorQueryTarget::Foreground => f.write_str("10")?,
            ColorQueryTarget::Background => f.write_str("11")?,
            ColorQueryTarget::Cursor => f.write_str("12")?,
        }
        write!(f, ";?{}", self.1.terminator())
    }
}

pub const HOST_KEYBOARD_QUERY_SEQUENCE: &[u8] = b"\x1b[?u\x1b[c";
pub const HOST_CELL_SIZE_QUERY_SEQUENCE: &[u8] = b"\x1b[16t";
pub const HOST_KITTY_KEYBOARD_POP_SEQUENCE: &[u8] = b"\x1b[<1u";
pub const HOST_CURSOR_SHAPE_DEFAULT_SEQUENCE: &[u8] = b"\x1b[0 q";
pub const HOST_CURSOR_AND_SHAPE_RESTORE_SEQUENCE: &[u8] = b"\x1b[?25h\x1b[0 q";
pub const HOST_MOUSE_SGR_PIXELS_ENABLE_SEQUENCE: &[u8] = b"\x1b[?1016h";
pub const HOST_WINDOW_TITLE_PUSH_SEQUENCE: &[u8] = b"\x1b[22;0t";
pub const HOST_WINDOW_TITLE_POP_SEQUENCE: &[u8] = b"\x1b[23;0t";

pub const HOST_COLOR_QUERY_SEQUENCE: &str = "\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
pub const HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE: &str = "\x1b[?2031h";
pub const HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE: &str = "\x1b[?2031l";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_reply_replicates_channels_and_preserves_terminator() {
        let rgb = RgbColor {
            r: 0x12,
            g: 0x80,
            b: 0xff,
        };
        assert_eq!(
            ColorReply(ColorQueryTarget::Palette(18), rgb, ReplyForm::Bel).to_string(),
            "\x1b]4;18;rgb:1212/8080/ffff\x07"
        );
        assert_eq!(
            ColorReply(ColorQueryTarget::Foreground, rgb, ReplyForm::St).to_string(),
            "\x1b]10;rgb:1212/8080/ffff\x1b\\"
        );
    }

    #[test]
    fn malformed_non_ascii_hash_colour_is_rejected() {
        assert_eq!(parse_rgb_color("#a\u{e9}000"), None);
    }
}
