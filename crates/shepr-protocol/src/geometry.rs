//! Cell geometry at the client protocol boundary.

use serde::{Deserialize, Serialize};
use shepr_core::geometry::{BoundedGridSize, BoundedGridSizeError, CellPx, GridSize};

/// Coherent geometry carried by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "ReceivedTerminalGeometry",
    into = "ReceivedTerminalGeometry"
)]
pub struct TerminalGeometry {
    grid: GridSize,
    cell: Option<CellPx>,
    pixel_mouse: bool,
}

/// The single positional wire shape used for both directions.
#[derive(Serialize, Deserialize)]
struct ReceivedTerminalGeometry {
    grid: GridSize,
    cell: Option<CellPx>,
    pixel_mouse: bool,
}

impl From<TerminalGeometry> for ReceivedTerminalGeometry {
    fn from(geometry: TerminalGeometry) -> Self {
        Self {
            grid: geometry.grid,
            cell: geometry.cell,
            pixel_mouse: geometry.pixel_mouse,
        }
    }
}

impl TryFrom<ReceivedTerminalGeometry> for TerminalGeometry {
    type Error = &'static str;

    fn try_from(received: ReceivedTerminalGeometry) -> Result<Self, Self::Error> {
        if received.pixel_mouse && received.cell.is_none() {
            return Err("pixel mouse requires known cell geometry");
        }
        // Keep representable raw dimensions intact here. The server checks
        // them during the handshake so it can distinguish an oversized axis
        // from an excessive cell count in its refusal.
        Ok(Self {
            grid: received.grid,
            cell: received.cell,
            pixel_mouse: received.pixel_mouse,
        })
    }
}

impl TerminalGeometry {
    pub fn new(cols: u16, rows: u16, width: u32, height: u32, pixel_mouse: bool) -> Self {
        let cell = CellPx::new(width, height);
        Self {
            grid: GridSize::clamped(cols, rows),
            cell,
            pixel_mouse: pixel_mouse && cell.is_some(),
        }
    }

    /// Construct a report from already coherent host cell geometry.
    pub fn with_cell(grid: GridSize, cell: ProtocolCellSize) -> Self {
        Self {
            grid,
            cell: cell.cell(),
            pixel_mouse: cell.exact(),
        }
    }

    pub fn grid(self) -> GridSize {
        self.grid
    }

    pub fn cell(self) -> Option<CellPx> {
        self.cell
    }

    pub fn pixel_mouse(self) -> bool {
        self.pixel_mouse
    }

    pub fn cell_geometry(self) -> ProtocolCellSize {
        ProtocolCellSize::from_wire(self.width(), self.height(), self.pixel_mouse)
    }

    pub fn cols(self) -> u16 {
        self.grid.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.grid.rows.get()
    }

    pub fn width(self) -> u32 {
        self.cell.map_or(0, |cell| cell.width.get())
    }

    pub fn height(self) -> u32 {
        self.cell.map_or(0, |cell| cell.height.get())
    }

    pub fn surface_size(self) -> super::ClientSurfaceSize {
        super::ClientSurfaceSize {
            cols: self.cols(),
            rows: self.rows(),
        }
    }

    /// Validate this raw geometry against the retained-surface grid budget.
    ///
    /// Handshake decoding deliberately does not call this: the server must
    /// inspect the raw dimensions to return the appropriate refusal reason.
    pub fn bounded_grid(self) -> Result<BoundedGridSize, BoundedGridSizeError> {
        self.grid.try_into()
    }
}

/// The shared host-cell policy, including the pixel bound and exactness.
pub type ProtocolCellSize = shepr_core::geometry::HostCellGeometry;

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct WireGridSize {
        cols: u16,
        rows: u16,
    }

    #[derive(Serialize)]
    struct WireReceivedGeometry {
        grid: WireGridSize,
        cell: Option<CellPx>,
        pixel_mouse: bool,
    }

    fn decode_received_geometry(
        cols: u16,
        rows: u16,
        cell: Option<CellPx>,
        pixel_mouse: bool,
    ) -> Result<TerminalGeometry, crate::codec::CodecError> {
        let bytes = crate::codec::to_vec(&WireReceivedGeometry {
            grid: WireGridSize { cols, rows },
            cell,
            pixel_mouse,
        })?;
        crate::codec::from_slice_exact(&bytes)
    }

    #[test]
    fn clamp_clears_exactness_and_preserves_unknown() {
        assert_eq!(ProtocolCellSize::from_host(8, 0, true).cell(), None);
        assert!(!ProtocolCellSize::from_host(8, 0, true).exact());
        let size = ProtocolCellSize::from_host(super::super::MAX_CELL_SIZE_PX + 1, 16, true);
        assert_eq!(size.width(), super::super::MAX_CELL_SIZE_PX);
        assert!(!size.exact());
        let invalid_wire = ProtocolCellSize::from_wire(u32::MAX, 16, true);
        assert_eq!((invalid_wire.width(), invalid_wire.height()), (0, 0));
        assert!(!invalid_wire.exact());
    }

    #[test]
    fn received_geometry_rejects_pixel_mouse_without_cells() {
        assert!(decode_received_geometry(80, 24, None, true).is_err());
        assert!(decode_received_geometry(0, 24, None, false).is_err());
    }

    #[test]
    fn received_oversized_cells_remain_raw_for_server_refusal() {
        let cell = CellPx::new(CellPx::MAX_DIMENSION + 1, 16);
        let geometry = decode_received_geometry(1, 1, cell, true).expect("raw report decodes");
        assert_eq!(geometry.cell(), cell);
        assert_eq!(geometry.grid(), GridSize::clamped(1, 1));
        assert_eq!(geometry.cell_geometry().cell(), None);
        assert!(!geometry.cell_geometry().exact());
    }

    #[test]
    fn terminal_geometry_uses_its_shared_positional_wire_shape() {
        let geometry = TerminalGeometry::new(80, 24, 8, 16, true);
        let fields = (geometry.grid, geometry.cell, geometry.pixel_mouse);
        let geometry_bytes = crate::codec::to_vec(&geometry).expect("geometry should encode");
        let field_bytes = crate::codec::to_vec(&fields).expect("geometry fields should encode");
        assert_eq!(geometry_bytes, field_bytes);
    }
}
