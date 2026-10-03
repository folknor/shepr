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
