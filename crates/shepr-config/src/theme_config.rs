use serde::{Deserialize, Serialize};

use crate::theme::{ParsedThemeColors, THEME_NAMES, canonical_theme_name};

/// Theme configuration: pick a built-in or override individual tokens.
///
/// ```toml
/// [theme]
/// name = "tokyo-night"  # built-in: catppuccin, terminal, dracula, nord, etc.
///
/// [theme.custom]        # override individual tokens on top of the base
/// accent = "#f5c2e7"
/// red = "#ff6188"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Built-in theme name. Default: "catppuccin".
    pub name: Option<String>,
    /// Custom overrides - applied on top of the selected base theme.
    pub custom: Option<CustomThemeColors>,
}

/// Per-token color overrides. All fields optional - only set what you want to change.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct CustomThemeColors {
    pub accent: Option<String>,
    pub panel_bg: Option<String>,
    pub sidebar_bg: Option<String>,
    pub active_row_bg: Option<String>,
    pub selection_bg: Option<String>,
    pub surface0: Option<String>,
    pub surface1: Option<String>,
    pub surface_dim: Option<String>,
    pub overlay0: Option<String>,
    pub overlay1: Option<String>,
    pub text: Option<String>,
    pub subtext0: Option<String>,
    pub mauve: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub red: Option<String>,
    pub blue: Option<String>,
    pub teal: Option<String>,
    pub peach: Option<String>,
}

impl CustomThemeColors {
    pub(crate) fn parse(&self) -> Result<ParsedThemeColors, Vec<String>> {
        let mut diagnostics = Vec::new();
        macro_rules! color {
            ($field:ident) => {
                match parse_configured_color(
                    concat!("theme.custom.", stringify!($field)),
                    self.$field.as_deref(),
                ) {
                    Ok(color) => color,
                    Err(errors) => {
                        diagnostics.extend(errors);
                        None
                    }
                }
            };
        }

        let parsed = ParsedThemeColors {
            accent: color!(accent),
            panel_bg: color!(panel_bg),
            sidebar_bg: color!(sidebar_bg),
            active_row_bg: color!(active_row_bg),
            selection_bg: color!(selection_bg),
            surface0: color!(surface0),
            surface1: color!(surface1),
            surface_dim: color!(surface_dim),
            overlay0: color!(overlay0),
            overlay1: color!(overlay1),
            text: color!(text),
            subtext0: color!(subtext0),
            mauve: color!(mauve),
            green: color!(green),
            yellow: color!(yellow),
            red: color!(red),
            blue: color!(blue),
            teal: color!(teal),
            peach: color!(peach),
        };
        if diagnostics.is_empty() {
            Ok(parsed)
        } else {
            Err(diagnostics)
        }
    }
}

fn parse_configured_color(
    field: &str,
    value: Option<&str>,
) -> Result<Option<ratatui::style::Color>, Vec<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    try_parse_color(value).map(Some).ok_or_else(|| {
        vec![format!(
            "invalid color {field} = {value:?}; expected #rrggbb, #rgb, rgb(r, g, b), a color name, or reset"
        )]
    })
}

pub(crate) fn resolve_palette(
    config: &super::Config,
    ui_accent_is_explicit: bool,
) -> Result<crate::theme::Palette, Vec<String>> {
    let mut diagnostics = Vec::new();
    let name = config.theme.name.as_deref().unwrap_or("catppuccin");
    let canonical = canonical_theme_name(name).or_else(|| {
        diagnostics.push(format!(
            "unknown theme name theme.name = {name:?}; valid themes: {}",
            THEME_NAMES.join(", ")
        ));
        None
    });
    let base_palette = canonical.and_then(|canonical| {
        let palette = crate::theme::Palette::from_name(canonical);
        if palette.is_none() {
            diagnostics.push(format!("theme {canonical:?} has no built-in palette"));
        }
        palette
    });
    let overrides = config.theme.custom.as_ref().map_or_else(
        || Ok(ParsedThemeColors::default()),
        CustomThemeColors::parse,
    );
    if let Err(errors) = &overrides {
        diagnostics.extend(errors.iter().cloned());
    }
    let ui_accent = match parse_configured_color("ui.accent", Some(config.ui.accent.as_str())) {
        Ok(color) => color,
        Err(errors) => {
            diagnostics.extend(errors);
            None
        }
    };
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let Some(mut palette) = base_palette else {
        return Err(vec![
            "the built-in theme palette could not be resolved".into(),
        ]);
    };
    if let Ok(overrides) = overrides {
        palette = palette.with_overrides(&overrides);
    }
    let custom_accent = config
        .theme
        .custom
        .as_ref()
        .is_some_and(|custom| custom.accent.is_some());
    if !custom_accent && ui_accent_is_explicit {
        let Some(accent) = ui_accent else {
            return Err(vec![
                "ui.accent was marked configured without a value".to_owned(),
            ]);
        };
        palette.accent = accent;
    }
    Ok(palette)
}

/// Parse a color string into a ratatui Color, or `None` if it is not one.
/// Supports: hex (#rrggbb, #rgb), named colors, rgb(r,g,b), and reset aliases.
pub(crate) fn try_parse_color(s: &str) -> Option<ratatui::style::Color> {
    use ratatui::style::Color;
    let s = s.trim().to_lowercase();

    match s.as_str() {
        "reset" | "default" | "none" | "transparent" => return Some(Color::Reset),
        _ => {}
    }

    // Check the digits as bytes before slicing: a byte length of 6 or 3 says
    // nothing about character boundaries ("#aééb" is 6 bytes), and
    // `from_str_radix` would also accept a leading '+'.
    if let Some(hex) = s.strip_prefix('#')
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        // Every byte is an ASCII hex digit here, so the arithmetic stays in range.
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => byte - b'0',
            _ => byte.to_ascii_lowercase() - b'a' + 10,
        };
        match *hex.as_bytes() {
            [r1, r2, g1, g2, b1, b2] => {
                return Some(Color::Rgb(
                    digit(r1) * 16 + digit(r2),
                    digit(g1) * 16 + digit(g2),
                    digit(b1) * 16 + digit(b2),
                ));
            }
            [r, g, b] => return Some(Color::Rgb(digit(r) * 17, digit(g) * 17, digit(b) * 17)),
            _ => {}
        }
    }

    if let Some(inner) = s.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 3
            && let (Ok(r), Ok(g), Ok(b)) = (
                parts[0].trim().parse::<u8>(),
                parts[1].trim().parse::<u8>(),
                parts[2].trim().parse::<u8>(),
            )
        {
            return Some(Color::Rgb(r, g, b));
        }
    }

    Some(match s.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn theme_name_parses() {
        let toml = r#"
[theme]
name = "dracula"
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.theme.name.as_deref(), Some("dracula"));
    }

    #[test]
    fn unknown_theme_names_are_diagnosed() {
        let config: Config = toml::from_str(
            r#"
[theme]
name = "catppucin"
"#,
        )
        .expect("test precondition");

        let diagnostics = config.collect_diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("theme.name = \"catppucin\""));
        assert!(diagnostics[0].contains("valid themes:"));
    }

    #[test]
    fn theme_name_aliases_are_valid() {
        for name in ["catppuccin-mocha", "tokyonight", "gruvbox-dark", "dawn"] {
            assert!(canonical_theme_name(name).is_some(), "alias: {name}");
        }
    }

    #[test]
    fn parse_color_accepts_reset_aliases() {
        use ratatui::style::Color;

        for value in ["reset", "default", "none", "transparent"] {
            assert_eq!(try_parse_color(value), Some(Color::Reset), "value: {value}");
        }
    }

    #[test]
    fn theme_custom_overrides_parse() {
        let toml = r##"
[theme]
name = "nord"

[theme.custom]
panel_bg = "#1e1e2e"
sidebar_bg = "#181825"
active_row_bg = "#313244"
selection_bg = "#45475a"
accent = "#ff79c6"
red = "rgb(255, 85, 85)"
"##;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.theme.name.as_deref(), Some("nord"));
        let custom = config.theme.custom.as_ref().expect("test precondition");
        assert_eq!(custom.panel_bg.as_deref(), Some("#1e1e2e"));
        assert_eq!(custom.sidebar_bg.as_deref(), Some("#181825"));
        assert_eq!(custom.active_row_bg.as_deref(), Some("#313244"));
        assert_eq!(custom.selection_bg.as_deref(), Some("#45475a"));
        assert_eq!(custom.accent.as_deref(), Some("#ff79c6"));
        assert_eq!(custom.red.as_deref(), Some("rgb(255, 85, 85)"));
        assert!(custom.green.is_none());
    }

    #[test]
    fn theme_defaults_when_missing() {
        let config: Config = toml::from_str("").expect("test precondition");
        assert!(config.theme.name.is_none());
        assert!(config.theme.custom.is_none());
    }

    #[test]
    fn parse_color_rejects_non_ascii_hex_without_panicking() {
        // Six and three bytes long, but not six or three hex digits: slicing
        // by byte offset would split a multi-byte character.
        for value in ["#aééb", "#é\u{1}", "#ab€", "#+f+f+f", "#+ff"] {
            assert_eq!(try_parse_color(value), None, "value: {value:?}");
        }
    }

    #[test]
    fn parse_color_reads_hex_forms() {
        use ratatui::style::Color;

        assert_eq!(
            try_parse_color("#1E1e2E"),
            Some(Color::Rgb(0x1e, 0x1e, 0x2e))
        );
        assert_eq!(try_parse_color(" #fff "), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(try_parse_color("#0a9"), Some(Color::Rgb(0x00, 0xaa, 0x99)));
        assert_eq!(try_parse_color("#12345"), None);
        assert_eq!(try_parse_color("#12345g"), None);
        assert_eq!(try_parse_color("rgb(1, 2, 3)"), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(try_parse_color("LightBlue"), Some(Color::LightBlue));
        assert_eq!(try_parse_color("bluish"), None);
    }

    #[test]
    fn invalid_custom_colors_are_diagnosed() {
        let config: Config = toml::from_str(
            r##"
[theme.custom]
accent = "#ff79c6"
red = "bluish"
peach = "#aééb"
"##,
        )
        .expect("test precondition");

        let diagnostics = config.collect_diagnostics();
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(diagnostics[0].contains("theme.custom.red = \"bluish\""));
        assert!(diagnostics[1].contains("theme.custom.peach = \"#aééb\""));
        assert!(diagnostics.iter().all(|d| d.contains("expected #rrggbb")));
    }

    #[test]
    fn invalid_ui_accent_reaches_config_diagnostics() {
        let config: Config =
            toml::from_str("[ui]\naccent = \"#aééb\"\n").expect("test precondition");
        let diagnostics = config.collect_diagnostics();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("invalid color ui.accent = \"#aééb\"")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("invalid color ui.accent"))
        );

        let custom: Config =
            toml::from_str("[ui]\naccent = \"#aééb\"\n[theme.custom]\naccent = \"#112233\"\n")
                .expect("test precondition");
        assert!(
            custom
                .collect_diagnostics()
                .iter()
                .any(|d| { d.contains("invalid color ui.accent") })
        );

        let valid: Config =
            toml::from_str("[ui]\naccent = \"magenta\"\n").expect("test precondition");
        assert!(
            !valid
                .collect_diagnostics()
                .iter()
                .any(|d| d.contains("invalid color"))
        );
    }

    #[test]
    fn default_config_has_no_color_diagnostics() {
        let config = Config::default();
        assert!(
            !config
                .collect_diagnostics()
                .iter()
                .any(|d| d.contains("invalid color"))
        );
    }
}
