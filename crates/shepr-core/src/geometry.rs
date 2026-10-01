//! Grid and cell pixel dimensions shared by the terminal, PTY, and client, and
//! the cell rect the pane layout is computed in.

use std::num::{NonZeroU16, NonZeroU32};

use serde::{Deserialize, Serialize};

use crate::limits::{PANE_MIN_COLS, PANE_MIN_ROWS};

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

/// A cell-addressed area: the layout model's rect. Rendering crates convert it
/// to their drawing library's rect at the boundary, so this crate stays free
/// of any TUI dependency.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    /// Clamp `width` and `height` so the right and bottom edges stay within
    /// `u16`.
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width: x.saturating_add(width) - x,
            height: y.saturating_add(height) - y,
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

/// Pane geometry with an optional cell-pixel size.
///
/// Deserialization clamps the grid to the pane minimum. The public fields
/// still permit callers to construct a below-minimum value directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PaneGeometryRepr")]
pub struct PaneGeometry {
    pub grid: GridSize,
    pub cell: Option<CellPx>,
}

#[derive(Deserialize)]
struct PaneGeometryRepr {
    grid: GridSize,
    cell: Option<CellPx>,
}

impl From<PaneGeometryRepr> for PaneGeometry {
    fn from(received: PaneGeometryRepr) -> Self {
        Self {
            grid: received.grid,
            cell: received.cell,
        }
        .clamped()
    }
}

impl PaneGeometry {
    pub fn new(cols: u16, rows: u16, width: u32, height: u32) -> Self {
        Self {
            grid: GridSize::clamped_pane(cols, rows),
            cell: CellPx::new(width, height),
        }
    }

    /// Clamp pane grids below the shared minimum. Deserialization applies this
    /// boundary; callers that build values directly can reapply it.
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
    fn rect_new_keeps_the_far_edges_within_u16() {
        assert_eq!(
            Rect::new(1, 2, 3, 4),
            Rect {
                x: 1,
                y: 2,
                width: 3,
                height: 4
            }
        );
        let edge = Rect::new(u16::MAX - 1, u16::MAX, 10, 10);
        assert_eq!((edge.width, edge.height), (1, 0));
    }

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
    fn pane_geometry_received_representation_clamps_grid_minimum() {
        let pane = PaneGeometry::from(PaneGeometryRepr {
            grid: GridSize::clamped(1, 1),
            cell: None,
        });

        assert_eq!((pane.cols(), pane.rows()), (PANE_MIN_COLS, PANE_MIN_ROWS));
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
