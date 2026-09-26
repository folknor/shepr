//! The outer host terminal's cell size in pixels, as reported by the XTWINOPS
//! `\x1b[16t` query. This is generic terminal geometry (used for pixel-accurate
//! pane resize reporting to child processes and mouse SGR-pixel coordinates),
//! not specific to any particular terminal graphics protocol.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct HostCellSize {
    pub width_px: u32,
    pub height_px: u32,
}

impl HostCellSize {
    pub(crate) fn is_known(&self) -> bool {
        self.width_px > 0 && self.height_px > 0
    }
}
