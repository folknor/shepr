//! The colours the client draws everything with. There is no theme to pick:
//! the palette is derived from the colours the host terminal reports
//! (`shepr_term::host_tint::UiPalette`), with the local server's hue as its
//! accent, and rederived whenever the terminal reports new ones. Until the
//! terminal has reported a background, and on a terminal that never does, the
//! palette is the terminal's own ANSI colours.

use ratatui::style::Color;
use shepr_term::host::TerminalTheme;
use shepr_term::host_tint::{HostHue, UiPalette};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::shell) struct Palette {
    /// Highlights and the focused pane's border.
    pub(in crate::shell) accent: Color,
    /// Floating panels, overlays and menus.
    pub(in crate::shell) panel_bg: Color,
    /// The active workspace and focused agent rows.
    pub(in crate::shell) active_row_bg: Color,
    /// The navigate-mode cursor row.
    pub(in crate::shell) selection_bg: Color,
    /// Selected and focused surfaces.
    pub(in crate::shell) surface0: Color,
    /// Hovered and active surfaces.
    pub(in crate::shell) surface1: Color,
    /// Separators and unfocused scrollbar tracks.
    pub(in crate::shell) surface_dim: Color,
    /// Muted text.
    pub(in crate::shell) overlay0: Color,
    /// Secondary text, a step brighter than `overlay0`.
    pub(in crate::shell) overlay1: Color,
    /// Main text.
    pub(in crate::shell) text: Color,
    /// Subdued text.
    pub(in crate::shell) subtext0: Color,
    /// Branch names and special labels.
    pub(in crate::shell) mauve: Color,
    /// Idle.
    pub(in crate::shell) green: Color,
    /// Working.
    pub(in crate::shell) yellow: Color,
    /// Blocked and errors.
    pub(in crate::shell) red: Color,
}

impl Palette {
    /// The palette for the terminal's colours as last reported, with `accent`
    /// as its accent hue.
    pub(in crate::shell) fn derive(theme: &TerminalTheme, accent: HostHue) -> Self {
        UiPalette::derive(theme, accent).map_or_else(|| Self::terminal(accent), Self::from_ui)
    }

    /// The terminal's own colours, for a terminal that has reported no
    /// background: the default foreground and background, its greys, and its
    /// ANSI colour of each hue.
    pub(in crate::shell) fn terminal(accent: HostHue) -> Self {
        Self {
            accent: Color::Indexed(accent.ansi_index()),
            panel_bg: Color::Reset,
            active_row_bg: Color::DarkGray,
            selection_bg: Color::Reset,
            surface0: Color::Reset,
            surface1: Color::DarkGray,
            surface_dim: Color::DarkGray,
            overlay0: Color::Gray,
            overlay1: Color::White,
            text: Color::Reset,
            subtext0: Color::Gray,
            mauve: Color::Indexed(HostHue::Purple.ansi_index()),
            green: Color::Indexed(HostHue::Green.ansi_index()),
            yellow: Color::Indexed(HostHue::Yellow.ansi_index()),
            red: Color::Indexed(HostHue::Red.ansi_index()),
        }
    }

    fn from_ui(ui: UiPalette) -> Self {
        let rgb = |color: shepr_term::RgbColor| Color::Rgb(color.r, color.g, color.b);
        Self {
            accent: rgb(ui.accent),
            panel_bg: rgb(ui.panel_bg),
            active_row_bg: rgb(ui.active_row_bg),
            selection_bg: rgb(ui.selection_bg),
            surface0: rgb(ui.surface0),
            surface1: rgb(ui.surface1),
            surface_dim: rgb(ui.surface_dim),
            overlay0: rgb(ui.overlay0),
            overlay1: rgb(ui.overlay1),
            text: rgb(ui.text),
            subtext0: rgb(ui.subtext0),
            mauve: rgb(ui.mauve),
            green: rgb(ui.green),
            yellow: rgb(ui.yellow),
            red: rgb(ui.red),
        }
    }
}

#[cfg(test)]
impl Palette {
    /// The palette a dark terminal (Catppuccin Mocha's background and text)
    /// derives, with a blue accent: every colour truecolour and distinct.
    pub(in crate::shell) fn test_dark() -> Self {
        Self::derive(&test_dark_theme(), HostHue::Blue)
    }
}

/// A dark terminal's reported colours.
#[cfg(test)]
pub(in crate::shell) fn test_dark_theme() -> TerminalTheme {
    TerminalTheme {
        background: Some(shepr_term::RgbColor {
            r: 0x1e,
            g: 0x1e,
            b: 0x2e,
        }),
        foreground: Some(shepr_term::RgbColor {
            r: 0xcd,
            g: 0xd6,
            b: 0xf4,
        }),
        ..TerminalTheme::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_without_a_background_gets_its_own_ansi_colours() {
        assert_eq!(
            Palette::derive(&TerminalTheme::default(), HostHue::Green),
            Palette::terminal(HostHue::Green)
        );
        assert_eq!(Palette::terminal(HostHue::Green).accent, Color::Indexed(2));
    }

    #[test]
    fn a_reported_background_derives_truecolour_with_the_hue_as_accent() {
        let blue = Palette::test_dark();
        let green = Palette::derive(&test_dark_theme(), HostHue::Green);
        assert!(matches!(blue.panel_bg, Color::Rgb(..)), "{blue:?}");
        assert_ne!(blue.accent, green.accent);
        assert_eq!(blue.panel_bg, green.panel_bg);
        assert_ne!(blue.active_row_bg, blue.selection_bg);
    }
}
