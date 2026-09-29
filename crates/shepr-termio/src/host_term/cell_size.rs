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
    pub fn is_known(&self) -> bool {
        shepr_core::geometry::CellPx::new(self.width_px, self.height_px).is_some()
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
