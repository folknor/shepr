use crate::protocol::{ClientHostAppearance, ClientHostColor, ClientHostDefaultColorKind};
pub use shepr_vt::{ColorScheme as HostAppearance, DefaultColor as DefaultColorKind, RgbColor};

impl From<RgbColor> for ClientHostColor {
    fn from(color: RgbColor) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
        }
    }
}

impl From<ClientHostColor> for RgbColor {
    fn from(color: ClientHostColor) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
        }
    }
}

impl From<DefaultColorKind> for ClientHostDefaultColorKind {
    fn from(kind: DefaultColorKind) -> Self {
        match kind {
            DefaultColorKind::Foreground => Self::Foreground,
            DefaultColorKind::Background => Self::Background,
        }
    }
}

impl From<ClientHostDefaultColorKind> for DefaultColorKind {
    fn from(kind: ClientHostDefaultColorKind) -> Self {
        match kind {
            ClientHostDefaultColorKind::Foreground => Self::Foreground,
            ClientHostDefaultColorKind::Background => Self::Background,
        }
    }
}

impl From<HostAppearance> for ClientHostAppearance {
    fn from(appearance: HostAppearance) -> Self {
        match appearance {
            HostAppearance::Dark => Self::Dark,
            HostAppearance::Light => Self::Light,
        }
    }
}

impl From<ClientHostAppearance> for HostAppearance {
    fn from(appearance: ClientHostAppearance) -> Self {
        match appearance {
            ClientHostAppearance::Dark => Self::Dark,
            ClientHostAppearance::Light => Self::Light,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTheme {
    pub foreground: Option<RgbColor>,
    pub background: Option<RgbColor>,
    pub palette: [Option<RgbColor>; 256],
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self {
            foreground: None,
            background: None,
            palette: [None; 256],
        }
    }
}

pub const HOST_COLOR_QUERY_SEQUENCE: &str = "\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
pub const HOST_COLOR_SCHEME_QUERY_SEQUENCE: &str = "\x1b[?996n";
pub const HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE: &str = "\x1b[?2031h";
pub const HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE: &str = "\x1b[?2031l";

impl TerminalTheme {
    pub fn with_color(mut self, kind: DefaultColorKind, color: RgbColor) -> Self {
        match kind {
            DefaultColorKind::Foreground => self.foreground = Some(color),
            DefaultColorKind::Background => self.background = Some(color),
        }
        self
    }

    pub fn with_palette_color(mut self, index: u8, color: RgbColor) -> Self {
        self.palette[usize::from(index)] = Some(color);
        self
    }

    pub fn is_empty(self) -> bool {
        self.foreground.is_none()
            && self.background.is_none()
            && self.palette.iter().all(Option::is_none)
    }
}

pub fn host_terminal_theme_query_sequence(include_palette: bool) -> String {
    use std::fmt::Write as _;

    let mut sequence = String::from(HOST_COLOR_QUERY_SEQUENCE);
    if include_palette {
        for index in 0..=u8::MAX {
            let _ = write!(sequence, "\x1b]4;{index};?\x1b\\");
        }
    }
    sequence
}

pub fn parse_default_color_response(sequence: &str) -> Option<(DefaultColorKind, RgbColor)> {
    let body = sequence.strip_prefix("\x1b]")?;
    let body = body
        .strip_suffix("\x1b\\")
        .or_else(|| body.strip_suffix('\u{7}'))?;
    let (command, value) = body.split_once(';')?;
    let kind = match command {
        "10" => DefaultColorKind::Foreground,
        "11" => DefaultColorKind::Background,
        _ => return None,
    };
    Some((kind, parse_rgb_color(value)?))
}

pub fn parse_palette_color_response(sequence: &str) -> Option<(u8, RgbColor)> {
    let body = sequence.strip_prefix("\x1b]4;")?;
    let body = body
        .strip_suffix("\x1b\\")
        .or_else(|| body.strip_suffix('\u{7}'))?;
    let (index, value) = body.split_once(';')?;
    Some((index.parse().ok()?, parse_rgb_color(value)?))
}

fn parse_rgb_color(value: &str) -> Option<RgbColor> {
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
        let digits = hex.len() / 3;
        if !matches!(digits, 1..=4) || hex.len() != digits * 3 {
            return None;
        }
        return Some(RgbColor {
            r: parse_hex_component(&hex[..digits])?,
            g: parse_hex_component(&hex[digits..digits * 2])?,
            b: parse_hex_component(&hex[digits * 2..])?,
        });
    }

    None
}

fn parse_hex_component(component: &str) -> Option<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_st_terminated_rgb_response() {
        let parsed = parse_default_color_response("\x1b]10;rgb:cccc/dddd/eeee\x1b\\");
        assert_eq!(
            parsed,
            Some((
                DefaultColorKind::Foreground,
                RgbColor {
                    r: 0xcc,
                    g: 0xdd,
                    b: 0xee,
                },
            ))
        );
    }

    #[test]
    fn parses_bel_terminated_hash_response() {
        let parsed = parse_default_color_response("\x1b]11;#123456\u{7}");
        assert_eq!(
            parsed,
            Some((
                DefaultColorKind::Background,
                RgbColor {
                    r: 0x12,
                    g: 0x34,
                    b: 0x56,
                },
            ))
        );
    }

    #[test]
    fn parses_palette_responses_and_builds_full_query() {
        assert_eq!(
            parse_palette_color_response("\x1b]4;255;rgb:1111/2222/3333\x1b\\"),
            Some((
                255,
                RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }
            ))
        );

        let query = host_terminal_theme_query_sequence(true);
        assert!(query.starts_with(HOST_COLOR_QUERY_SEQUENCE));
        assert!(query.contains("\x1b]4;0;?\x1b\\"));
        assert!(query.ends_with("\x1b]4;255;?\x1b\\"));
        assert_eq!(query.matches("\x1b]4;").count(), 256);

        assert_eq!(
            host_terminal_theme_query_sequence(false),
            HOST_COLOR_QUERY_SEQUENCE
        );
    }

    #[test]
    fn scales_short_hex_components() {
        assert_eq!(parse_hex_component("f"), Some(255));
        assert_eq!(parse_hex_component("80"), Some(128));
        assert_eq!(parse_hex_component("800"), Some(128));
        assert_eq!(parse_hex_component("8000"), Some(128));
    }

    #[test]
    fn palette_only_theme_is_not_empty() {
        let theme = TerminalTheme::default().with_palette_color(12, RgbColor { r: 1, g: 2, b: 3 });
        assert!(!theme.is_empty());
        assert!(TerminalTheme::default().is_empty());
    }
}
