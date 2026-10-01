//! Typed endpoint operations a client shell sends over the server socket, and
//! the replies it gets back.
//!
//! This is the client shell's whole vocabulary: the server dispatches an
//! [`EndpointCommand`] straight to its handlers and answers with an
//! [`EndpointReply`], with no JSON API method in between. The types are positional wire types:
//! no field is skipped or flattened, and every id is typed.

use serde::{Deserialize, Serialize};

use crate::{AgentStatus, PublicPaneId, WorkspaceId};

/// Updates whether the requesting client shell receives and controls pane presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellSurfaceSetParams {
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTarget {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTarget {
    pub pane_id: PublicPaneId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

/// Where a new workspace's first pane starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceCreateSource {
    /// An explicit working directory.
    Cwd(String),
    /// The focused pane of this workspace supplies the cwd policy
    /// (`new_terminal_cwd`); a workspace that no longer exists falls back to
    /// [`Self::Default`].
    Follow(WorkspaceId),
    /// The server's default working directory.
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreateParams {
    pub source: WorkspaceCreateSource,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCloseParams {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRenameParams {
    pub workspace_id: WorkspaceId,
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
    pub workspace_id: WorkspaceId,
    /// Insert before this workspace, or at the end when absent.
    pub before_workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: WorkspaceId,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaneRightClickTarget {
    #[default]
    Shepr,
    Pane,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSplitParams {
    pub pane_id: PublicPaneId,
    pub direction: SplitDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInputSetParams {
    pub pane_id: PublicPaneId,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSwapParams {
    Direction {
        pane_id: PublicPaneId,
        direction: PaneDirection,
    },
    Panes {
        source: PublicPaneId,
        target: PublicPaneId,
    },
}

/// Always a toggle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneZoomParams {
    pub pane_id: PublicPaneId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutSetSplitRatioParams {
    pub workspace_id: WorkspaceId,
    /// Exact pane membership of the two children, captured when dragging starts.
    pub first_panes: Vec<PublicPaneId>,
    pub second_panes: Vec<PublicPaneId>,
    pub ratio: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneFocusDirectionParams {
    pub pane_id: PublicPaneId,
    pub direction: PaneDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneResizeParams {
    pub pane_id: PublicPaneId,
    pub direction: PaneDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneScrollParams {
    pub pane_id: PublicPaneId,
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
    pub pane_id: PublicPaneId,
    pub anchor: PaneTextPoint,
    pub cursor: PaneTextPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneLineMotion {
    End,
    FirstNonBlank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneWordMotion {
    NextStart,
    PreviousStart,
    NextEnd,
    NextBigStart,
    PreviousBigStart,
    NextBigEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneParagraphMotion {
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneCopyMotion {
    Line(PaneLineMotion),
    Word(PaneWordMotion),
    Paragraph(PaneParagraphMotion),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCopyMotionParams {
    pub pane_id: PublicPaneId,
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
    pub pane_id: PublicPaneId,
    pub query: String,
    pub direction: PaneCopySearchDirection,
    pub cursor: PaneTextPoint,
    pub previous: Option<PaneTextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRenameParams {
    pub pane_id: PublicPaneId,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneScrollInfo {
    pub offset_from_bottom: u64,
    pub max_offset_from_bottom: u64,
    pub viewport_rows: u64,
}

/// What a client shell reads back about one pane after a command: which pane
/// it was, whether it now has focus, and its scroll position. Everything else
/// about a pane reaches the shell through its snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: PublicPaneId,
    pub focused: bool,
    pub scroll: Option<PaneScrollInfo>,
}

/// One endpoint operation a client shell asks of the server it is connected
/// to; nothing outside this set can be asked through a client shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EndpointCommand {
    ClientShellSurfaceSet(ClientShellSurfaceSetParams),
    WorkspaceCreate(WorkspaceCreateParams),
    WorkspaceFocus(WorkspaceTarget),
    WorkspaceRename(WorkspaceRenameParams),
    WorkspaceCheckoutRoot(WorkspaceCheckoutRootParams),
    WorkspaceMove(WorkspaceMoveParams),
    WorkspaceClose(WorkspaceCloseParams),
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

/// Facts about one endpoint command, kept in one exhaustive table so the
/// client's notices, the server's logs and the server loop's routing agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointCommandTraits {
    /// The dotted name logs and client notices use.
    pub name: &'static str,
    /// The command creates or removes a workspace or pane, or reorders
    /// workspaces (which client locations track by index), so every shell
    /// client's location is reconciled after it.
    pub changes_topology: bool,
    /// The requesting shell claims geometry when the command's action should
    /// make it the PTY size source for the workspace.
    pub claims_shell_geometry: bool,
}

impl EndpointCommand {
    pub fn traits(&self) -> EndpointCommandTraits {
        let (name, changes_topology, claims_shell_geometry) = match self {
            Self::ClientShellSurfaceSet(_) => ("client_shell.surface.set", false, false),
            Self::WorkspaceCreate(_) => ("workspace.create", true, true),
            Self::WorkspaceFocus(_) => ("workspace.focus", false, true),
            Self::WorkspaceRename(_) => ("workspace.rename", false, true),
            Self::WorkspaceCheckoutRoot(_) => ("workspace.checkout_root", false, false),
            Self::WorkspaceMove(_) => ("workspace.move", true, true),
            Self::WorkspaceClose(_) => ("workspace.close", true, true),
            Self::PaneSplit(_) => ("pane.split", true, true),
            Self::PaneSwap(_) => ("pane.swap", false, true),
            Self::PaneZoom(_) => ("pane.zoom", false, true),
            Self::LayoutSetSplitRatio(_) => ("layout.set_split_ratio", false, true),
            Self::PaneFocusDirection(_) => ("pane.focus_direction", false, true),
            Self::PaneResize(_) => ("pane.resize", false, true),
            Self::PaneScroll(_) => ("pane.scroll", false, true),
            Self::PaneClear(_) => ("pane.clear", false, true),
            Self::PaneSelectionRead(_) => ("pane.selection.read", false, false),
            Self::PaneCopyMotion(_) => ("pane.copy_motion", false, false),
            Self::PaneCopySearch(_) => ("pane.copy_search", false, false),
            Self::PaneFocus(_) => ("pane.focus", false, true),
            Self::PaneInputSet(_) => ("pane.input.set", false, true),
            Self::PaneRename(_) => ("pane.rename", false, true),
            Self::PaneClose(_) => ("pane.close", true, true),
        };
        EndpointCommandTraits {
            name,
            changes_topology,
            claims_shell_geometry,
        }
    }

    /// The command's dotted name, as logs and client notices spell it.
    pub fn name(&self) -> &'static str {
        self.traits().name
    }
}

/// The successful result of an [`EndpointCommand`], carrying what the client
/// shell reads. A command the client shell only acknowledges (a moved workspace, a
/// swapped pane, a new split ratio) answers `Done`.
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
        pane_id: PublicPaneId,
        text: String,
    },
    PaneCopyMotion {
        pane_id: PublicPaneId,
        cursor: PaneTextPoint,
    },
    PaneCopySearch {
        pane_id: PublicPaneId,
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

/// Why an [`EndpointCommand`] failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointError {
    /// The app refused the command; the message is for the user.
    Rejected(String),
    /// The server is shutting down.
    ShuttingDown,
    /// The command was aimed at another boot of the server.
    StaleBoot,
    /// The requesting client's surface is not active.
    SurfaceInactive,
    /// The reply did not fit the wire limit.
    ResponseTooLarge { size: u64, limit: u64 },
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => f.write_str(message),
            Self::ShuttingDown => f.write_str("the server is shutting down"),
            Self::StaleBoot => f.write_str("the command was aimed at a previous server boot"),
            Self::SurfaceInactive => f.write_str("the client surface is not active"),
            Self::ResponseTooLarge { size, limit } => write!(
                f,
                "the response of {size} bytes exceeds the {limit} byte limit"
            ),
        }
    }
}

impl std::error::Error for EndpointError {}
