//! Cell geometry at the client protocol boundary.

use serde::{Deserialize, Serialize};
use shepr_core::geometry::{CellPx, GridSize};

/// Coherent geometry carried by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "ReceivedTerminalGeometry",
    into = "ReceivedTerminalGeometry"
)]
pub struct TerminalGeometry {
    pub grid: GridSize,
    pub cell: Option<CellPx>,
    pub pixel_mouse: bool,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolCellSize {
    pub cell: Option<CellPx>,
    pub exact: bool,
}

impl ProtocolCellSize {
    /// Unknown dimensions stay absent. Clamping a reported size loses
    /// exactness, so it must also disable pixel mouse coordinates.
    pub fn from_host(width: u32, height: u32, exact: bool) -> Self {
        let cell = CellPx::new(width, height);
        let within_limit = width <= super::MAX_CELL_SIZE_PX && height <= super::MAX_CELL_SIZE_PX;
        Self {
            cell: cell.and_then(|_| {
                CellPx::new(
                    width.min(super::MAX_CELL_SIZE_PX),
                    height.min(super::MAX_CELL_SIZE_PX),
                )
            }),
            exact: exact && within_limit && cell.is_some(),
        }
    }

    pub fn width(self) -> u32 {
        self.cell.map_or(0, |cell| cell.width.get())
    }

    pub fn height(self) -> u32 {
        self.cell.map_or(0, |cell| cell.height.get())
    }

    /// A peer's out-of-range pixel report is unusable rather than clamped:
    /// clamping here would claim a geometry the peer did not send. The
    /// server's handshake and resize paths already refuse an oversized
    /// report before calling this, but the bound stays here too: it is this
    /// type's own invariant, and a public constructor that relied on every
    /// caller checking first would let the next caller build an out-of-range
    /// cell size.
    pub fn from_wire(width: u32, height: u32, exact: bool) -> Self {
        let cell = if width <= super::MAX_CELL_SIZE_PX && height <= super::MAX_CELL_SIZE_PX {
            CellPx::new(width, height)
        } else {
            None
        };
        Self {
            cell,
            exact: exact && cell.is_some(),
        }
    }
}

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
        assert_eq!(ProtocolCellSize::from_host(8, 0, true).cell, None);
        assert!(!ProtocolCellSize::from_host(8, 0, true).exact);
        let size = ProtocolCellSize::from_host(super::super::MAX_CELL_SIZE_PX + 1, 16, true);
        assert_eq!(size.width(), super::super::MAX_CELL_SIZE_PX);
        assert!(!size.exact);
        let invalid_wire = ProtocolCellSize::from_wire(u32::MAX, 16, true);
        assert_eq!((invalid_wire.width(), invalid_wire.height()), (0, 0));
        assert!(!invalid_wire.exact);
    }

    #[test]
    fn received_geometry_rejects_pixel_mouse_without_cells() {
        assert!(decode_received_geometry(80, 24, None, true).is_err());
        assert!(decode_received_geometry(0, 24, None, false).is_err());
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
