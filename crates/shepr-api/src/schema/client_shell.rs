//! Conversions between the client-shell wire vocabulary
//! (`shepr_protocol::command`) and the API's methods and results. The
//! parameter and result types are the same types, so every conversion is a
//! move; this is the one place both vocabularies are visible.

use shepr_protocol::command::{EndpointCommand, EndpointError, EndpointReply};

use super::{Method, ResponseResult};
use crate::error::ApiError;

/// Generates both directions of the command conversion from one list of the
/// variants the two enums share. `From<EndpointCommand>` matches exhaustively,
/// so a command missing from the list fails to compile.
macro_rules! client_shell_methods {
    ($($variant:ident),+ $(,)?) => {
        impl From<EndpointCommand> for Method {
            fn from(command: EndpointCommand) -> Self {
                match command {
                    $(EndpointCommand::$variant(params) => Self::$variant(params),)+
                }
            }
        }

        impl Method {
            /// The client-shell command this method is, or the method back when
            /// a client shell cannot ask for it.
            pub fn into_endpoint_command(self) -> Result<EndpointCommand, Box<Self>> {
                match self {
                    $(Self::$variant(params) => Ok(EndpointCommand::$variant(params)),)+
                    other => Err(Box::new(other)),
                }
            }
        }
    };
}

client_shell_methods! {
    ClientShellSurfaceSet,
    WorkspaceCreate,
    WorkspaceFocus,
    WorkspaceRename,
    WorkspaceCheckoutRoot,
    WorkspaceMove,
    WorkspaceClose,
    TabCreate,
    TabFocus,
    TabRename,
    TabMove,
    TabClose,
    PaneSplit,
    PaneSwap,
    PaneZoom,
    LayoutSetSplitRatio,
    PaneFocusDirection,
    PaneResize,
    PaneScroll,
    PaneClear,
    PaneSelectionRead,
    PaneCopyMotion,
    PaneCopySearch,
    PaneFocus,
    PaneInputSet,
    PaneRename,
    PaneClose,
}

impl From<ResponseResult> for EndpointReply {
    fn from(result: ResponseResult) -> Self {
        match result {
            ResponseResult::PaneInfo { pane } => Self::PaneInfo {
                pane: Box::new(pane),
            },
            ResponseResult::WorkspaceInfo { workspace } => Self::WorkspaceInfo { workspace },
            ResponseResult::WorkspaceCheckoutRoot { root, home } => {
                Self::WorkspaceCheckoutRoot { root, home }
            }
            ResponseResult::PaneSelection { pane_id, text } => {
                Self::PaneSelection { pane_id, text }
            }
            ResponseResult::PaneCopyMotion { pane_id, cursor } => {
                Self::PaneCopyMotion { pane_id, cursor }
            }
            ResponseResult::PaneCopySearch {
                pane_id,
                matches,
                total,
                current,
                current_global,
            } => Self::PaneCopySearch {
                pane_id,
                matches,
                total,
                current,
                current_global,
            },
            ResponseResult::ClientShellSurfaceSet {
                active,
                projection_revision,
            } => Self::ClientShellSurfaceSet {
                active,
                projection_revision,
            },
            // Results the client shell only acknowledges, and the ones no
            // client-shell command produces.
            ResponseResult::Ok {}
            | ResponseResult::Pong { .. }
            | ResponseResult::SessionSnapshot { .. }
            | ResponseResult::WorkspaceCreated { .. }
            | ResponseResult::WorkspaceList { .. }
            | ResponseResult::TabInfo { .. }
            | ResponseResult::TabCreated { .. }
            | ResponseResult::TabList { .. }
            | ResponseResult::PaneSwap { .. }
            | ResponseResult::PaneZoom { .. }
            | ResponseResult::LayoutSplitRatioSet { .. }
            | ResponseResult::PaneFocusDirection { .. }
            | ResponseResult::PaneResize { .. }
            | ResponseResult::DetectCapture { .. }
            | ResponseResult::DetectExplain { .. } => Self::Done,
        }
    }
}

impl From<EndpointReply> for ResponseResult {
    fn from(reply: EndpointReply) -> Self {
        match reply {
            EndpointReply::Done => Self::Ok {},
            EndpointReply::PaneInfo { pane } => Self::PaneInfo { pane: *pane },
            EndpointReply::WorkspaceInfo { workspace } => Self::WorkspaceInfo { workspace },
            EndpointReply::WorkspaceCheckoutRoot { root, home } => {
                Self::WorkspaceCheckoutRoot { root, home }
            }
            EndpointReply::PaneSelection { pane_id, text } => Self::PaneSelection { pane_id, text },
            EndpointReply::PaneCopyMotion { pane_id, cursor } => {
                Self::PaneCopyMotion { pane_id, cursor }
            }
            EndpointReply::PaneCopySearch {
                pane_id,
                matches,
                total,
                current,
                current_global,
            } => Self::PaneCopySearch {
                pane_id,
                matches,
                total,
                current,
                current_global,
            },
            EndpointReply::ClientShellSurfaceSet {
                active,
                projection_revision,
            } => Self::ClientShellSurfaceSet {
                active,
                projection_revision,
            },
        }
    }
}

impl From<ApiError> for EndpointError {
    fn from(error: ApiError) -> Self {
        Self {
            code: error.code.as_str().to_owned(),
            message: error.into_message(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApiErrorCode;
    use crate::schema::{PaneTarget, PingParams, ServerStopParams, TabMoveParams};

    #[test]
    fn client_shell_commands_convert_to_their_methods_and_back() {
        let method = Method::TabMove(TabMoveParams {
            tab_id: "w1:t2".into(),
            insert_index: 0,
        });
        let command = method
            .clone()
            .into_endpoint_command()
            .expect("tab.move is a client-shell command");
        assert_eq!(Method::from(command), method);
    }

    #[test]
    fn api_front_door_and_lifecycle_methods_are_not_client_shell_commands() {
        for method in [
            Method::Ping(PingParams::default()),
            Method::ServerStop(ServerStopParams::default()),
            Method::DetectCapture(PaneTarget {
                pane_id: "w1:p1".into(),
            }),
        ] {
            assert_eq!(
                method.clone().into_endpoint_command(),
                Err(Box::new(method))
            );
        }
    }

    #[test]
    fn acknowledged_results_cross_as_done_and_read_back_as_ok() {
        let reply = EndpointReply::from(ResponseResult::Ok {});
        assert_eq!(reply, EndpointReply::Done);
        assert_eq!(ResponseResult::from(reply), ResponseResult::Ok {});
    }

    #[test]
    fn api_errors_keep_their_wire_code_and_message() {
        assert_eq!(
            EndpointError::from(ApiError::pane_not_found("w1:p7")),
            EndpointError {
                code: "pane_not_found".into(),
                message: "pane w1:p7 not found".into(),
            }
        );
        assert_eq!(
            EndpointError::from(ApiError::new(ApiErrorCode::StaleBoot, "old boot")).code,
            "stale_boot"
        );
    }
}
