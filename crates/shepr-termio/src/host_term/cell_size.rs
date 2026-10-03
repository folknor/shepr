//! The outer host terminal's cell size in pixels, as reported by the XTWINOPS
//! `\x1b[16t` query. This is generic terminal geometry (used for pixel-accurate
//! pane resize reporting to child processes and mouse SGR-pixel coordinates),
//! not specific to any particular terminal graphics protocol.

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
