use serde::{Deserialize, Serialize};

pub mod agents;
pub mod common;
pub mod events;
pub mod integrations;
pub mod panes;
pub mod response;
pub mod server;
pub mod session;
pub mod tabs;
pub mod workspaces;

pub use agents::*;
pub use common::*;
pub use events::*;
pub use integrations::*;
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
// Request enums are short-lived wire values; keeping variants direct preserves
// the simple serde shape and avoids boxing churn across every caller.
#[allow(clippy::large_enum_variant)]
pub enum Method {
    #[serde(rename = "ping")]
    Ping(PingParams),
    #[serde(rename = "server.stop")]
    ServerStop(EmptyParams),
    #[serde(rename = "server.ssh_agent.register")]
    ServerSshAgentRegister(ServerSshAgentRegisterParams),
    #[serde(rename = "server.agent_manifests")]
    ServerAgentManifests(EmptyParams),
    #[serde(rename = "server.reload_agent_manifests")]
    ServerReloadAgentManifests(EmptyParams),
    #[serde(rename = "client.window_title.set")]
    ClientWindowTitleSet(ClientWindowTitleSetParams),
    #[serde(rename = "client.window_title.clear")]
    ClientWindowTitleClear(EmptyParams),
    #[serde(rename = "client_shell.surface.set")]
    ClientShellSurfaceSet(ClientShellSurfaceSetParams),
    #[serde(rename = "session.snapshot")]
    SessionSnapshot(EmptyParams),
    #[serde(rename = "workspace.create")]
    WorkspaceCreate(WorkspaceCreateParams),
    #[serde(rename = "workspace.list")]
    WorkspaceList(EmptyParams),
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceTarget),
    #[serde(rename = "workspace.focus")]
    WorkspaceFocus(WorkspaceTarget),
    #[serde(rename = "workspace.rename")]
    WorkspaceRename(WorkspaceRenameParams),
    #[serde(rename = "workspace.move")]
    WorkspaceMove(WorkspaceMoveParams),
    #[serde(rename = "workspace.move_block")]
    WorkspaceMoveBlock(WorkspaceMoveBlockParams),
    #[serde(rename = "workspace.report_metadata")]
    WorkspaceReportMetadata(WorkspaceReportMetadataParams),
    #[serde(rename = "workspace.close")]
    WorkspaceClose(WorkspaceCloseParams),
    #[serde(rename = "tab.create")]
    TabCreate(TabCreateParams),
    #[serde(rename = "tab.list")]
    TabList(TabListParams),
    #[serde(rename = "tab.get")]
    TabGet(TabTarget),
    #[serde(rename = "tab.focus")]
    TabFocus(TabTarget),
    #[serde(rename = "tab.rename")]
    TabRename(TabRenameParams),
    #[serde(rename = "tab.move")]
    TabMove(TabMoveParams),
    #[serde(rename = "tab.close")]
    TabClose(TabTarget),
    #[serde(rename = "agent.list")]
    AgentList(EmptyParams),
    #[serde(rename = "agent.get")]
    AgentGet(AgentTarget),
    #[serde(rename = "agent.read")]
    AgentRead(AgentReadParams),
    #[serde(rename = "agent.explain")]
    AgentExplain(AgentTarget),
    #[serde(rename = "agent.send_keys")]
    AgentSendKeys(AgentSendKeysParams),
    #[serde(rename = "agent.rename")]
    AgentRename(AgentRenameParams),
    #[serde(rename = "agent.focus")]
    AgentFocus(AgentTarget),
    #[serde(rename = "agent.start")]
    AgentStart(AgentStartParams),
    #[serde(rename = "agent.prompt")]
    AgentPrompt(AgentPromptParams),
    #[serde(rename = "agent.wait")]
    AgentWait(AgentWaitParams),
    #[serde(rename = "pane.split")]
    PaneSplit(PaneSplitParams),
    #[serde(rename = "pane.swap")]
    PaneSwap(PaneSwapParams),
    #[serde(rename = "pane.move")]
    PaneMove(PaneMoveParams),
    #[serde(rename = "pane.zoom")]
    PaneZoom(PaneZoomParams),
    #[serde(rename = "pane.layout")]
    PaneLayout(PaneLayoutParams),
    #[serde(rename = "pane.process_info")]
    PaneProcessInfo(PaneProcessInfoParams),
    #[serde(rename = "layout.export")]
    LayoutExport(LayoutExportParams),
    #[serde(rename = "layout.apply")]
    LayoutApply(LayoutApplyParams),
    #[serde(rename = "layout.set_split_ratio")]
    LayoutSetSplitRatio(LayoutSetSplitRatioParams),
    #[serde(rename = "pane.neighbor")]
    PaneNeighbor(PaneNeighborParams),
    #[serde(rename = "pane.edges")]
    PaneEdges(PaneEdgesParams),
    #[serde(rename = "pane.focus_direction")]
    PaneFocusDirection(PaneFocusDirectionParams),
    #[serde(rename = "pane.resize")]
    PaneResize(PaneResizeParams),
    #[serde(rename = "pane.scroll")]
    PaneScroll(PaneScrollParams),
    #[serde(rename = "pane.clear")]
    PaneClear(PaneTarget),
    #[serde(rename = "pane.selection.read")]
    PaneSelectionRead(PaneSelectionReadParams),
    #[serde(rename = "pane.copy_motion")]
    PaneCopyMotion(PaneCopyMotionParams),
    #[serde(rename = "pane.copy_search")]
    PaneCopySearch(PaneCopySearchParams),
    #[serde(rename = "pane.list")]
    PaneList(PaneListParams),
    #[serde(rename = "pane.current")]
    PaneCurrent(PaneCurrentParams),
    #[serde(rename = "pane.get")]
    PaneGet(PaneTarget),
    #[serde(rename = "pane.focus")]
    PaneFocus(PaneTarget),
    #[serde(rename = "pane.input.set")]
    PaneInputSet(PaneInputSetParams),
    #[serde(rename = "pane.rename")]
    PaneRename(PaneRenameParams),
    #[serde(rename = "pane.send_text")]
    PaneSendText(PaneSendTextParams),
    #[serde(rename = "pane.send_keys")]
    PaneSendKeys(PaneSendKeysParams),
    #[serde(rename = "pane.send_input")]
    PaneSendInput(PaneSendInputParams),
    #[serde(rename = "pane.read")]
    PaneRead(PaneReadParams),
    #[serde(rename = "pane.report_agent")]
    PaneReportAgent(PaneReportAgentParams),
    #[serde(rename = "pane.report_agent_session")]
    PaneReportAgentSession(PaneReportAgentSessionParams),
    #[serde(rename = "pane.report_metadata")]
    PaneReportMetadata(PaneReportMetadataParams),
    #[serde(rename = "pane.clear_agent_authority")]
    PaneClearAgentAuthority(PaneClearAgentAuthorityParams),
    #[serde(rename = "pane.release_agent")]
    PaneReleaseAgent(PaneReleaseAgentParams),
    #[serde(rename = "pane.close")]
    PaneClose(PaneTarget),
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(EventsSubscribeParams),
    #[serde(rename = "events.wait")]
    EventsWait(EventsWaitParams),
    #[serde(rename = "pane.wait_for_output")]
    PaneWaitForOutput(PaneWaitForOutputParams),
}

/// Facts about one API method, kept together so request handling, rendering,
/// logging, and routing share one exhaustive classification.
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

impl Method {
    pub fn traits(&self) -> MethodTraits {
        // The headless server uses these flags for separate decisions: public
        // geometry reapplication, shell endpoint geometry claims, and shell
        // location reconciliation. Keep every Method arm explicit so a new
        // request must be classified here with its other routing facts.
        let (
            name,
            mutates_ui,
            changes_topology,
            changes_geometry,
            claims_shell_geometry,
            runs_on_socket_thread,
            routine,
        ) = match self {
            Self::Ping(_) => ("ping", false, false, false, false, true, false),
            Self::ServerStop(_) => ("server.stop", false, false, false, false, true, false),
            Self::ServerSshAgentRegister(_) => (
                "server.ssh_agent.register",
                false,
                false,
                false,
                false,
                true,
                false,
            ),
            Self::ServerAgentManifests(_) => (
                "server.agent_manifests",
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::ServerReloadAgentManifests(_) => (
                "server.reload_agent_manifests",
                true,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::ClientWindowTitleSet(_) => (
                "client.window_title.set",
                true,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::ClientWindowTitleClear(_) => (
                "client.window_title.clear",
                true,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::ClientShellSurfaceSet(_) => (
                "client_shell.surface.set",
                true,
                false,
                false,
                false,
                true,
                false,
            ),
            Self::SessionSnapshot(_) => {
                ("session.snapshot", false, false, false, false, false, false)
            }
            Self::WorkspaceCreate(_) => ("workspace.create", true, true, true, true, false, false),
            Self::WorkspaceList(_) => ("workspace.list", false, false, false, false, false, true),
            Self::WorkspaceGet(_) => ("workspace.get", false, false, false, false, false, false),
            Self::WorkspaceFocus(_) => ("workspace.focus", true, false, true, true, false, false),
            Self::WorkspaceRename(_) => {
                ("workspace.rename", true, false, false, true, false, false)
            }
            Self::WorkspaceMove(_) => ("workspace.move", true, false, false, true, false, false),
            Self::WorkspaceMoveBlock(_) => (
                "workspace.move_block",
                true,
                false,
                false,
                true,
                false,
                false,
            ),
            Self::WorkspaceReportMetadata(_) => (
                "workspace.report_metadata",
                true,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::WorkspaceClose(_) => ("workspace.close", true, true, true, true, false, false),
            Self::TabCreate(_) => ("tab.create", true, true, true, true, false, false),
            Self::TabList(_) => ("tab.list", false, false, false, false, false, true),
            Self::TabGet(_) => ("tab.get", false, false, false, false, false, false),
            Self::TabFocus(_) => ("tab.focus", true, false, true, true, false, false),
            Self::TabRename(_) => ("tab.rename", true, false, false, true, false, false),
            Self::TabMove(_) => ("tab.move", true, false, false, true, false, false),
            Self::TabClose(_) => ("tab.close", true, true, true, true, false, false),
            Self::AgentList(_) => ("agent.list", false, false, false, false, false, false),
            Self::AgentGet(_) => ("agent.get", false, false, false, false, false, false),
            Self::AgentRead(_) => ("agent.read", false, false, false, false, false, false),
            Self::AgentExplain(_) => ("agent.explain", false, false, false, false, false, false),
            Self::AgentSendKeys(_) => ("agent.send_keys", true, false, false, false, false, false),
            Self::AgentRename(_) => ("agent.rename", true, false, false, false, false, false),
            Self::AgentFocus(_) => ("agent.focus", true, false, true, true, false, false),
            Self::AgentStart(_) => ("agent.start", true, false, false, false, false, false),
            Self::AgentPrompt(_) => ("agent.prompt", true, false, false, false, true, false),
            Self::AgentWait(_) => ("agent.wait", false, false, false, false, true, false),
            Self::PaneSplit(_) => ("pane.split", true, true, true, true, false, false),
            Self::PaneSwap(_) => ("pane.swap", true, false, true, true, false, false),
            Self::PaneMove(_) => ("pane.move", true, true, true, true, false, false),
            Self::PaneZoom(_) => ("pane.zoom", true, false, true, true, false, false),
            Self::PaneLayout(_) => ("pane.layout", false, false, false, false, false, false),
            Self::PaneProcessInfo(_) => (
                "pane.process_info",
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::LayoutExport(_) => ("layout.export", false, false, false, false, false, false),
            Self::LayoutApply(_) => ("layout.apply", true, true, true, true, false, false),
            Self::LayoutSetSplitRatio(_) => (
                "layout.set_split_ratio",
                true,
                false,
                true,
                true,
                false,
                false,
            ),
            Self::PaneNeighbor(_) => ("pane.neighbor", false, false, false, false, false, false),
            Self::PaneEdges(_) => ("pane.edges", false, false, false, false, false, false),
            Self::PaneFocusDirection(_) => (
                "pane.focus_direction",
                true,
                false,
                true,
                true,
                false,
                false,
            ),
            Self::PaneResize(_) => ("pane.resize", true, false, true, true, false, false),
            Self::PaneScroll(_) => ("pane.scroll", true, false, false, true, false, false),
            Self::PaneClear(_) => ("pane.clear", true, false, false, true, false, false),
            Self::PaneSelectionRead(_) => (
                "pane.selection.read",
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::PaneCopyMotion(_) => {
                ("pane.copy_motion", false, false, false, true, false, false)
            }
            Self::PaneCopySearch(_) => {
                ("pane.copy_search", false, false, false, true, false, false)
            }
            Self::PaneList(_) => ("pane.list", false, false, false, false, false, true),
            Self::PaneCurrent(_) => ("pane.current", false, false, false, false, false, false),
            Self::PaneGet(_) => ("pane.get", false, false, false, false, false, true),
            Self::PaneFocus(_) => ("pane.focus", true, false, true, true, false, false),
            Self::PaneInputSet(_) => ("pane.input.set", true, false, false, true, false, false),
            Self::PaneRename(_) => ("pane.rename", true, false, false, true, false, false),
            Self::PaneSendText(_) => ("pane.send_text", false, false, false, false, false, false),
            Self::PaneSendKeys(_) => ("pane.send_keys", false, false, false, false, false, false),
            Self::PaneSendInput(_) => ("pane.send_input", false, false, false, false, false, false),
            Self::PaneRead(_) => ("pane.read", false, false, false, false, false, true),
            Self::PaneReportAgent(_) => {
                ("pane.report_agent", true, false, false, false, false, true)
            }
            Self::PaneReportAgentSession(_) => (
                "pane.report_agent_session",
                true,
                false,
                false,
                false,
                false,
                true,
            ),
            Self::PaneReportMetadata(_) => (
                "pane.report_metadata",
                true,
                false,
                false,
                false,
                false,
                true,
            ),
            Self::PaneClearAgentAuthority(_) => (
                "pane.clear_agent_authority",
                true,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::PaneReleaseAgent(_) => (
                "pane.release_agent",
                true,
                false,
                false,
                false,
                false,
                false,
            ),
            Self::PaneClose(_) => ("pane.close", true, true, true, true, false, false),
            Self::EventsSubscribe(_) => {
                ("events.subscribe", false, false, false, false, true, false)
            }
            Self::EventsWait(_) => ("events.wait", false, false, false, false, true, false),
            Self::PaneWaitForOutput(_) => (
                "pane.wait_for_output",
                false,
                false,
                false,
                false,
                true,
                false,
            ),
        };

        MethodTraits {
            name,
            mutates_ui,
            changes_topology,
            changes_geometry,
            claims_shell_geometry,
            runs_on_socket_thread,
            routine,
        }
    }
}

#[cfg(test)]
mod tests;
