use super::*;
use serde::{Deserialize, Serialize};

/// Initial resource projection used by the client-owned shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellSnapshot {
    /// Changes whenever the endpoint process restarts.
    pub boot_id: BootId,
    /// Monotonic replacement revision within one endpoint boot.
    pub revision: ProjectionRevision,
    /// Positional encoding of the endpoint's resolved configuration and provenance.
    /// Present on the first snapshot of a connection; empty later means reuse
    /// that connection's validated config.
    #[serde(
        serialize_with = "codec::serialize_bounded_bytes::<MAX_FRAME_SIZE, _>",
        deserialize_with = "codec::deserialize_bounded_bytes::<MAX_FRAME_SIZE, _>"
    )]
    pub resolved_config: Vec<u8>,
    pub focused_workspace_id: Option<WorkspaceId>,
    pub focused_tab_id: Option<PublicTabId>,
    pub focused_pane_id: Option<PublicPaneId>,
    pub tab_bar_right: Vec<ClientShellTabStatusSegment>,
    pub tab_bar_right_separator: String,
    pub workspaces: Vec<ClientShellWorkspace>,
    pub tabs: Vec<ClientShellTab>,
    pub panes: Vec<ClientShellPane>,
    pub agents: Vec<ClientShellAgent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellTabStatusSegment {
    pub text: String,
    pub accent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellWorkspace {
    pub workspace_id: WorkspaceId,
    pub active_tab_id: PublicTabId,
    pub new_workspace_cwd: String,
    pub number: usize,
    pub label: String,
    pub custom_label: bool,
    pub branch: Option<String>,
    pub git_ahead_behind: Option<(usize, usize)>,
    pub focused: bool,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellTab {
    pub tab_id: PublicTabId,
    pub workspace_id: WorkspaceId,
    pub number: usize,
    pub label: String,
    pub custom_label: bool,
    pub zoomed: bool,
    pub focused: bool,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellPane {
    pub pane_id: PublicPaneId,
    pub workspace_id: WorkspaceId,
    pub tab_id: PublicTabId,
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub focused: bool,
    pub right_click_passthrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellAgent {
    pub pane_id: PublicPaneId,
    pub workspace_id: WorkspaceId,
    pub tab_id: PublicTabId,
    pub name: Option<String>,
    pub agent: Option<String>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub agent_status: AgentStatus,
    pub state_change_seq: u64,
    pub focused: bool,
}
