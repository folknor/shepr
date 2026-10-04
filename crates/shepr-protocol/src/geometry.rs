//! Cell geometry at the client protocol boundary.

use serde::{Deserialize, Serialize};
use shepr_core::geometry::{
    BoundedGridSize, BoundedGridSizeError, CellReport, GridSize, HostCell, HostGeometry,
};

/// A client's host cell as sent; reports stay raw so the server can refuse
/// an oversized one by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportedCell {
    Unknown,
    Estimated(CellReport),
    Exact(CellReport),
}

impl ReportedCell {
    fn from_host(cell: HostCell) -> Self {
        match cell {
            HostCell::Unknown => Self::Unknown,
            HostCell::Estimated(cell) => Self::Estimated(cell.into()),
            HostCell::Exact(cell) => Self::Exact(cell.into()),
        }
    }

    /// The validated host cell, or the refusal naming an oversized axis.
    fn host_cell(self) -> Result<HostCell, super::SurfaceRefusal> {
        let (report, exact) = match self {
            Self::Unknown => return Ok(HostCell::Unknown),
            Self::Estimated(report) => (report, false),
            Self::Exact(report) => (report, true),
        };
        match report.cell() {
            Some(cell) if exact => Ok(HostCell::Exact(cell)),
            Some(cell) => Ok(HostCell::Estimated(cell)),
            None => Err(super::SurfaceRefusal::CellTooLarge),
        }
    }
}

/// Coherent geometry carried by a client.
///
/// Decoding keeps representable raw dimensions intact. The server checks
/// them during the handshake (`host_geometry`) so it can distinguish an
/// oversized axis from an excessive cell count in its refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalGeometry {
    grid: GridSize,
    cell: ReportedCell,
}

impl TerminalGeometry {
    /// Report a host's geometry as it was observed.
    pub fn from_host(grid: GridSize, cell: HostCell) -> Self {
        Self {
            grid,
            cell: ReportedCell::from_host(cell),
        }
    }

    pub fn grid(self) -> GridSize {
        self.grid
    }

    pub fn surface_size(self) -> super::ClientSurfaceSize {
        super::ClientSurfaceSize {
            cols: self.grid.cols(),
            rows: self.grid.rows(),
        }
    }

    /// Validate this raw geometry against the retained-surface grid budget.
    ///
    /// Handshake decoding deliberately does not call this: the server must
    /// inspect the raw dimensions to return the appropriate refusal reason.
    pub fn bounded_grid(self) -> Result<BoundedGridSize, BoundedGridSizeError> {
        self.grid.try_into()
    }

    /// The server's acceptance of a client geometry: the surface bounds
    /// (`DimensionTooLarge`, `TooManyCells`) and the cell bound
    /// (`CellTooLarge`), then the validated host geometry.
    pub fn host_geometry(self) -> Result<HostGeometry, super::SurfaceRefusal> {
        self.bounded_grid().map_err(|error| match error {
            BoundedGridSizeError::DimensionTooLarge => super::SurfaceRefusal::DimensionTooLarge,
            // A `GridSize` is nonzero by type, so a zero axis cannot arrive;
            // the budget it would break is the cell count.
            BoundedGridSizeError::ZeroDimension | BoundedGridSizeError::TooManyCells => {
                super::SurfaceRefusal::TooManyCells
            }
        })?;
        Ok(HostGeometry::new(self.grid, self.cell.host_cell()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::geometry::CellPx;

    fn report(width: u32, height: u32) -> CellReport {
        CellReport::new(width, height).expect("nonzero test cell")
    }

    #[test]
    fn host_cells_survive_the_wire_shape_with_their_exactness() {
        let cell = CellPx::new(8, 16).expect("valid");
        for host in [
            HostCell::Unknown,
            HostCell::Estimated(cell),
            HostCell::Exact(cell),
        ] {
            let geometry = TerminalGeometry::from_host(GridSize::clamped(80, 24), host);
            let bytes = crate::codec::to_vec(&geometry).expect("geometry encodes");
            let decoded: TerminalGeometry =
                crate::codec::from_slice_exact(&bytes).expect("geometry decodes");
            assert_eq!(decoded, geometry);
            assert_eq!(decoded.host_geometry().expect("accepted").cell(), host);
        }
    }

    #[test]
    fn received_oversized_cells_remain_raw_for_server_refusal() {
        let oversized = report(CellPx::MAX_DIMENSION + 1, 16);
        let geometry = TerminalGeometry {
            grid: GridSize::clamped(1, 1),
            cell: ReportedCell::Exact(oversized),
        };
        let bytes = crate::codec::to_vec(&geometry).expect("raw report encodes");
        let decoded: TerminalGeometry =
            crate::codec::from_slice_exact(&bytes).expect("raw report decodes");
        assert_eq!(decoded.cell, ReportedCell::Exact(oversized));
        assert_eq!(
            decoded.host_geometry(),
            Err(super::super::SurfaceRefusal::CellTooLarge)
        );
    }

    #[test]
    fn oversized_surfaces_are_refused_by_reason() {
        let dimension = TerminalGeometry::from_host(
            GridSize::clamped(super::super::MAX_SURFACE_DIMENSION + 1, 1),
            HostCell::Unknown,
        );
        assert_eq!(
            dimension.host_geometry(),
            Err(super::super::SurfaceRefusal::DimensionTooLarge)
        );
        let cells = TerminalGeometry::from_host(
            GridSize::clamped(
                super::super::MAX_SURFACE_DIMENSION,
                super::super::MAX_SURFACE_DIMENSION,
            ),
            HostCell::Unknown,
        );
        assert_eq!(
            cells.host_geometry(),
            Err(super::super::SurfaceRefusal::TooManyCells)
        );
    }
}
