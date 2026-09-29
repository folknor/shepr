use serde::{Deserialize, Serialize};
use shepr_protocol::{PublicPaneId, PublicTabId, WorkspaceId};

use super::common::{PaneAgentState, SplitDirection};

pub use shepr_protocol::command::{
    LayoutSetSplitRatioParams, PaneCopyMotion, PaneCopyMotionParams, PaneCopySearchDirection,
    PaneCopySearchParams, PaneDirection, PaneFocusDirectionParams, PaneInfo, PaneInputSetParams,
    PaneRenameParams, PaneResizeParams, PaneRightClickTarget, PaneScrollInfo, PaneScrollParams,
    PaneSelectionReadParams, PaneSplitParams, PaneSwapParams, PaneTextPoint, PaneTextRange,
    PaneZoomMode, PaneZoomParams,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    pub state: PaneAgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentSessionParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_start_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLayoutSnapshot {
    pub workspace_id: WorkspaceId,
    pub tab_id: PublicTabId,
    pub zoomed: bool,
    pub area: PaneLayoutRect,
    pub focused_pane_id: PublicPaneId,
    pub panes: Vec<PaneLayoutPane>,
    pub splits: Vec<PaneLayoutSplit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLayoutRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLayoutPane {
    pub pane_id: PublicPaneId,
    pub focused: bool,
    pub rect: PaneLayoutRect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLayoutSplit {
    pub id: String,
    pub direction: SplitDirection,
    pub ratio: f32,
    pub rect: PaneLayoutRect,
}
