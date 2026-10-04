//! The input mode, with the Navigate workspace preview living only inside Navigate.

use crate::shell::navigation::location::PinnedLocation;
use crate::shell::state::ClientShellMode;

/// This is the input-mode authority. A copy session can be parked while its pane
/// stays focused, so focus alone cannot say whether copy input is active. Navigation
/// stays active when no workspace target exists, so its preview is optional.
#[derive(Debug)]
pub(in crate::shell) struct ModeState {
    kind: ClientShellMode,
    /// Always `None` outside Navigate.
    preview: Option<PinnedLocation>,
}

impl Default for ModeState {
    fn default() -> Self {
        Self {
            kind: ClientShellMode::Terminal,
            preview: None,
        }
    }
}

impl ModeState {
    pub(in crate::shell) fn kind(&self) -> ClientShellMode {
        self.kind
    }

    pub(in crate::shell) fn is(&self, kind: ClientShellMode) -> bool {
        self.kind == kind
    }

    /// Any mode but Navigate; drops the preview.
    pub(in crate::shell) fn set(&mut self, kind: ClientShellMode) {
        if kind == ClientShellMode::Navigate {
            // Navigate is entered through `enter_navigate`; a caller that
            // got here enters it with no preview.
            self.enter_navigate(None);
            return;
        }
        self.kind = kind;
        self.preview = None;
    }

    pub(in crate::shell) fn enter_navigate(&mut self, preview: Option<PinnedLocation>) {
        self.kind = ClientShellMode::Navigate;
        self.preview = preview;
    }

    pub(in crate::shell) fn preview(&self) -> Option<&PinnedLocation> {
        self.preview.as_ref()
    }

    /// Navigate only: outside it the preview stays absent.
    pub(in crate::shell) fn set_preview(&mut self, preview: Option<PinnedLocation>) {
        // Outside Navigate a preview does not exist, so it is dropped.
        if self.kind == ClientShellMode::Navigate {
            self.preview = preview;
        }
    }

    pub(in crate::shell) fn take_preview(&mut self) -> Option<PinnedLocation> {
        self.preview.take()
    }

    /// Fills an empty Navigate preview; no-op outside Navigate or with a preview.
    #[cfg(test)]
    pub(in crate::shell) fn fill_preview(
        &mut self,
        preview: impl FnOnce() -> Option<PinnedLocation>,
    ) {
        if self.kind == ClientShellMode::Navigate && self.preview.is_none() {
            self.preview = preview();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::ClientEndpointId;
    use crate::shell::navigation::location::Location;
    use crate::tests::{test_boot_id, test_workspace_id};

    fn pinned(id: &str) -> PinnedLocation {
        PinnedLocation::new(
            Location::workspace(ClientEndpointId::Local, test_workspace_id(id)),
            test_boot_id("boot"),
            shepr_protocol::ConnectionGeneration::FIRST,
        )
    }

    #[test]
    fn leaving_navigate_drops_the_preview() {
        let mut mode = ModeState::default();
        mode.enter_navigate(Some(pinned("w1")));
        assert!(mode.preview().is_some());
        mode.set(ClientShellMode::Terminal);
        assert_eq!(mode.kind(), ClientShellMode::Terminal);
        assert!(mode.preview().is_none());
        mode.set_preview(None);
        assert!(mode.preview().is_none());
    }

    #[test]
    fn fill_preview_only_fills_an_empty_navigate_preview() {
        let mut mode = ModeState::default();
        mode.fill_preview(|| Some(pinned("w1")));
        assert!(mode.preview().is_none());

        mode.enter_navigate(None);
        mode.fill_preview(|| Some(pinned("w1")));
        assert_eq!(mode.preview(), Some(&pinned("w1")));

        mode.fill_preview(|| Some(pinned("w2")));
        assert_eq!(mode.preview(), Some(&pinned("w1")));
    }
}
