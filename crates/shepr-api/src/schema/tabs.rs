use serde::{Deserialize, Serialize};
use shepr_protocol::{PublicTabId, WorkspaceId};

use super::common::AgentStatus;

pub use shepr_protocol::command::{TabCreateParams, TabMoveParams, TabRenameParams};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub tab_id: PublicTabId,
    pub workspace_id: WorkspaceId,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub agent_status: AgentStatus,
}
