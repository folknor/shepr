mod config;
mod copy;
mod input;
mod navigation;
mod notices;
mod overlays;
mod presentation;
mod sidebar;
mod view;

mod endpoints;
pub use navigation::location::{Location, LocationTarget};
mod ledger;
pub(crate) use ledger::{ClientShellEndpointRequest, DropReason};
mod mode;

mod state;
mod transitions;

pub use config::ClientShellConfig;
pub(crate) use notices::cards::{EndpointNotice, EndpointNoticeKind};
pub(crate) use presentation::surface_patch::{
    ClientComposedSurfacePatch, ClientPaneSurfacePatchOutcome, PatchPresentation,
};
pub use state::ClientShellState;
pub(crate) use state::{
    ClientPresentationLogContext, ClientShellAction, ClientShellEndpointError, ClientShellInput,
    ClientShellRequest, Repaint,
};
pub(crate) use view::ComposedFrame;

#[cfg(test)]
mod tests;
