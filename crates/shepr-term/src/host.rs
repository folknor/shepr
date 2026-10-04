//! Facts observed about the terminal hosting a client: its default and
//! palette colours and its appearance. The client
//! reads them from the host and reports them; the server applies them to
//! panes. Querying and parsing them is host terminal I/O, which lives in
//! `shepr-termio`.

pub use crate::{ColorScheme as HostAppearance, DefaultColor as DefaultColorKind, RgbColor};

use shepr_core::limits::PALETTE_COLOR_COUNT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTheme {
    pub foreground: Option<RgbColor>,
    pub background: Option<RgbColor>,
    pub palette: [Option<RgbColor>; PALETTE_COLOR_COUNT],
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self {
            foreground: None,
            background: None,
            palette: [None; PALETTE_COLOR_COUNT],
        }
    }
}

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

    /// Returns the host's palette entry, falling back to the built-in palette.
    pub fn palette_color(&self, index: u8) -> RgbColor {
        self.palette[usize::from(index)].unwrap_or_else(|| crate::default_palette_color(index))
    }

    pub fn is_empty(self) -> bool {
        self.foreground.is_none()
            && self.background.is_none()
            && self.palette.iter().all(Option::is_none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_only_theme_is_not_empty() {
        let theme = TerminalTheme::default().with_palette_color(12, RgbColor { r: 1, g: 2, b: 3 });
        assert!(!theme.is_empty());
        assert!(TerminalTheme::default().is_empty());
    }
}
