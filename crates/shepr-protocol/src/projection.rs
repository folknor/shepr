use super::*;
use serde::{Deserialize, Serialize};

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
    pub new_workspace_cwd: String,
    pub label: String,
    pub branch: Option<String>,
    pub git_ahead_behind: Option<(usize, usize)>,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellPane {
    pub pane_id: PublicPaneId,
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub right_click_passthrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellAgent {
    pub pane_id: PublicPaneId,
    /// The bundled agent the pane runs, if any. A hook report from a custom
    /// source with a free label (one that names no bundled agent) still
    /// drives the pane's state, but the TUI presents that pane without an
    /// agent name. This is deliberate: shepr installs only its own
    /// integrations, so a custom reporter is never something it ships, and a
    /// `Known(Agent) | Custom(String)` projection or a separate custom-label
    /// field would bring an open string back to the wire and to the sidebar's
    /// per-agent row lookup for a source the owner does not use.
    pub agent: Option<shepr_agent::agent::Agent>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub agent_status: AgentStatus,
    pub state_change_seq: u64,
}
