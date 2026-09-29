//! Typed endpoint operations a client shell sends over the client socket, and
//! the replies it gets back.
//!
//! These are the one definition of the parameter and result types they carry.
//! The JSON API in `shepr-api` re-exports them for its methods of the same
//! names, so a client-shell command converts to and from its API method by a
//! move, and neither end of the client socket does a JSON pass. The types are
//! positional wire types: no field is skipped or flattened, so an absent
//! `Option` is written to JSON as `null`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use shepr_core::agent_session::AgentSessionRefKind;

use crate::{AgentStatus, PublicPaneId, PublicTabId, TerminalId, WorkspaceId};

/// Updates whether the requesting client shell receives and controls pane presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellSurfaceSetParams {
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTarget {
    pub workspace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTarget {
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabTarget {
    pub tab_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreateParams {
    /// Workspace whose focused pane supplies the `follow` cwd policy.
    #[serde(default)]
    pub source_workspace_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCloseParams {
    pub workspace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRenameParams {
    pub workspace_id: String,
    pub label: String,
}

/// Asks the server for the Git checkout root of a directory on the server's
/// own host, which is what a new workspace's default label derives from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCheckoutRootParams {
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMoveParams {
    pub workspace_id: String,
    pub insert_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: WorkspaceId,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub tab_count: usize,
    pub active_tab_id: PublicTabId,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCreateParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabRenameParams {
    pub tab_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabMoveParams {
    pub tab_id: String,
    pub insert_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaneRightClickTarget {
    #[default]
    Shepr,
    Pane,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSplitParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub target_pane_id: Option<String>,
    pub direction: SplitDirection,
    #[serde(default)]
    pub ratio: Option<f32>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default)]
    pub right_click: PaneRightClickTarget,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInputSetParams {
    pub pane_id: String,
    pub right_click: PaneRightClickTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaneSwapParams {
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub direction: Option<PaneDirection>,
    #[serde(default)]
    pub source_pane_id: Option<String>,
    #[serde(default)]
    pub target_pane_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaneZoomParams {
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub mode: PaneZoomMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaneZoomMode {
    #[default]
    Toggle,
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutSetSplitRatioParams {
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub pane_id: Option<String>,
    pub path: Vec<bool>,
    pub ratio: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneFocusDirectionParams {
    #[serde(default)]
    pub pane_id: Option<String>,
    pub direction: PaneDirection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneResizeParams {
    #[serde(default)]
    pub pane_id: Option<String>,
    pub direction: PaneDirection,
    #[serde(default)]
    pub amount: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneScrollParams {
    pub pane_id: String,
    pub offset_from_bottom: u64,
}

/// A terminal cell addressed by a stable absolute row: output and history
/// eviction never make it name another line. Selections, copy-mode cursors
/// and search matches all use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTextPoint {
    pub row: shepr_vt::AbsRow,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTextRange {
    pub start: PaneTextPoint,
    pub end: PaneTextPoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSelectionReadParams {
    pub pane_id: String,
    pub anchor: PaneTextPoint,
    pub cursor: PaneTextPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneCopyMotion {
    LineEnd,
    FirstNonBlank,
    NextWordStart,
    PreviousWordStart,
    NextWordEnd,
    NextBigWordStart,
    PreviousBigWordStart,
    NextBigWordEnd,
    PreviousParagraph,
    NextParagraph,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCopyMotionParams {
    pub pane_id: String,
    pub cursor: PaneTextPoint,
    pub motion: PaneCopyMotion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneCopySearchDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCopySearchParams {
    pub pane_id: String,
    pub query: String,
    pub direction: PaneCopySearchDirection,
    pub cursor: PaneTextPoint,
    #[serde(default)]
    pub previous: Option<PaneTextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRenameParams {
    pub pane_id: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    pub kind: AgentSessionRefKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneScrollInfo {
    pub offset_from_bottom: u64,
    pub max_offset_from_bottom: u64,
    pub viewport_rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: PublicPaneId,
    pub terminal_id: TerminalId,
    pub workspace_id: WorkspaceId,
    pub tab_id: PublicTabId,
    pub focused: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub restore_error: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub terminal_title: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
    #[serde(default)]
    pub scroll: Option<PaneScrollInfo>,
}

/// One endpoint operation a client shell asks of the server it is connected
/// to. Each variant is the API method of the same name; nothing outside this
/// set can be asked through a client shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EndpointCommand {
    ClientShellSurfaceSet(ClientShellSurfaceSetParams),
    WorkspaceCreate(WorkspaceCreateParams),
    WorkspaceFocus(WorkspaceTarget),
    WorkspaceRename(WorkspaceRenameParams),
    WorkspaceCheckoutRoot(WorkspaceCheckoutRootParams),
    WorkspaceMove(WorkspaceMoveParams),
    WorkspaceClose(WorkspaceCloseParams),
    TabCreate(TabCreateParams),
    TabFocus(TabTarget),
    TabRename(TabRenameParams),
    TabMove(TabMoveParams),
    TabClose(TabTarget),
    PaneSplit(PaneSplitParams),
    PaneSwap(PaneSwapParams),
    PaneZoom(PaneZoomParams),
    LayoutSetSplitRatio(LayoutSetSplitRatioParams),
    PaneFocusDirection(PaneFocusDirectionParams),
    PaneResize(PaneResizeParams),
    PaneScroll(PaneScrollParams),
    PaneClear(PaneTarget),
    PaneSelectionRead(PaneSelectionReadParams),
    PaneCopyMotion(PaneCopyMotionParams),
    PaneCopySearch(PaneCopySearchParams),
    PaneFocus(PaneTarget),
    PaneInputSet(PaneInputSetParams),
    PaneRename(PaneRenameParams),
    PaneClose(PaneTarget),
}

/// The successful result of an [`EndpointCommand`], carrying what the client
/// shell reads. The variants share their names and fields with the API's
/// results. A result the client shell only acknowledges (a created tab, a
/// swapped pane, a layout) crosses as `Done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointReply {
    Done,
    PaneInfo {
        pane: Box<PaneInfo>,
    },
    WorkspaceInfo {
        workspace: WorkspaceInfo,
    },
    /// The Git checkout root of the asked directory, `None` outside any
    /// repository, and the home directory of the server's host (`None` when it
    /// has no usable one), which a directory outside Git is compared with.
    WorkspaceCheckoutRoot {
        root: Option<String>,
        home: Option<String>,
    },
    PaneSelection {
        pane_id: String,
        text: String,
    },
    PaneCopyMotion {
        pane_id: String,
        cursor: PaneTextPoint,
    },
    PaneCopySearch {
        pane_id: String,
        matches: Vec<PaneTextRange>,
        total: u64,
        current: Option<u32>,
        current_global: Option<u64>,
    },
    /// Acknowledgement for the client-shell surface interest lease. Its
    /// revision-bearing result can establish an activation floor.
    ClientShellSurfaceSet {
        active: bool,
        projection_revision: u64,
    },
}

/// Why an [`EndpointCommand`] failed: the API error code and its message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointError {
    pub code: String,
    pub message: String,
}
