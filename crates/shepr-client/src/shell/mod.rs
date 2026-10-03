mod input;
mod navigation;
mod overlays;
mod presentation;
mod sidebar;

mod endpoints;
pub(crate) use endpoints::ClientEndpointFocusTarget;
mod ledger;
pub(crate) use ledger::DropReason;

mod state;

pub(crate) use overlays::endpoint_notices::{EndpointNotice, EndpointNoticeKind};
pub(crate) use presentation::surface_patch::{
    ClientComposedSurfacePatch, ClientPaneSurfacePatchOutcome, PatchPresentation,
};
pub(crate) use state::{
    ClientPresentationLogContext, ClientShellAction, ClientShellEndpointError,
    ClientShellEndpointRequest, ClientShellInput, ClientShellRequest, Repaint,
};
pub use state::{ClientShellConfig, ClientShellState};

#[cfg(test)]
mod tests;

#[cfg(test)]
impl ClientShellState {
    /// A full surface from whichever connection the shown snapshot came from.
    /// Production names the connection (`receive_pane_surface_from`).
    pub(crate) fn receive_pane_surface(&mut self, surface: shepr_protocol::PaneSurfaceFrame) {
        self.receive_tagged_pane_surface(surface, self.active_snapshot_generation);
    }

    /// A patch from whichever connection the shown snapshot came from.
    /// Production names the connection (`apply_pane_surface_patch_from`).
    pub(crate) fn apply_pane_surface_patch(
        &mut self,
        patch: &shepr_protocol::PaneSurfacePatch,
    ) -> ClientPaneSurfacePatchOutcome {
        self.apply_tagged_pane_surface_patch(patch, self.active_snapshot_generation)
    }
}
