//! Pure state mutations on AppState.
//! These don't need channels, async, or PTY runtime.

use shepr_agent::ownership::{AgentOwnershipMutation, EffectiveStateChange};
use shepr_core::layout::PaneId;
use shepr_mux::events::AppEvent;
use shepr_mux::git::WorkspaceGitStatus;
use shepr_mux::workspace::{
    PaneRemoval, PaneRemovalPlan as WorkspacePaneRemovalPlan, PaneRemovalScope,
};

use super::state::AppState;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneRemovalPlan {
    pub(crate) workspace_index: usize,
    workspace_plan: WorkspacePaneRemovalPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneRemovalOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) removal: PaneRemoval,
    /// Terminals the removal detached from state; the caller shuts down
    /// their runtimes.
    pub(crate) detached_terminal_ids: Vec<shepr_protocol::TerminalId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a stale plan removed nothing; the caller must report it"]
pub(crate) enum PaneRemovalCommit {
    Removed(PaneRemovalOutcome),
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceRemovalOutcome {
    pub(crate) workspace_id: shepr_protocol::WorkspaceId,
    pub(crate) pane_ids: Vec<PaneId>,
    pub(crate) terminal_ids: Vec<shepr_protocol::TerminalId>,
    /// Terminals the removal detached from state; the caller shuts down
    /// their runtimes.
    pub(crate) detached_terminal_ids: Vec<shepr_protocol::TerminalId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceCreationOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) workspace_id: shepr_protocol::WorkspaceId,
    pub(crate) root_pane: PaneId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneCreationOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) pane_id: PaneId,
    pub(crate) terminal_id: shepr_protocol::TerminalId,
}

/// What a zoom toggle did: whether the workspace's zoom and the pane focus moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneZoomOutcome {
    pub changed: bool,
    pub focus_changed: bool,
}

mod events;
mod focus;
mod pane;
mod workspace;

#[cfg(test)]
mod tests;
