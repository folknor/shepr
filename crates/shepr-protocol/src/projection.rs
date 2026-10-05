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
    /// The server stopped saving its session for the rest of this boot (its
    /// persister refused a save it can never run): layout changes from then
    /// on are not restored when it next starts. Repeated on every projection.
    pub session_saves_stopped: bool,
    /// The saved session source could not be opened for its required backup.
    /// The server stops saving until its access is fixed and it is restarted.
    /// Repeated on every projection.
    pub session_saves_blocked_on_backup: bool,
    pub focused_workspace_id: Option<WorkspaceId>,
    pub focused_pane_id: Option<PublicPaneId>,
    /// Ordered workspaces. The client derives one-based display positions
    /// from this order; the stable ID's allocator number is unrelated.
    pub workspaces: Vec<ClientShellWorkspace>,
    pub panes: Vec<ClientShellPane>,
    pub agents: Vec<ClientShellAgent>,
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
    pub label: Option<String>,
    pub cwd: Option<RemotePath>,
    pub foreground_cwd: Option<RemotePath>,
    pub right_click_passthrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellAgent {
    pub pane_id: PublicPaneId,
    /// The bundled agent the pane runs, if any. The server accepts hook
    /// reports only from shepr's own integrations, each of which names a
    /// bundled agent, so there is no free-label agent to project.
    pub agent: Option<shepr_agent::Agent>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub agent_status: AgentStatus,
    pub state_change_seq: StateChangeSeq,
}
