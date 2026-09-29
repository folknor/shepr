use serde::{Deserialize, Serialize};
use shepr_protocol::{PublicPaneId, PublicTabId, TerminalId, WorkspaceId};

use super::common::AgentStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub terminal_id: TerminalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title_stripped: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub screen_detection_skipped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<AgentSessionInfo>,
    pub workspace_id: WorkspaceId,
    pub tab_id: PublicTabId,
    pub pane_id: PublicPaneId,
    pub focused: bool,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    pub kind: shepr_agent::agent::resume::AgentSessionRefKind,
    pub value: String,
}
