//! Grid and cell pixel dimensions shared by the terminal, PTY, and client, and
//! the cell rect the pane layout is computed in.

use std::num::{NonZeroU16, NonZeroU32};

use serde::{Deserialize, Serialize};

use crate::limits::{
    MAX_TERMINAL_GRID_CELLS, MAX_TERMINAL_GRID_DIMENSION, PANE_MIN_COLS, PANE_MIN_ROWS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSize {
    pub cols: NonZeroU16,
    pub rows: NonZeroU16,
}

impl GridSize {
    pub fn cols(self) -> u16 {
        self.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.rows.get()
    }

    pub fn rect(self) -> Rect {
        Rect::new(0, 0, self.cols(), self.rows())
    }

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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// A raw nonzero cell size as a host or peer reported it, possibly above
/// `CellPx::MAX_DIMENSION`. Kept raw so a refusal can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellReport {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl CellReport {
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Some(Self {
            width: NonZeroU32::new(width)?,
            height: NonZeroU32::new(height)?,
        })
    }

    /// The usable cell, `None` when an axis is above the bound.
    pub fn cell(self) -> Option<CellPx> {
        CellPx::try_from(self).ok()
    }
}

/// A usable cell size: nonzero and within `MAX_DIMENSION` on both axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "CellReport", into = "CellReport")]
pub struct CellPx {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl CellPx {
    /// Largest usable host cell axis. Raw protocol reports may exceed this
    /// so the server can refuse them with a specific reason.
    pub const MAX_DIMENSION: u32 = crate::limits::MAX_HOST_CELL_PX;

    /// `None` for a zero or oversized axis.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        CellReport::new(width, height)?.cell()
    }

    pub fn width(self) -> NonZeroU32 {
        self.width
    }

    pub fn height(self) -> NonZeroU32 {
        self.height
    }
}

impl TryFrom<CellReport> for CellPx {
    /// The refused report, so a caller can name it.
    type Error = CellReport;

    fn try_from(report: CellReport) -> Result<Self, Self::Error> {
        if report.width.get() > Self::MAX_DIMENSION || report.height.get() > Self::MAX_DIMENSION {
            return Err(report);
        }
        Ok(Self {
            width: report.width,
            height: report.height,
        })
    }
}

impl std::fmt::Display for CellReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl From<CellPx> for CellReport {
    fn from(cell: CellPx) -> Self {
        Self {
            width: cell.width,
            height: cell.height,
        }
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
    /// A `cols` by `rows` grid, clamped to the pane minimum, with an
    /// already validated cell pixel size.
    pub fn with_cell(cols: u16, rows: u16, cell: Option<CellPx>) -> Self {
        Self {
            grid: GridSize::clamped_pane(cols, rows),
            cell,
        }
    }

    /// A pane with no known cell size.
    pub fn cells_only(cols: u16, rows: u16) -> Self {
        Self::with_cell(cols, rows, None)
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

    /// The extent `TIOCSWINSZ` can carry: each axis clamped to `u16::MAX`.
    /// `None` without a cell. Terminal reports use this same extent so a
    /// child sees one pixel size.
    pub fn pixel_extent(self) -> Option<PanePixelExtent> {
        let cell = self.cell?;
        let width = u64::from(self.cols()) * u64::from(cell.width.get());
        let height = u64::from(self.rows()) * u64::from(cell.height.get());
        PanePixelExtent::new(
            self.grid,
            u16::try_from(width.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
            u16::try_from(height.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
        )
    }
}

/// A pane's text area in pixels as its child was told it (winsize,
/// `CSI 14 t`, mode 2048), with the grid it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePixelExtent {
    grid: GridSize,
    width: NonZeroU16,
    height: NonZeroU16,
}

impl PanePixelExtent {
    /// `None` when either axis is zero.
    pub fn new(grid: GridSize, width: u16, height: u16) -> Option<Self> {
        Some(Self {
            grid,
            width: NonZeroU16::new(width)?,
            height: NonZeroU16::new(height)?,
        })
    }

    pub fn grid(self) -> GridSize {
        self.grid
    }

    pub fn width(self) -> NonZeroU16 {
        self.width
    }

    pub fn height(self) -> NonZeroU16 {
        self.height
    }

    /// The integer cell pitch a child derives from this extent:
    /// `(width / cols).max(1)`, `(height / rows).max(1)`.
    pub fn cell_pitch(self) -> (NonZeroU32, NonZeroU32) {
        let axis = |extent: NonZeroU16, count: u16| {
            let pitch = (u32::from(extent.get()) / u32::from(count).max(1)).max(1);
            NonZeroU32::new(pitch).unwrap_or(NonZeroU32::MIN)
        };
        (
            axis(self.width, self.grid.cols()),
            axis(self.height, self.grid.rows()),
        )
    }

    /// Whether a 1-based pixel lies inside the extent.
    pub fn contains(self, x: u32, y: u32) -> bool {
        (1..=u32::from(self.width.get())).contains(&x)
            && (1..=u32::from(self.height.get())).contains(&y)
    }

    /// The 1-based top-left pixel of a 0-based cell, through `cell_pitch`.
    pub fn cell_origin(self, column: u16, row: u16) -> (u32, u32) {
        let (cell_width, cell_height) = self.cell_pitch();
        (
            u32::from(column) * cell_width.get() + 1,
            u32::from(row) * cell_height.get() + 1,
        )
    }
}

/// The host terminal's cell as one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCell {
    Unknown,
    /// A cell the host did not measure exactly: a guess, an XTWINOPS reply
    /// without an ioctl extent, or a clamped report.
    Estimated(CellPx),
    /// Measured from one coherent ioctl; pixel mouse is possible.
    Exact(CellPx),
}

impl HostCell {
    /// A host reading: a zero axis is unknown, an oversized one is clamped
    /// to `CellPx::MAX_DIMENSION` and never exact. The one place a zero
    /// from the host is interpreted.
    pub fn from_host(width: u32, height: u32, exact: bool) -> Self {
        match CellReport::new(width, height) {
            Some(report) => Self::from_report(report, exact),
            None => Self::Unknown,
        }
    }

    /// The same for a nonzero raw report.
    pub fn from_report(report: CellReport, exact: bool) -> Self {
        match report.cell() {
            Some(cell) if exact => Self::Exact(cell),
            Some(cell) => Self::Estimated(cell),
            None => {
                let clamp = |axis: NonZeroU32| axis.get().min(CellPx::MAX_DIMENSION);
                match CellPx::new(clamp(report.width), clamp(report.height)) {
                    Some(cell) => Self::Estimated(cell),
                    None => Self::Unknown,
                }
            }
        }
    }

    pub fn cell(self) -> Option<CellPx> {
        match self {
            Self::Unknown => None,
            Self::Estimated(cell) | Self::Exact(cell) => Some(cell),
        }
    }

    pub fn is_exact(self) -> bool {
        matches!(self, Self::Exact(_))
    }

    /// What a connection keeps after a newer observation: an unknown newer
    /// observation keeps this cell, demoted to an estimate.
    pub fn refreshed_by(self, next: Self) -> Self {
        match (next, self.cell()) {
            (Self::Unknown, Some(cell)) => Self::Estimated(cell),
            _ => next,
        }
    }
}

/// Physical host geometry, whose grid may be smaller than a pane's minimum.
///
/// It lives here rather than in the client because the server's transport
/// carries and applies it too: it is the geometry a client reports, not only
/// one the client measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostGeometry {
    grid: GridSize,
    cell: HostCell,
}

impl HostGeometry {
    pub fn new(grid: GridSize, cell: HostCell) -> Self {
        Self { grid, cell }
    }

    pub fn with_grid(self, grid: GridSize) -> Self {
        Self { grid, ..self }
    }

    pub fn grid(self) -> GridSize {
        self.grid
    }

    pub fn cell(self) -> HostCell {
        self.cell
    }

    pub fn cols(self) -> u16 {
        self.grid.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.grid.rows.get()
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
        assert!(CellReport::new(0, 16).is_none());
        assert_eq!(HostCell::from_host(8, 0, true), HostCell::Unknown);
        assert!(PanePixelExtent::new(GridSize::clamped(80, 24), 0, 1).is_none());
    }

    #[test]
    fn cell_px_is_bounded_and_serde_goes_through_the_report() {
        assert!(CellPx::new(CellPx::MAX_DIMENSION, 1).is_some());
        assert!(CellPx::new(CellPx::MAX_DIMENSION + 1, 1).is_none());
        let oversized = CellReport::new(CellPx::MAX_DIMENSION + 1, 16).expect("nonzero");
        assert_eq!(oversized.cell(), None);
        assert_eq!(CellPx::try_from(oversized), Err(oversized));
        let cell = CellPx::new(8, 16).expect("valid");
        assert_eq!(CellReport::from(cell).cell(), Some(cell));
    }

    #[test]
    fn host_cell_keeps_exactness_and_bounds_reports() {
        let cell = CellPx::new(8, 16).expect("valid");
        assert_eq!(HostCell::from_host(8, 16, true), HostCell::Exact(cell));
        assert_eq!(HostCell::from_host(8, 16, false), HostCell::Estimated(cell));
        assert!(HostCell::from_host(8, 16, true).is_exact());
        assert!(!HostCell::Unknown.is_exact());

        let clamped = CellPx::new(CellPx::MAX_DIMENSION, 16).expect("valid");
        assert_eq!(
            HostCell::from_host(CellPx::MAX_DIMENSION + 1, 16, true),
            HostCell::Estimated(clamped)
        );
    }

    #[test]
    fn host_cell_refresh_demotes_a_cell_an_unknown_observation_keeps() {
        let cell = CellPx::new(8, 16).expect("valid");
        let other = CellPx::new(9, 18).expect("valid");
        assert_eq!(
            HostCell::Exact(cell).refreshed_by(HostCell::Unknown),
            HostCell::Estimated(cell)
        );
        assert_eq!(
            HostCell::Unknown.refreshed_by(HostCell::Unknown),
            HostCell::Unknown
        );
        assert_eq!(
            HostCell::Estimated(cell).refreshed_by(HostCell::Exact(other)),
            HostCell::Exact(other)
        );
    }

    #[test]
    fn host_geometry_keeps_small_grids_and_its_cell() {
        let cell = HostCell::from_host(8, 16, true);
        let small = HostGeometry::new(GridSize::clamped(1, 1), cell);
        assert_eq!(small.grid(), GridSize::clamped(1, 1));
        assert!(small.cell().is_exact());
        assert_eq!(small.with_grid(GridSize::clamped(2, 3)).cell(), cell);
    }

    #[test]
    fn pane_geometry_uses_the_shared_minimum_grid() {
        let pane = PaneGeometry::with_cell(0, 1, CellPx::new(8, 16));
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
        let small = PaneGeometry::with_cell(80, 24, CellPx::new(9, 18)).pixel_extent();
        let small = small.expect("cell known");
        assert_eq!((small.width().get(), small.height().get()), (720, 432));
        assert_eq!(small.grid(), GridSize::clamped(80, 24));

        let large = PaneGeometry::with_cell(80, 24, CellPx::new(CellPx::MAX_DIMENSION, 4096))
            .pixel_extent()
            .expect("cell known");
        assert_eq!(
            (large.width().get(), large.height().get()),
            (u16::MAX, u16::MAX)
        );
        assert_eq!(PaneGeometry::cells_only(80, 24).pixel_extent(), None);
        assert_eq!(
            small.cell_pitch(),
            (
                NonZeroU32::new(9).expect("nonzero"),
                NonZeroU32::new(18).expect("nonzero")
            )
        );
        assert_eq!(large.cell_pitch().0.get(), 65535 / 80);
    }

    #[test]
    fn pane_pixel_extent_cell_origin_is_one_based_top_left() {
        let extent = PaneGeometry::with_cell(80, 24, CellPx::new(9, 18))
            .pixel_extent()
            .expect("cell known");
        assert_eq!(extent.cell_origin(0, 0), (1, 1));
        assert_eq!(extent.cell_origin(2, 3), (19, 55));
        assert!(extent.contains(1, 1));
        assert!(extent.contains(720, 432));
        assert!(!extent.contains(0, 1));
        assert!(!extent.contains(721, 1));
        assert!(!extent.contains(1, 433));
    }
}
