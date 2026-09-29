use serde::{Deserialize, Serialize};
use shepr_protocol::{PublicPaneId, PublicTabId, WorkspaceId};

use super::agents::AgentInfo;
use super::panes::{PaneInfo, PaneLayoutSnapshot};
use super::tabs::TabInfo;
use super::workspaces::WorkspaceInfo;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_tab_id: Option<PublicTabId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_id: Option<PublicPaneId>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub panes: Vec<PaneInfo>,
    pub layouts: Vec<PaneLayoutSnapshot>,
    pub agents: Vec<AgentInfo>,
}
