//! Pure state mutations on AppState.
//! These don't need channels, async, or PTY runtime.

use shepr_core::layout::PaneId;
use shepr_detect::ownership::{AgentOwnershipMutation, EffectiveStateChange};
use shepr_mux::events::RuntimeEvent;
use shepr_mux::git::WorkspaceGitStatus;
use shepr_mux::workspace::PaneRemovalScope;

use super::state::AppState;

/// Committed presentation changes shared by the small state mutators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewMutation {
    Unchanged,
    Metadata,
    Focus,
    Geometry,
    WorkspaceOrder,
    Swap { focus_changed: bool },
}

impl ViewMutation {
    pub(crate) fn changed(self) -> bool {
        self != Self::Unchanged
    }
}

/// What applying an event did to a terminal's effective agent state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateUpdate {
    /// The effective state did not change.
    Unchanged,
    /// The effective state changed.
    Changed,
    /// The agent was released from the terminal.
    Released,
}

/// A pane removed from its workspace; a workspace's last pane takes the
/// workspace with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneRemovalOutcome {
    pub(crate) workspace_id: shepr_protocol::WorkspaceId,
    pub(crate) pane_id: PaneId,
    pub(crate) scope: PaneRemovalScope,
    pub(crate) focus_changed: bool,
    /// The panes that left; the caller shuts down their runtimes.
    pub(crate) removed: Vec<PaneId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceRemovalOutcome {
    pub(crate) workspace_id: shepr_protocol::WorkspaceId,
    /// The panes that left; the caller shuts down their runtimes.
    pub(crate) removed: Vec<PaneId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceCreationOutcome {
    pub(crate) workspace_id: shepr_protocol::WorkspaceId,
    pub(crate) root_pane: PaneId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneCreationOutcome {
    pub(crate) workspace_id: shepr_protocol::WorkspaceId,
    pub(crate) pane_id: PaneId,
}

/// What a zoom toggle did: whether the workspace's zoom and the pane focus moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneZoomOutcome {
    pub(crate) changed: bool,
    pub(crate) focus_changed: bool,
}

mod events;
mod focus;
mod pane;
mod workspace;

#[cfg(test)]
mod tests;
