//! Facts observed about the terminal hosting a client: its default and
//! palette colours, its appearance and its cell size in pixels. The client
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

/// The outer host terminal's cell size in pixels, as reported by the XTWINOPS
/// `\x1b[16t` query. This is generic terminal geometry (used for pixel-accurate
/// pane resize reporting to child processes and mouse SGR-pixel coordinates),
/// not specific to any particular terminal graphics protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostCellSize {
    pub width_px: u32,
    pub height_px: u32,
}

impl HostCellSize {
    /// The usable cell, under the core host-cell policy: a zero axis or one
    /// above `CellPx::MAX_DIMENSION` is unknown. Real terminals report cells
    /// of tens of pixels (XTWINOPS 16t, or the winsize pixel fields divided
    /// by the grid), so the bound only ever refuses garbage. Every production
    /// value is built by `from_cell` from a `HostCellGeometry` that already
    /// applied this bound, so accepting any nonzero size here, as an earlier
    /// version did, changes nothing for a real report; it would only let a
    /// hand-built value bypass the policy. Keeping the bound on this type
    /// also keeps the pixel products its users take (cell times a `u16` grid
    /// axis, in `u32`) free of overflow without a check at each site.
    pub fn cell(self) -> Option<shepr_core::geometry::CellPx> {
        shepr_core::geometry::HostCellGeometry::from_wire(self.width_px, self.height_px, false)
            .cell()
    }

    pub fn from_cell(cell: Option<shepr_core::geometry::CellPx>) -> Self {
        Self {
            width_px: cell.map_or(0, |cell| cell.width.get()),
            height_px: cell.map_or(0, |cell| cell.height.get()),
        }
    }

    pub fn is_known(&self) -> bool {
        self.cell().is_some()
    }

    /// This size, or the unknown default when the reported dimensions are
    /// invalid.
    pub fn or_default(self) -> Self {
        if self.is_known() {
            self
        } else {
            Self::default()
        }
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
