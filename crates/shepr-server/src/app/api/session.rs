use crate::app::App;
use shepr_protocol::command::WorkspaceInfo;
use shepr_protocol::{AgentStatus, PublicPaneId, WorkspaceId};

/// The session's workspaces, panes and agents, the layout-free step between
/// `App` state and the client-shell snapshot each shell receives. It names no
/// focused workspace: which workspace a shell views is its own location, and
/// each projection derives the focus from it. It carries only what
/// `HeadlessServer::snapshot_from_session` projects; the server rebuilds it on the loop, so a
/// field nothing reads costs every rebuild.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectionInput {
    pub(crate) workspaces: Vec<WorkspaceInfo>,
    pub(crate) panes: Vec<SnapshotPane>,
    pub(crate) agents: Vec<SnapshotAgent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotPane {
    pub(crate) pane_id: PublicPaneId,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) label: Option<String>,
    pub(crate) cwd: Option<shepr_protocol::RemotePath>,
    pub(crate) foreground_cwd: Option<shepr_protocol::RemotePath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotAgent {
    pub(crate) pane_id: PublicPaneId,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) agent: shepr_agent::Agent,
    pub(crate) terminal_title: Option<String>,
    pub(crate) terminal_title_stripped: Option<String>,
    pub(crate) agent_status: AgentStatus,
    pub(crate) state_change_seq: shepr_agent::StateChangeSeq,
}

impl App {
    /// The session's workspaces, panes and agents, which the server
    /// projects into each client shell's snapshot.
    pub(crate) fn projection_input(&self) -> ProjectionInput {
        let mut workspaces = Vec::new();
        let mut panes = Vec::new();
        for ws in self.state.workspaces().iter() {
            workspaces.extend(self.workspace_info(&ws.id()));
            panes.extend(
                ws.tree()
                    .pane_ids()
                    .into_iter()
                    .filter_map(|pane_id| self.state.pane(pane_id))
                    .map(|pane| self.snapshot_pane(&pane)),
            );
        }

        ProjectionInput {
            workspaces,
            panes,
            agents: self.collect_agent_infos(),
        }
    }

    fn snapshot_pane(&self, pane: &shepr_mux::workspace::PaneRef<'_>) -> SnapshotPane {
        let ws = pane.workspace();
        let pane_id = pane.id();
        let terminal = pane.terminal();
        SnapshotPane {
            pane_id: pane.public_id(),
            workspace_id: ws.id(),
            label: terminal.manual_label().map(str::to_owned),
            cwd: ws
                .cwd_for_pane(pane_id, &self.terminal_runtimes)
                .map(|cwd| shepr_protocol::RemotePath::from(cwd.into_path_buf())),
            // Runs on the server main loop once per pane for every session
            // snapshot the client shells are projected from, so the runtime
            // accessor behind it must stay a few /proc reads and never wait on
            // the PTY actor thread.
            foreground_cwd: ws
                .foreground_cwd_for_pane(pane_id, &self.terminal_runtimes)
                .map(shepr_protocol::RemotePath::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::*;
    use shepr_config::ServerConfig;
    use shepr_mux::workspace::Workspace;

    fn app_with_two_panes() -> crate::app::TestApp {
        let mut app = crate::app::App::new(&ServerConfig::default());
        let mut workspace = Workspace::test_new("snapshot");
        workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.test_set_workspaces(vec![workspace]);
        app.state.seed_bookmark_index(Some(0));
        app
    }

    #[test]
    fn projection_input_lists_the_session_without_naming_a_focus() {
        let app = app_with_two_panes();
        let snapshot = app.projection_input();

        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.panes.len(), 2);
        // Focus is each client's own location, not part of the shared
        // session: the snapshot's types have no focus field to set.
    }
}
