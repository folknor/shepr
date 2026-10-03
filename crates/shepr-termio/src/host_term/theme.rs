use shepr_term::host::{DefaultColorKind, RgbColor};
use shepr_term::seq::parse_rgb_color;

pub const HOST_COLOR_QUERY_SEQUENCE: &str = shepr_term::seq::HOST_COLOR_QUERY_SEQUENCE;
pub const HOST_COLOR_SCHEME_QUERY_SEQUENCE: &str = shepr_term::seq::COLOR_SCHEME_QUERY;
pub const HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE: shepr_term::seq::DecModeSequence =
    shepr_term::seq::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE;
pub const HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE: shepr_term::seq::DecModeSequence =
    shepr_term::seq::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE;

pub fn host_terminal_theme_query_sequence() -> String {
    use std::fmt::Write as _;

    let mut sequence = String::from(HOST_COLOR_QUERY_SEQUENCE);
    for index in 0..=u8::MAX {
        // fmt::Write for String never returns an error.
        write!(
            sequence,
            "{}",
            shepr_term::seq::ColorQuerySequence(
                shepr_term::ColorQueryTarget::Palette(index),
                shepr_term::seq::ReplyForm::St
            )
        )
        .ok();
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

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::limits::PALETTE_COLOR_COUNT;
    use shepr_term::seq::parse_hex_component;

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

        let query = host_terminal_theme_query_sequence();
        assert!(query.starts_with(HOST_COLOR_QUERY_SEQUENCE));
        assert!(query.contains("\x1b]4;0;?\x1b\\"));
        assert!(query.ends_with("\x1b]4;255;?\x1b\\"));
        assert_eq!(query.matches("\x1b]4;").count(), PALETTE_COLOR_COUNT);
    }

    #[test]
    fn scales_short_hex_components() {
        assert_eq!(parse_hex_component("f"), Some(255));
        assert_eq!(parse_hex_component("80"), Some(128));
        assert_eq!(parse_hex_component("800"), Some(128));
        assert_eq!(parse_hex_component("8000"), Some(128));
    }
}
