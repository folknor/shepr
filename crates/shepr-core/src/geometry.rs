//! Grid and cell pixel dimensions shared by the terminal, PTY, and client.

use std::num::{NonZeroU16, NonZeroU32};

use serde::{Deserialize, Serialize};

const PANE_MIN_COLS: u16 = 4;
const PANE_MIN_ROWS: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SplitBranch {
    First,
    Second,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSize {
    pub cols: NonZeroU16,
    pub rows: NonZeroU16,
}

impl GridSize {
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        Some(Self {
            cols: NonZeroU16::new(cols)?,
            rows: NonZeroU16::new(rows)?,
        })
    }

    /// Preserve host and protocol grids down to one cell. Pane PTYs use
    /// `clamped_pane` for their larger minimum.
    pub fn clamped(cols: u16, rows: u16) -> Self {
        Self {
            cols: NonZeroU16::new(cols).unwrap_or(NonZeroU16::MIN),
            rows: NonZeroU16::new(rows).unwrap_or(NonZeroU16::MIN),
        }
    }

    /// Clamp pane grids to the shared minimum used by the PTY and emulator.
    pub fn clamped_pane(cols: u16, rows: u16) -> Self {
        Self {
            cols: NonZeroU16::new(cols.max(PANE_MIN_COLS)).unwrap_or(NonZeroU16::MIN),
            rows: NonZeroU16::new(rows.max(PANE_MIN_ROWS)).unwrap_or(NonZeroU16::MIN),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellPx {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl CellPx {
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Some(Self {
            width: NonZeroU32::new(width)?,
            height: NonZeroU32::new(height)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneGeometry {
    pub grid: GridSize,
    pub cell: Option<CellPx>,
}

impl PaneGeometry {
    pub fn new(cols: u16, rows: u16, width: u32, height: u32) -> Self {
        Self {
            grid: GridSize::clamped_pane(cols, rows),
            cell: CellPx::new(width, height),
        }
    }

    /// Reapply the pane-grid boundary to geometry received as a struct or wire
    /// value.
    pub fn clamped(self) -> Self {
        Self {
            grid: GridSize::clamped_pane(self.cols(), self.rows()),
            cell: self.cell,
        }
    }

    pub fn cols(self) -> u16 {
        self.grid.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.grid.rows.get()
    }

    pub fn cell_width(self) -> u32 {
        self.cell.map_or(0, |cell| cell.width.get())
    }

    pub fn cell_height(self) -> u32 {
        self.cell.map_or(0, |cell| cell.height.get())
    }

    /// The pixel extent representable by `TIOCSWINSZ` and `TIOCGWINSZ`.
    /// Terminal reports use this same extent so a child sees one pixel size.
    pub fn text_area_px(self) -> Option<(u16, u16)> {
        let cell = self.cell?;
        let width = u64::from(self.cols()) * u64::from(cell.width.get());
        let height = u64::from(self.rows()) * u64::from(cell.height.get());
        Some((
            u16::try_from(width.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
            u16::try_from(height.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostGeometry {
    pub pane: PaneGeometry,
    pub exact: bool,
}

impl HostGeometry {
    pub fn new(cols: u16, rows: u16, width: u32, height: u32, exact: bool) -> Self {
        let pane = PaneGeometry::new(cols, rows, width, height);
        Self {
            pane,
            exact: exact && pane.cell.is_some(),
        }
    }

    pub fn cols(self) -> u16 {
        self.pane.cols()
    }

    pub fn rows(self) -> u16 {
        self.pane.rows()
    }

    pub fn cell_width(self) -> u32 {
        self.pane.cell_width()
    }

    pub fn cell_height(self) -> u32 {
        self.pane.cell_height()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_rejects_zero_components() {
        assert!(GridSize::new(0, 24).is_none());
        assert!(CellPx::new(8, 0).is_none());
        assert_eq!(PaneGeometry::new(80, 24, 8, 0).cell, None);
        assert!(!HostGeometry::new(80, 24, 8, 0, true).exact);
    }

    #[test]
    fn pane_geometry_uses_the_shared_minimum_grid() {
        let pane = PaneGeometry::new(0, 1, 8, 16);
        assert_eq!((pane.cols(), pane.rows()), (PANE_MIN_COLS, PANE_MIN_ROWS));
        let generic = GridSize::clamped(0, 0);
        assert_eq!((generic.cols.get(), generic.rows.get()), (1, 1));
        assert_eq!(pane.clamped(), pane);
    }

    #[test]
    fn pixel_extent_uses_winsize_limits() {
        let small = PaneGeometry::new(80, 24, 9, 18);
        assert_eq!(small.text_area_px(), Some((720, 432)));

        let large = PaneGeometry::new(80, 24, 100_000, 100_000);
        assert_eq!(large.text_area_px(), Some((u16::MAX, u16::MAX)));
        assert_eq!(PaneGeometry::new(80, 24, 0, 18).text_area_px(), None);
    }
}
