use serde::{Deserialize, Serialize};

use shepr_protocol::PublicPaneId;

use super::events::EventEnvelope;
use super::panes::{
    LayoutDescription, PaneFocusDirectionResult, PaneInfo, PaneResizeResult, PaneSwapResult,
    PaneTextPoint, PaneTextRange, PaneZoomResult,
};
use super::server::ServerCapabilities;
use super::session::SessionSnapshot;
use super::tabs::TabInfo;
use super::workspaces::WorkspaceInfo;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessResponse {
    pub id: String,
    pub result: ResponseResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub id: String,
    pub error: ErrorBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &crate::error::ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_owned(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseResult {
    Pong {
        version: String,
        build_id: String,
        capabilities: Option<ServerCapabilities>,
    },
    SessionSnapshot {
        snapshot: Box<SessionSnapshot>,
    },
    WorkspaceInfo {
        workspace: WorkspaceInfo,
    },
    WorkspaceCreated {
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
    },
    WorkspaceList {
        workspaces: Vec<WorkspaceInfo>,
    },
    TabInfo {
        tab: TabInfo,
    },
    TabCreated {
        tab: TabInfo,
        root_pane: PaneInfo,
    },
    TabList {
        tabs: Vec<TabInfo>,
    },
    PaneInfo {
        pane: PaneInfo,
    },
    PaneSwap {
        swap: PaneSwapResult,
    },
    PaneZoom {
        zoom: PaneZoomResult,
    },
    LayoutExport {
        layout: LayoutDescription,
    },
    LayoutApply {
        layout: LayoutDescription,
    },
    LayoutSplitRatioSet {
        layout: LayoutDescription,
    },
    PaneFocusDirection {
        focus: PaneFocusDirectionResult,
    },
    PaneResize {
        resize: PaneResizeResult,
    },
    PaneSelection {
        pane_id: String,
        text: String,
    },
    PaneCopyMotion {
        pane_id: String,
        cursor: PaneTextPoint,
        content_revision: u64,
    },
    PaneCopySearch {
        pane_id: String,
        content_revision: u64,
        matches: Vec<PaneTextRange>,
        total: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_global: Option<u64>,
    },
    /// The detector's input for one pane: the detection-source screen text.
    DetectCapture {
        pane_id: PublicPaneId,
        text: String,
    },
    DetectExplain {
        explain: serde_json::Value,
    },
    SubscriptionStarted {},
    WaitMatched {
        event: EventEnvelope,
    },
    /// Acknowledgement for the client-shell surface interest lease. Its revision-bearing
    /// result can establish an activation floor.
    ClientShellSurfaceSet {
        active: bool,
        projection_revision: u64,
    },
    Ok {},
}
