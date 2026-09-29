//! Pure state mutations on AppState.
//! These don't need channels, async, or PTY runtime.

use shepr_agent::detect::AgentState;
use shepr_core::layout::PaneId;
use shepr_mux::events::AppEvent;
use shepr_mux::git::WorkspaceGitStatus;
use shepr_mux::terminal::{EffectiveStateChange, TerminalStateMutation};
use shepr_mux::workspace::{
    PaneRemoval, PaneRemovalPlan as WorkspacePaneRemovalPlan, PaneRemovalScope, TabRemoval,
};

use super::state::{AppState, Mode, PaneFocusTarget};

fn public_tab_id_for_index(
    ws: &shepr_mux::workspace::Workspace,
    tab_idx: usize,
) -> Option<shepr_protocol::PublicTabId> {
    let tab_number = ws.public_tab_number(tab_idx)?;
    Some(shepr_protocol::PublicTabId::new(&ws.id, tab_number))
}

/// What applying an event did to a terminal's effective agent state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateUpdate {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabRemovalScope {
    Tab,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabRemovalPlan {
    pub(crate) workspace_index: usize,
    pub(crate) tab_index: usize,
    pub(crate) scope: TabRemovalScope,
    workspace_id: shepr_protocol::WorkspaceId,
    tab_number: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabRemovalOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) scope: TabRemovalScope,
    pub(crate) pane_ids: Vec<PaneId>,
    pub(crate) terminal_ids: Vec<shepr_protocol::TerminalId>,
    /// Terminals the removal detached from state; the caller shuts down
    /// their runtimes.
    pub(crate) detached_terminal_ids: Vec<shepr_protocol::TerminalId>,
    pub(crate) tab: Option<TabRemoval>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a stale plan removed nothing; the caller must report it"]
pub(crate) enum TabRemovalCommit {
    Removed(TabRemovalOutcome),
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceCreationOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) workspace_id: shepr_protocol::WorkspaceId,
    pub(crate) root_pane: PaneId,
    pub(crate) focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneCreationOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) tab_index: usize,
    pub(crate) pane_id: PaneId,
    pub(crate) terminal_id: shepr_protocol::TerminalId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneContextFallback {
    None,
    ActiveWorkspace,
    WorkspaceCreation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneContext {
    pub(crate) workspace_index: usize,
    pub(crate) tab_index: usize,
    pub(crate) pane_id: PaneId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneZoomCommand {
    Toggle,
    On,
    Off,
}

/// What a zoom command did: whether the tab's zoom and the pane focus moved.
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
use shepr_core::layout::{NavDirection, find_in_direction};

#[cfg(test)]
mod tests;
