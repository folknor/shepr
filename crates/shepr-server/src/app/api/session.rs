use crate::app::App;
use shepr_api::schema::SessionSnapshot;

impl App {
    /// The session's workspaces, tabs, panes and agents, which the server
    /// projects into each client shell's snapshot.
    pub(crate) fn session_snapshot(&self) -> SessionSnapshot {
        let focused_workspace_id = self.state.active.clone();
        let focused_tab_id = self.state.active_tab_id.clone();
        let focused_pane_id = self.state.active_index().and_then(|ws_idx| {
            let ws = self.state.workspaces.get(ws_idx)?;
            self.public_pane_id(ws_idx, ws.focused_pane_id())
        });

        let mut workspaces = Vec::new();
        let mut tabs = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            workspaces.extend(self.workspace_info(ws_idx));
            for tab_idx in 0..ws.tabs().len() {
                if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                    tabs.push(tab);
                }
            }
        }

        SessionSnapshot {
            version: shepr_protocol::build_version(),
            focused_workspace_id,
            focused_tab_id,
            focused_pane_id,
            workspaces,
            tabs,
            panes: self.collect_panes(),
            agents: self.collect_agent_infos(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::*;
    use shepr_config::Config;
    use shepr_mux::workspace::Workspace;

    fn app_with_two_tabs() -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        let mut workspace = Workspace::test_new("snapshot");
        workspace.test_add_tab(None);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app
    }

    #[test]
    fn session_snapshot_lists_the_session_and_its_focus() {
        let app = app_with_two_tabs();
        let snapshot = app.session_snapshot();

        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.tabs.len(), 2);
        assert_eq!(snapshot.panes.len(), 2);
        assert_eq!(
            snapshot.focused_workspace_id.as_deref(),
            Some(snapshot.workspaces[0].workspace_id.as_str())
        );
        assert_eq!(
            snapshot.focused_tab_id.as_deref(),
            Some(snapshot.tabs[0].tab_id.as_str())
        );
        assert_eq!(
            snapshot.focused_pane_id.as_deref(),
            Some(snapshot.panes[0].pane_id.as_str())
        );
    }
}
