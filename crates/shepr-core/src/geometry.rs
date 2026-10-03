//! Grid and cell pixel dimensions shared by the terminal, PTY, and client, and
//! the cell rect the pane layout is computed in.

use std::num::{NonZeroU16, NonZeroU32};

use serde::{Deserialize, Serialize};

use crate::limits::{
    MAX_TERMINAL_GRID_CELLS, MAX_TERMINAL_GRID_DIMENSION, PANE_MIN_COLS, PANE_MIN_ROWS,
};

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

/// A nonempty terminal grid that fits the shared per-axis and cell-count
/// resource budgets.
///
/// Raw host geometry remains a [`GridSize`]: a host terminal can report
/// dimensions outside these budgets, and the server handshake needs to see
/// those values so it can return the matching refusal. Use this type for
/// grids that the application is about to retain or allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedGridSize(GridSize);

/// Why a grid cannot be represented by [`BoundedGridSize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedGridSizeError {
    /// At least one axis has no cells.
    ZeroDimension,
    /// At least one axis exceeds the per-axis limit.
    DimensionTooLarge,
    /// The grid exceeds the total cell-count limit.
    TooManyCells,
}

impl BoundedGridSize {
    /// Validate nonzero dimensions against the shared grid resource budgets.
    pub fn new(cols: u16, rows: u16) -> Result<Self, BoundedGridSizeError> {
        let grid = GridSize::new(cols, rows).ok_or(BoundedGridSizeError::ZeroDimension)?;
        Self::try_from(grid)
    }

    /// Clamp a requested grid to the shared budgets, keeping its width first
    /// and trimming excess height.
    pub fn clamped(cols: u16, rows: u16) -> Self {
        let cols = cols.clamp(1, MAX_TERMINAL_GRID_DIMENSION);
        let row_budget = MAX_TERMINAL_GRID_CELLS / usize::from(cols);
        let max_rows_by_cells = u16::try_from(row_budget).unwrap_or(MAX_TERMINAL_GRID_DIMENSION);
        let max_rows = MAX_TERMINAL_GRID_DIMENSION.min(max_rows_by_cells);
        let rows = rows.clamp(1, max_rows);
        Self(GridSize::clamped(cols, rows))
    }

    /// The ordinary nonzero grid carried by this bounded value.
    pub fn grid(self) -> GridSize {
        self.0
    }

    pub fn cols(self) -> u16 {
        self.0.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.0.rows.get()
    }

    /// Number of cells in this already-validated grid.
    pub fn cell_count(self) -> usize {
        usize::from(self.cols()) * usize::from(self.rows())
    }
}

impl TryFrom<GridSize> for BoundedGridSize {
    type Error = BoundedGridSizeError;

    fn try_from(grid: GridSize) -> Result<Self, Self::Error> {
        let cols = grid.cols.get();
        let rows = grid.rows.get();
        if cols > MAX_TERMINAL_GRID_DIMENSION || rows > MAX_TERMINAL_GRID_DIMENSION {
            return Err(BoundedGridSizeError::DimensionTooLarge);
        }
        if usize::from(cols) * usize::from(rows) > MAX_TERMINAL_GRID_CELLS {
            return Err(BoundedGridSizeError::TooManyCells);
        }
        Ok(Self(grid))
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
/// The fields are private and every constructor, deserialization included,
/// clamps the grid to the pane minimum, so a value never holds a grid below
/// it. The PTY and the emulator rely on that and apply no clamp of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PaneGeometryRepr")]
pub struct PaneGeometry {
    grid: GridSize,
    cell: Option<CellPx>,
}

#[derive(Deserialize)]
struct PaneGeometryRepr {
    grid: GridSize,
    cell: Option<CellPx>,
}

impl From<PaneGeometryRepr> for PaneGeometry {
    fn from(received: PaneGeometryRepr) -> Self {
        Self::with_cell(
            received.grid.cols.get(),
            received.grid.rows.get(),
            received.cell,
        )
    }
}

impl PaneGeometry {
    /// A `cols` by `rows` grid, clamped to the pane minimum, whose cells
    /// measure `width` by `height` pixels; pixel-less when either is zero.
    pub fn new(cols: u16, rows: u16, width: u32, height: u32) -> Self {
        Self::with_cell(cols, rows, CellPx::new(width, height))
    }

    /// A `cols` by `rows` grid, clamped to the pane minimum, with an
    /// already validated cell pixel size.
    pub fn with_cell(cols: u16, rows: u16, cell: Option<CellPx>) -> Self {
        Self {
            grid: GridSize::clamped_pane(cols, rows),
            cell,
        }
    }

    pub fn grid(self) -> GridSize {
        self.grid
    }

    pub fn cell(self) -> Option<CellPx> {
        self.cell
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

/// Physical host geometry, whose grid may be smaller than a pane's minimum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostGeometry {
    grid: GridSize,
    cell: Option<CellPx>,
    pub exact: bool,
}

impl HostGeometry {
    pub fn new(cols: u16, rows: u16, width: u32, height: u32, exact: bool) -> Self {
        let grid = GridSize::clamped(cols, rows);
        let cell = CellPx::new(width, height);
        Self {
            grid,
            cell,
            exact: exact && cell.is_some(),
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
        assert_eq!(PaneGeometry::new(80, 24, 8, 0).cell(), None);
        assert!(!HostGeometry::new(80, 24, 8, 0, true).exact);
    }

    #[test]
    fn pane_geometry_uses_the_shared_minimum_grid() {
        let pane = PaneGeometry::new(0, 1, 8, 16);
        assert_eq!((pane.cols(), pane.rows()), (PANE_MIN_COLS, PANE_MIN_ROWS));
        let generic = GridSize::clamped(0, 0);
        assert_eq!((generic.cols.get(), generic.rows.get()), (1, 1));
        let with_cell = PaneGeometry::with_cell(1, 0, CellPx::new(8, 16));
        assert_eq!(
            (with_cell.cols(), with_cell.rows()),
            (PANE_MIN_COLS, PANE_MIN_ROWS)
        );
        assert_eq!(with_cell.cell(), CellPx::new(8, 16));
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
