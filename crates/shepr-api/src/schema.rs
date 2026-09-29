use serde::{Deserialize, Serialize};

pub mod agents;
mod client_shell;
pub mod common;
pub mod panes;
pub mod response;
pub mod server;
pub mod session;
pub mod tabs;
pub mod workspaces;

pub use agents::*;
pub use common::*;
pub use panes::*;
pub use response::*;
pub use server::*;
pub use session::*;
pub use tabs::*;
pub use workspaces::*;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub method: Method,
}

/// Facts about one API method, kept together so request handling, rendering,
/// logging, and routing share one exhaustive classification. Whether a client
/// shell may ask for a method is not one of them: that set is
/// `shepr_protocol::command::EndpointCommand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodTraits {
    pub name: &'static str,
    pub mutates_ui: bool,
    pub changes_topology: bool,
    pub changes_geometry: bool,
    pub claims_shell_geometry: bool,
    pub runs_on_socket_thread: bool,
    pub routine: bool,
}

macro_rules! define_methods {
    (
        $(
            $variant:ident($params:ty) => $name:literal {
                mutates_ui: $mutates_ui:literal,
                changes_topology: $changes_topology:literal,
                changes_geometry: $changes_geometry:literal,
                claims_shell_geometry: $claims_shell_geometry:literal,
                runs_on_socket_thread: $runs_on_socket_thread:literal,
                routine: $routine:literal,
            };
        )+
    ) => {
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum Method {
            $(
                #[serde(rename = $name)]
                $variant($params),
            )+
        }

        #[derive(Clone, Copy)]
        enum MethodKind {
            $($variant,)+
        }

        impl MethodKind {
            fn traits(self) -> MethodTraits {
                // Keep each method's routing facts in one generated match arm.
                match self {
                    $(
                        Self::$variant => MethodTraits {
                            name: $name,
                            mutates_ui: $mutates_ui,
                            changes_topology: $changes_topology,
                            changes_geometry: $changes_geometry,
                            claims_shell_geometry: $claims_shell_geometry,
                            runs_on_socket_thread: $runs_on_socket_thread,
                            routine: $routine,
                        },
                    )+
                }
            }
        }

        impl Method {
            /// All wire names declared by the API schema.
            pub const ALL_NAMES: &'static [&'static str] = &[$($name,)+];

            pub fn traits(&self) -> MethodTraits {
                match self {
                    $(Self::$variant(_) => MethodKind::$variant,)+
                }
                .traits()
            }
        }
    };
}

define_methods! {
    Ping(PingParams) => "ping" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: true, routine: false,
    };
    ServerStop(ServerStopParams) => "server.stop" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: true, routine: false,
    };
    ServerSshAgentRegister(ServerSshAgentRegisterParams) => "server.ssh_agent.register" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: true, routine: false,
    };
    ClientShellSurfaceSet(ClientShellSurfaceSetParams) => "client_shell.surface.set" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: true, routine: false,
    };
    SessionSnapshot(EmptyParams) => "session.snapshot" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: false, routine: false,
    };
    WorkspaceCreate(WorkspaceCreateParams) => "workspace.create" {
        mutates_ui: true, changes_topology: true,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    WorkspaceFocus(WorkspaceTarget) => "workspace.focus" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    WorkspaceRename(WorkspaceRenameParams) => "workspace.rename" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    WorkspaceCheckoutRoot(WorkspaceCheckoutRootParams) => "workspace.checkout_root" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: false, routine: false,
    };
    WorkspaceMove(WorkspaceMoveParams) => "workspace.move" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    WorkspaceClose(WorkspaceCloseParams) => "workspace.close" {
        mutates_ui: true, changes_topology: true,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    TabCreate(TabCreateParams) => "tab.create" {
        mutates_ui: true, changes_topology: true,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    TabFocus(TabTarget) => "tab.focus" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    TabRename(TabRenameParams) => "tab.rename" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    TabMove(TabMoveParams) => "tab.move" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    TabClose(TabTarget) => "tab.close" {
        mutates_ui: true, changes_topology: true,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    DetectCapture(PaneTarget) => "detect.capture" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: false, routine: false,
    };
    DetectExplain(PaneTarget) => "detect.explain" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: false, routine: false,
    };
    PaneSplit(PaneSplitParams) => "pane.split" {
        mutates_ui: true, changes_topology: true,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneSwap(PaneSwapParams) => "pane.swap" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneZoom(PaneZoomParams) => "pane.zoom" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    LayoutSetSplitRatio(LayoutSetSplitRatioParams) => "layout.set_split_ratio" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneFocusDirection(PaneFocusDirectionParams) => "pane.focus_direction" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneResize(PaneResizeParams) => "pane.resize" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneScroll(PaneScrollParams) => "pane.scroll" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneClear(PaneTarget) => "pane.clear" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneSelectionRead(PaneSelectionReadParams) => "pane.selection.read" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: false, routine: false,
    };
    PaneCopyMotion(PaneCopyMotionParams) => "pane.copy_motion" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneCopySearch(PaneCopySearchParams) => "pane.copy_search" {
        mutates_ui: false, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneFocus(PaneTarget) => "pane.focus" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneInputSet(PaneInputSetParams) => "pane.input.set" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneRename(PaneRenameParams) => "pane.rename" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
    PaneReportAgent(PaneReportAgentParams) => "pane.report_agent" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: false, routine: true,
    };
    PaneReportAgentSession(PaneReportAgentSessionParams) => "pane.report_agent_session" {
        mutates_ui: true, changes_topology: false,
        changes_geometry: false, claims_shell_geometry: false,
        runs_on_socket_thread: false, routine: true,
    };
    PaneClose(PaneTarget) => "pane.close" {
        mutates_ui: true, changes_topology: true,
        changes_geometry: true, claims_shell_geometry: true,
        runs_on_socket_thread: false, routine: false,
    };
}

#[cfg(test)]
mod tests;
