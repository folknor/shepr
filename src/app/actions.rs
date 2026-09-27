//! Pure state mutations on AppState.
//! These don't need channels, async, or PTY runtime.

use std::time::Instant;

use tracing::debug;

use crate::core::layout::PaneId;
#[cfg(test)]
use crate::core::layout::{NavDirection, find_in_direction};
use crate::detect::{Agent, AgentState};
use crate::events::AppEvent;
use crate::git::WorkspaceGitStatus;
use crate::terminal::{EffectiveStateChange, TerminalStateMutation};
use crate::workspace::{
    PaneRemoval, PaneRemovalPlan as WorkspacePaneRemovalPlan, PaneRemovalScope, TabRemoval,
};

use super::state::{AppState, Mode, PaneFocusTarget};

fn public_tab_id_for_index(ws: &crate::workspace::Workspace, tab_idx: usize) -> Option<String> {
    let tab_number = ws.public_tab_number(tab_idx)?;
    Some(crate::workspace::public_tab_id_for_number(
        &ws.id, tab_number,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneStateSnapshot {
    pub agent_label: Option<String>,
    pub known_agent: Option<Agent>,
    pub state: AgentState,
    pub presentation: crate::terminal::EffectivePresentation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStateCause {
    StateChanged,
    NameChanged,
    Released,
    NameChangedAndReleased,
}

impl PaneStateCause {
    pub fn name_changed(self) -> bool {
        matches!(self, Self::NameChanged | Self::NameChangedAndReleased)
    }

    pub fn released(self) -> bool {
        matches!(self, Self::Released | Self::NameChangedAndReleased)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneStateUpdate {
    pub pane_id: PaneId,
    pub workspace_id: String,
    pub previous: PaneStateSnapshot,
    pub current: PaneStateSnapshot,
    pub cause: PaneStateCause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneRemovalPlan {
    pub(crate) workspace_index: usize,
    pub(crate) tab_index: usize,
    pub(crate) scope: PaneRemovalScope,
    workspace_plan: WorkspacePaneRemovalPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneRemovalOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) removal: PaneRemoval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneRemovalCommit {
    Removed(PaneRemovalOutcome),
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceRemovalOutcome {
    pub(crate) workspace_id: String,
    pub(crate) pane_ids: Vec<PaneId>,
    pub(crate) terminal_ids: Vec<crate::protocol::TerminalId>,
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
    workspace_id: String,
    tab_number: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabRemovalOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) scope: TabRemovalScope,
    pub(crate) pane_ids: Vec<PaneId>,
    pub(crate) terminal_ids: Vec<crate::protocol::TerminalId>,
    pub(crate) tab: Option<TabRemoval>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TabRemovalCommit {
    Removed(TabRemovalOutcome),
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceCreationOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) workspace_id: String,
    pub(crate) root_pane: Option<PaneId>,
    pub(crate) focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneCreationOutcome {
    pub(crate) workspace_index: usize,
    pub(crate) tab_index: usize,
    pub(crate) pane_id: PaneId,
    pub(crate) terminal_id: crate::protocol::TerminalId,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneZoomNoopReason {
    SinglePane,
    AlreadyZoomed,
    AlreadyUnzoomed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneZoomOutcome {
    pub changed: bool,
    pub focus_changed: bool,
    pub reason: Option<PaneZoomNoopReason>,
    pub zoomed: bool,
}

mod events;
mod focus;
mod pane;
mod workspace;

#[cfg(test)]
mod tests;
