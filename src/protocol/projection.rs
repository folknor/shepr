use super::*;
use serde::{Deserialize, Serialize};

/// Initial resource projection used by the client-owned shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellSnapshot {
    /// Changes whenever the endpoint process restarts.
    pub boot_id: String,
    /// Monotonic replacement revision within one endpoint boot.
    pub revision: u64,
    /// Endpoint's complete resolved configuration and provenance.
    pub resolved_config: crate::config::ValidatedConfig,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>"
    )]
    pub tab_bar_right: Vec<ClientShellTabStatusSegment>,
    pub tab_bar_right_separator: String,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>"
    )]
    pub workspaces: Vec<ClientShellWorkspace>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>"
    )]
    pub tabs: Vec<ClientShellTab>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>"
    )]
    pub panes: Vec<ClientShellPane>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>"
    )]
    pub agents: Vec<ClientShellAgent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellTabStatusSegment {
    pub text: String,
    pub accent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellWorkspace {
    pub workspace_id: String,
    pub active_tab_id: String,
    pub new_workspace_cwd: String,
    pub number: usize,
    pub label: String,
    pub custom_label: bool,
    pub branch: Option<String>,
    pub git_ahead_behind: Option<(usize, usize)>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>"
    )]
    pub tokens: Vec<(String, String)>,
    pub focused: bool,
    pub agent_status: crate::agent_status::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellTab {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub custom_label: bool,
    pub zoomed: bool,
    pub focused: bool,
    pub agent_status: crate::agent_status::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellPane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub focused: bool,
    pub right_click_passthrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellAgent {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub name: Option<String>,
    pub display_agent: Option<String>,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub agent_status: crate::agent_status::AgentStatus,
    pub state_change_seq: u64,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>"
    )]
    pub state_labels: Vec<(String, String)>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>"
    )]
    pub tokens: Vec<(String, String)>,
    pub focused: bool,
}
