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
pub enum SplitDirection {
    Right,
    Down,
}

/// Where a new workspace's first pane starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceCreateSource {
    /// An explicit working directory.
    Cwd(crate::RemotePath),
    /// The focused pane of this workspace supplies the cwd policy
    /// (`terminal.new_cwd`); a workspace that no longer exists falls back to
    /// [`Self::Default`].
    Follow(WorkspaceId),
    /// The server's default working directory.
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreateParams {
    pub source: WorkspaceCreateSource,
    /// The new workspace's name. `None`, or a label that is empty once
    /// trimmed, names it after the directory it starts in.
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCloseParams {
    pub workspace_id: WorkspaceId,
}

/// `None`, or a label that is empty once trimmed, names the workspace after its
/// current directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRenameParams {
    pub workspace_id: WorkspaceId,
    pub label: Option<String>,
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
    pub label: String,
    pub pane_count: usize,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// Pane navigation uses the layout's own cardinal direction.
pub use shepr_core::layout::NavDirection as PaneDirection;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSetSplitRatioParams {
    pub workspace_id: WorkspaceId,
    /// The split's address and the layout epoch the surface published it with.
    /// The server refuses the command when its epoch has moved on.
    #[serde(
        serialize_with = "crate::codec::serialize_bounded_vec::<{ crate::MAX_SURFACE_SPLIT_PATH }, _, _>",
        deserialize_with = "crate::codec::deserialize_bounded_vec::<{ crate::MAX_SURFACE_SPLIT_PATH }, _, _>"
    )]
    pub path: Vec<shepr_core::layout::SplitBranch>,
    pub epoch: shepr_core::layout::LayoutEpoch,
    pub ratio: shepr_core::layout::SplitRatio,
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
    pub offset_from_bottom: usize,
}

/// A terminal cell addressed by a stable absolute row: output and history
/// eviction never make it name another line. Selections, copy-mode cursors
/// and search matches all use it.
pub type PaneTextPoint = shepr_term::Point<shepr_term::AbsRow>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTextRange {
    pub start: PaneTextPoint,
    pub end: PaneTextPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSelectionReadParams {
    pub pane_id: PublicPaneId,
    pub anchor: PaneTextPoint,
    pub cursor: PaneTextPoint,
}

// The copy motions and search direction are the terminal's own vocabulary
// (`shepr-term`), carried on the wire as they are.
pub use shepr_term::copy_motion::CopyMotion as PaneCopyMotion;
pub use shepr_term::copy_motion::LineMotion as PaneLineMotion;
pub use shepr_term::copy_motion::ParagraphMotion as PaneParagraphMotion;
pub use shepr_term::copy_motion::SearchDirection as PaneCopySearchDirection;
pub use shepr_term::copy_motion::WordMotion as PaneWordMotion;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCopyMotionParams {
    pub pane_id: PublicPaneId,
    pub cursor: PaneTextPoint,
    pub motion: PaneCopyMotion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCopySearchParams {
    pub pane_id: PublicPaneId,
    pub query: String,
    pub direction: PaneCopySearchDirection,
    pub cursor: PaneTextPoint,
    pub previous: Option<PaneTextRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCopySearchPosition {
    /// The match's index in the returned window.
    pub window_index: usize,
    /// The match's index in the full result set.
    pub global_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCopySearch {
    pub matches: Vec<PaneTextRange>,
    pub total: usize,
    /// Absent when the search found no matches; both indexes travel together.
    pub current: Option<PaneCopySearchPosition>,
}

/// `None` clears the manual label, as does a label that is empty once trimmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRenameParams {
    pub pane_id: PublicPaneId,
    pub label: Option<String>,
}

pub use shepr_term::ScrollMetrics as PaneScrollInfo;

/// What a client shell reads back about one pane after a command: which pane
/// it was and its scroll position. Focus is part of the requester-specific
/// shell snapshot. Everything else about a pane reaches the shell through it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: PublicPaneId,
    pub scroll: Option<PaneScrollInfo>,
}

/// Facts about one endpoint command, kept in one exhaustive table so the
/// client's notices, the server's logs and server dispatch agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointCommandTraits {
    /// The dotted name logs and client notices use.
    pub name: &'static str,
    /// The command can move the selected workspace or pane.
    pub changes_focus: bool,
    /// The command creates or removes a workspace or pane, or reorders
    /// workspaces (which client locations track by index), so every shell
    /// client's location is reconciled after it.
    pub changes_topology: bool,
    /// The requesting shell claims geometry when the command's action should
    /// make it the PTY size source for the workspace.
    pub claims_shell_geometry: bool,
}

macro_rules! define_endpoint_commands {
    (
        loop_commands {
            $(
                $loop_variant:ident($loop_params:ty) => $loop_name:literal {
                    changes_focus: $loop_focus:literal,
                    changes_topology: $loop_topology:literal,
                    claims_shell_geometry: $loop_geometry:literal,
                };
            )+
        }
        app_commands {
            $(
                $app_variant:ident($app_params:ty) => $app_name:literal {
                    changes_focus: $app_focus:literal,
                    changes_topology: $app_topology:literal,
                    claims_shell_geometry: $app_geometry:literal,
                };
            )+
        }
    ) => {
        /// One endpoint operation a client shell asks of the server it is connected
        /// to; nothing outside this set can be asked through a client shell.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum EndpointCommand {
            $($loop_variant($loop_params),)+
            $($app_variant($app_params),)+
        }

        /// The finite identity of an endpoint command, without its request data.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum CommandKind {
            $($loop_variant,)+
            $($app_variant,)+
        }

        /// Commands the app handles after the server loop has routed its own work.
        #[derive(Debug, Clone, PartialEq)]
        pub enum EndpointAppCommand {
            $($app_variant($app_params),)+
        }

        /// Commands answered or delegated by the server loop itself.
        #[derive(Debug, Clone, PartialEq)]
        pub enum EndpointLoopCommand {
            $($loop_variant($loop_params),)+
        }

        impl EndpointCommand {
            pub fn kind(&self) -> CommandKind {
                match self {
                    $(Self::$loop_variant(_) => CommandKind::$loop_variant,)+
                    $(Self::$app_variant(_) => CommandKind::$app_variant,)+
                }
            }

            pub fn traits(&self) -> EndpointCommandTraits {
                match self {
                    $(
                        Self::$loop_variant(_) => EndpointCommandTraits {
                            name: $loop_name,
                            changes_focus: $loop_focus,
                            changes_topology: $loop_topology,
                            claims_shell_geometry: $loop_geometry,
                        },
                    )+
                    $(
                        Self::$app_variant(_) => EndpointCommandTraits {
                            name: $app_name,
                            changes_focus: $app_focus,
                            changes_topology: $app_topology,
                            claims_shell_geometry: $app_geometry,
                        },
                    )+
                }
            }

            /// The command's dotted name, as logs and client notices spell it.
            pub fn name(&self) -> &'static str {
                self.traits().name
            }

            /// Makes it impossible to pass a server-loop command to app dispatch.
            pub fn into_app_command(self) -> Result<EndpointAppCommand, EndpointLoopCommand> {
                match self {
                    $(
                        Self::$loop_variant(params) => {
                            Err(EndpointLoopCommand::$loop_variant(params))
                        }
                    )+
                    $(
                        Self::$app_variant(params) => {
                            Ok(EndpointAppCommand::$app_variant(params))
                        }
                    )+
                }
            }
        }

        impl CommandKind {
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$loop_variant => $loop_name,)+
                    $(Self::$app_variant => $app_name,)+
                }
            }
        }

        impl EndpointAppCommand {
            pub fn traits(&self) -> EndpointCommandTraits {
                match self {
                    $(
                        Self::$app_variant(_) => EndpointCommandTraits {
                            name: $app_name,
                            changes_focus: $app_focus,
                            changes_topology: $app_topology,
                            claims_shell_geometry: $app_geometry,
                        },
                    )+
                }
            }
        }

        impl EndpointLoopCommand {
            /// The loop-owned command's dotted name, for a rejected test-only app call.
            pub fn name(&self) -> &'static str {
                match self {
                    $(Self::$loop_variant(_) => $loop_name,)+
                }
            }
        }
    };
}

define_endpoint_commands! {
    loop_commands {
        ClientShellSurfaceSet(ClientShellSurfaceSetParams) => "client_shell.surface.set" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: false,
        };
    }
    app_commands {
        WorkspaceCreate(WorkspaceCreateParams) => "workspace.create" {
            changes_focus: true,
            changes_topology: true,
            claims_shell_geometry: true,
        };
        WorkspaceFocus(WorkspaceTarget) => "workspace.focus" {
            changes_focus: true,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        WorkspaceRename(WorkspaceRenameParams) => "workspace.rename" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        WorkspaceMove(WorkspaceMoveParams) => "workspace.move" {
            changes_focus: false,
            changes_topology: true,
            claims_shell_geometry: true,
        };
        WorkspaceClose(WorkspaceCloseParams) => "workspace.close" {
            changes_focus: true,
            changes_topology: true,
            claims_shell_geometry: true,
        };
        PaneSplit(PaneSplitParams) => "pane.split" {
            changes_focus: true,
            changes_topology: true,
            claims_shell_geometry: true,
        };
        // The swap focuses its source pane and navigates to its workspace.
        PaneSwap(PaneSwapParams) => "pane.swap" {
            changes_focus: true,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneZoom(PaneZoomParams) => "pane.zoom" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        LayoutSetSplitRatio(LayoutSetSplitRatioParams) => "layout.set_split_ratio" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneFocusDirection(PaneFocusDirectionParams) => "pane.focus_direction" {
            changes_focus: true,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneResize(PaneResizeParams) => "pane.resize" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneScroll(PaneScrollParams) => "pane.scroll" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneClear(PaneTarget) => "pane.clear" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneSelectionRead(PaneSelectionReadParams) => "pane.selection.read" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: false,
        };
        PaneCopyMotion(PaneCopyMotionParams) => "pane.copy_motion" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: false,
        };
        PaneCopySearch(PaneCopySearchParams) => "pane.copy_search" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: false,
        };
        PaneFocus(PaneTarget) => "pane.focus" {
            changes_focus: true,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneInputSet(PaneInputSetParams) => "pane.input.set" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneRename(PaneRenameParams) => "pane.rename" {
            changes_focus: false,
            changes_topology: false,
            claims_shell_geometry: true,
        };
        PaneClose(PaneTarget) => "pane.close" {
            changes_focus: true,
            changes_topology: true,
            claims_shell_geometry: true,
        };
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
        search: PaneCopySearch,
    },
    /// Acknowledgement for the client-shell surface interest lease. Its
    /// revision-bearing result can establish an activation floor.
    ClientShellSurfaceSet {
        active: bool,
        projection_revision: crate::ProjectionRevision,
    },
}

/// Why an [`EndpointCommand`] failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointError {
    WorkspaceGone(WorkspaceId),
    PaneGone(PublicPaneId),
    SplitGone,
    InvalidArgument(String),
    Busy(String),
    ResourceFailure(String),
    Internal(String),
    Unavailable(String),
    AlternateScreen(PublicPaneId),
    /// The server is shutting down.
    ShuttingDown,
    /// The command was aimed at another boot of the server.
    StaleBoot,
    /// The requesting client's surface is not active.
    SurfaceInactive,
    /// The reply did not fit its wire limit.
    LimitExceeded(crate::LimitExceeded),
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument(message)
            | Self::Busy(message)
            | Self::ResourceFailure(message)
            | Self::Internal(message)
            | Self::Unavailable(message) => f.write_str(message),
            Self::WorkspaceGone(id) => write!(f, "workspace {id} not found"),
            Self::PaneGone(id) => write!(f, "pane {id} not found"),
            Self::SplitGone => f.write_str("split not found"),
            Self::AlternateScreen(_) => f.write_str("the pane is on the alternate screen"),
            Self::ShuttingDown => f.write_str("the server is shutting down"),
            Self::StaleBoot => f.write_str("the command was aimed at a previous server boot"),
            Self::SurfaceInactive => f.write_str("the client surface is not active"),
            Self::LimitExceeded(error) => write!(f, "the {error}"),
        }
    }
}

impl std::error::Error for EndpointError {}

/// Reply payloads consumed by shell continuations. Decode the wire sum at the
/// request boundary, before calling a continuation with its specific payload.
macro_rules! endpoint_reply_payloads {
    ($($name:ident => $variant:ident { $($field:ident: $ty:ty),+ } ;)+) => {
        $(#[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name { $(pub $field: $ty,)+ }
        impl TryFrom<EndpointReply> for $name {
            type Error = EndpointError;
            fn try_from(reply: EndpointReply) -> Result<Self, Self::Error> {
                match reply {
                    EndpointReply::$variant { $($field,)+ } => Ok(Self { $($field,)+ }),
                    _ => Err(EndpointError::Internal(concat!(
                        "endpoint returned an unexpected ", stringify!($variant), " result"
                    ).into())),
                }
            }
        })+
    };
}
endpoint_reply_payloads! {
    PaneInfoReply => PaneInfo { pane: Box<PaneInfo> };
    PaneSelectionReply => PaneSelection { pane_id: PublicPaneId, text: String };
    PaneCopyMotionReply => PaneCopyMotion { pane_id: PublicPaneId, cursor: PaneTextPoint };
    PaneCopySearchReply => PaneCopySearch { pane_id: PublicPaneId, search: PaneCopySearch };
}

impl CommandKind {
    /// Checks the dynamic wire envelope against the request that owns it.
    pub fn accepts_reply(self, reply: &EndpointReply) -> bool {
        match self {
            Self::ClientShellSurfaceSet => {
                matches!(reply, EndpointReply::ClientShellSurfaceSet { .. })
            }
            Self::WorkspaceFocus | Self::WorkspaceRename => {
                matches!(reply, EndpointReply::WorkspaceInfo { .. })
            }
            Self::PaneSplit | Self::PaneFocus | Self::PaneRename | Self::PaneScroll => {
                matches!(reply, EndpointReply::PaneInfo { .. })
            }
            Self::PaneSelectionRead => matches!(reply, EndpointReply::PaneSelection { .. }),
            Self::PaneCopyMotion => matches!(reply, EndpointReply::PaneCopyMotion { .. }),
            Self::PaneCopySearch => matches!(reply, EndpointReply::PaneCopySearch { .. }),
            Self::WorkspaceCreate
            | Self::WorkspaceMove
            | Self::WorkspaceClose
            | Self::PaneSwap
            | Self::PaneZoom
            | Self::LayoutSetSplitRatio
            | Self::PaneFocusDirection
            | Self::PaneResize
            | Self::PaneClear
            | Self::PaneInputSet
            | Self::PaneClose => matches!(reply, EndpointReply::Done),
        }
    }
}

#[cfg(test)]
mod reply_contract_tests {
    use super::*;

    #[test]
    fn read_continuations_reject_an_acknowledgement_as_an_internal_error() {
        assert!(matches!(
            PaneSelectionReply::try_from(EndpointReply::Done),
            Err(EndpointError::Internal(_))
        ));
        assert!(matches!(
            PaneInfoReply::try_from(EndpointReply::Done),
            Err(EndpointError::Internal(_))
        ));
        assert!(matches!(
            PaneCopyMotionReply::try_from(EndpointReply::Done),
            Err(EndpointError::Internal(_))
        ));
        assert!(matches!(
            PaneCopySearchReply::try_from(EndpointReply::Done),
            Err(EndpointError::Internal(_))
        ));
    }

    #[test]
    fn command_contract_checks_even_acknowledgement_only_work() {
        let selection = EndpointReply::PaneSelection {
            pane_id: "w1:p1".parse().expect("pane id"),
            text: String::new(),
        };
        assert!(CommandKind::PaneSelectionRead.accepts_reply(&selection));
        assert!(!CommandKind::PaneSelectionRead.accepts_reply(&EndpointReply::Done));
        assert!(CommandKind::WorkspaceClose.accepts_reply(&EndpointReply::Done));
        assert!(!CommandKind::WorkspaceClose.accepts_reply(&selection));
    }

    #[test]
    fn selection_payload_preserves_its_pane_identity() {
        let pane_id = "w9:p2".parse().expect("valid pane id");
        let payload = PaneSelectionReply::try_from(EndpointReply::PaneSelection {
            pane_id,
            text: "selected".into(),
        })
        .expect("selection payload");
        assert_eq!(payload.pane_id, pane_id);
        assert_eq!(payload.text, "selected");
    }
}
