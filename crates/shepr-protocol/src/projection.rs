use super::*;
use serde::{Deserialize, Serialize};

/// The agent state-change order the projection carries, re-exported so a
/// consumer of the wire type does not link the agent crate for it.
pub use shepr_agent::StateChangeSeq;

/// Initial resource projection used by the client-owned shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellSnapshot {
    /// Changes whenever the endpoint process restarts.
    pub boot_id: BootId,
    /// Monotonic replacement revision within one endpoint boot.
    pub revision: ProjectionRevision,
    /// Incomplete saved-session restore for this boot, repeated on every projection.
    pub restore_notice: Option<SessionRestoreNotice>,
    /// Whether session saves are ready, stopped, or blocked on a recovery
    /// backup. Repeated on every projection.
    pub session_save_status: SessionSaveStatus,
    /// The endpoint host's home directory, against which the client shortens
    /// the paths it shows.
    pub home_dir: Option<RemotePath>,
    pub focused_workspace_id: Option<WorkspaceId>,
    pub focused_pane_id: Option<PublicPaneId>,
    /// Ordered workspaces. The client derives one-based display positions
    /// from this order; the stable ID's allocator number is unrelated.
    pub workspaces: Vec<ClientShellWorkspace>,
    pub panes: Vec<ClientShellPane>,
    pub agents: Vec<ClientShellAgent>,
}

/// The mutually exclusive session-save condition projected by a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSaveStatus {
    /// Session saves can proceed normally.
    Ready,
    /// The persister refused a save it cannot run; layout changes from then
    /// on are not restored when the server next starts.
    Stopped,
    /// The saved session source could not be opened for its required backup;
    /// saving remains blocked until its access is fixed and the server restarts.
    BlockedOnBackup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellWorkspace {
    pub workspace_id: WorkspaceId,
    pub new_workspace_cwd: Option<RemotePath>,
    pub label: String,
    pub branch: Option<String>,
    /// The pair stays compact at the projection boundary; the sidebar is the only reader.
    pub git_ahead_behind: Option<(usize, usize)>,
    pub agent_status: AgentStatus,
    /// Whether the workspace shows only its focused pane. Panes are listed in
    /// `ClientShellSnapshot::panes` whatever the zoom, so the client counts a
    /// workspace's panes there.
    pub zoomed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellPane {
    pub pane_id: PublicPaneId,
    /// The name the user gave the pane, the one rename edits.
    pub label: Option<String>,
    /// The name of the directory the pane's current agent runs in.
    pub agent_label: Option<String>,
    pub cwd: Option<RemotePath>,
    pub foreground_cwd: Option<RemotePath>,
    pub right_click_passthrough: bool,
}

impl ClientShellPane {
    /// The name the pane is shown under
    /// (`shepr_core::pane_label::pane_display_label`).
    pub fn display_label(&self) -> Option<std::borrow::Cow<'_, str>> {
        shepr_core::pane_label::pane_display_label(
            self.label.as_deref(),
            self.agent_label.as_deref(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellAgent {
    pub pane_id: PublicPaneId,
    /// The bundled agent the pane runs. The server accepts hook reports only
    /// from shepr's own integrations, each of which names a bundled agent.
    pub agent: shepr_agent::Agent,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub agent_status: AgentStatus,
    pub state_change_seq: StateChangeSeq,
}
