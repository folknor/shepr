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
pub(crate) struct SessionSnapshot {
    pub(crate) workspaces: Vec<WorkspaceInfo>,
    pub(crate) panes: Vec<SnapshotPane>,
    pub(crate) agents: Vec<SnapshotAgent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotPane {
    pub(crate) pane_id: PublicPaneId,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) label: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) foreground_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotAgent {
    pub(crate) pane_id: PublicPaneId,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) agent: Option<String>,
    pub(crate) terminal_title: Option<String>,
    pub(crate) terminal_title_stripped: Option<String>,
    pub(crate) agent_status: AgentStatus,
    pub(crate) state_change_seq: u64,
}

impl App {
    /// The session's workspaces, panes and agents, which the server
    /// projects into each client shell's snapshot.
    pub(crate) fn session_snapshot(&self) -> SessionSnapshot {
        let mut workspaces = Vec::new();
        let mut panes = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            workspaces.extend(self.workspace_info(ws_idx));
            panes.extend(
                ws.layout()
                    .pane_ids()
                    .into_iter()
                    .filter_map(|pane_id| self.snapshot_pane(ws_idx, pane_id)),
            );
        }

        SessionSnapshot {
            workspaces,
            panes,
            agents: self.collect_agent_infos(),
        }
    }

    fn snapshot_pane(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<SnapshotPane> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        Some(SnapshotPane {
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            workspace_id: self.public_workspace_id(ws_idx)?,
            label: terminal.manual_label().map(str::to_owned),
            cwd: ws
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            // Runs on the server main loop once per pane for every session
            // snapshot the client shells are projected from, so the runtime
            // accessor behind it must stay a few /proc reads and never wait on
            // the PTY actor thread.
            foreground_cwd: ws
                .foreground_cwd_for_pane(pane_id, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::*;
    use shepr_config::ServerConfig;
    use shepr_mux::workspace::Workspace;

    fn app_with_two_panes() -> crate::app::App {
        let mut app = crate::app::App::new(&ServerConfig::default(), crate::app::AppPolicy::Test);
        let mut workspace = Workspace::test_new("snapshot");
        workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_bookmark_index(Some(0));
        app
    }

    #[test]
    fn session_snapshot_lists_the_session_without_naming_a_focus() {
        let app = app_with_two_panes();
        let snapshot = app.session_snapshot();

        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.panes.len(), 2);
        // Focus is each client's own location, not part of the shared
        // session: the snapshot's types have no focus field to set.
    }
}
