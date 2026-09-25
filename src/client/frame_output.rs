use std::io;

use crate::protocol::FrameData;

/// Local output only. Shepr no longer forwards pane images to the outer
/// terminal, so this is a thin wrapper around the plain text frame.
#[derive(Debug)]
pub(crate) struct ComposedFrame {
    pub(crate) frame: FrameData,
}

impl From<FrameData> for ComposedFrame {
    fn from(frame: FrameData) -> Self {
        Self { frame }
    }
}

impl std::ops::Deref for ComposedFrame {
    type Target = FrameData;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

pub(super) fn write_composed_frame(mut writer: impl io::Write, encoded: &[u8]) -> io::Result<()> {
    writer.write_all(encoded)
}
