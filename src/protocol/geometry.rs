//! Cell geometry at the client protocol boundary.

use serde::{Deserialize, Serialize};
use shepr_core::geometry::{CellPx, GridSize};

/// Coherent geometry carried by a direct terminal client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ReceivedTerminalGeometry")]
pub struct TerminalGeometry {
    pub grid: GridSize,
    pub cell: Option<CellPx>,
    pub pixel_mouse: bool,
}

#[derive(Deserialize)]
struct ReceivedTerminalGeometry {
    grid: GridSize,
    cell: Option<CellPx>,
    pixel_mouse: bool,
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
    pub(crate) fn new(cols: u16, rows: u16, width: u32, height: u32, pixel_mouse: bool) -> Self {
        let cell = CellPx::new(width, height);
        Self {
            grid: GridSize::clamped(cols, rows),
            cell,
            pixel_mouse: pixel_mouse && cell.is_some(),
        }
    }

    pub(crate) fn cols(self) -> u16 {
        self.grid.cols.get()
    }

    pub(crate) fn rows(self) -> u16 {
        self.grid.rows.get()
    }

    pub(crate) fn width(self) -> u32 {
        self.cell.map_or(0, |cell| cell.width.get())
    }

    pub(crate) fn height(self) -> u32 {
        self.cell.map_or(0, |cell| cell.height.get())
    }

    pub(crate) fn surface_size(self) -> super::ClientSurfaceSize {
        super::ClientSurfaceSize {
            cols: self.cols(),
            rows: self.rows(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProtocolCellSize {
    pub cell: Option<CellPx>,
    pub exact: bool,
}

impl ProtocolCellSize {
    /// Unknown dimensions stay absent. Clamping a reported size loses
    /// exactness, so it must also disable pixel mouse coordinates.
    pub(crate) fn from_host(width: u32, height: u32, exact: bool) -> Self {
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

    pub(crate) fn width(self) -> u32 {
        self.cell.map_or(0, |cell| cell.width.get())
    }

    pub(crate) fn height(self) -> u32 {
        self.cell.map_or(0, |cell| cell.height.get())
    }

    /// A peer's out-of-range pixel report is unusable rather than clamped:
    /// clamping here would claim a geometry the peer did not send.
    pub(crate) fn from_wire(width: u32, height: u32, exact: bool) -> Self {
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
        let invalid = serde_json::json!({
            "grid": { "cols": 80, "rows": 24 },
            "cell": null,
            "pixel_mouse": true,
        });
        assert!(serde_json::from_value::<TerminalGeometry>(invalid).is_err());
        let empty_grid = serde_json::json!({
            "grid": { "cols": 0, "rows": 24 },
            "cell": null,
            "pixel_mouse": false,
        });
        assert!(serde_json::from_value::<TerminalGeometry>(empty_grid).is_err());
    }
}
