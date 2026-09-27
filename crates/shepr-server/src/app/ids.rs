use super::App;

impl App {
    pub(crate) fn find_pane(
        &self,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<(usize, &shepr_mux::pane::PaneState)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| ws.pane_state(pane_id).map(|pane| (ws_idx, pane)))
    }

    /// Public id of the workspace at `ws_idx`. Callers pass an index they just
    /// resolved; a stale one is a caller bug, reported and answered with an
    /// empty id rather than a panic that would take the server down.
    pub(crate) fn public_workspace_id(&self, ws_idx: usize) -> String {
        match self.state.workspaces.get(ws_idx) {
            Some(ws) => ws.id.to_string(),
            None => {
                tracing::warn!(
                    ws_idx,
                    "public workspace id requested for a missing workspace"
                );
                String::new()
            }
        }
    }

    pub(crate) fn public_tab_id(&self, ws_idx: usize, tab_idx: usize) -> Option<String> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_number = ws.public_tab_number(tab_idx)?;
        Some(shepr_mux::workspace::public_tab_id_for_number(
            &ws.id, tab_number,
        ))
    }

    pub(crate) fn public_pane_id(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<String> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_number = ws.public_pane_number(pane_id)?;
        Some(shepr_mux::workspace::public_pane_id_for_number(
            &ws.id,
            pane_number,
        ))
    }

    /// The tab holding `pane_id` in workspace `ws_idx`, or `None` when either
    /// is gone. API handlers hold an index parsed from a public id earlier in
    /// the same request; looking it up rather than indexing keeps a stale
    /// index a not-found answer instead of a server panic.
    pub(crate) fn tab_index_for_pane(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<usize> {
        self.state
            .workspaces
            .get(ws_idx)?
            .find_tab_index_for_pane(pane_id)
    }

    pub(super) fn pane_launch_env(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
        extra_env: Vec<(String, String)>,
    ) -> Option<shepr_mux::pane::PaneLaunchEnv> {
        let tab_idx = self.tab_index_for_pane(ws_idx, pane_id)?;
        let workspace = self.state.workspaces.get(ws_idx)?;
        let tab_number = workspace.public_tab_number(tab_idx)?;
        let pane_number = workspace.public_pane_number(pane_id)?;
        let workspace_id = shepr_protocol::WorkspaceId::new(workspace.id.clone());
        let tab_id = shepr_protocol::PublicTabId::new(workspace.id.as_str(), tab_number);
        let pane_id = shepr_protocol::PublicPaneId::new(workspace.id.as_str(), pane_number);
        Some(
            shepr_mux::pane::PaneLaunchEnv::from_extra(extra_env)
                .with_api_socket_path(shepr_api::socket_path(&self.paths))
                .with_identity(workspace_id, tab_id, pane_id),
        )
    }

    /// Resolves a public workspace id (`w<n>`) to its current index.
    ///
    /// Only the exact stable id is accepted. Positional forms (`w_N`, bare
    /// `N`) are deliberately rejected: a mistyped or index-style id must fail
    /// rather than silently target whichever workspace sits at that position.
    pub(crate) fn parse_workspace_id(&self, id: &str) -> Option<usize> {
        self.state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == id)
    }

    /// Resolves a public tab id (`<workspace_id>:t<n>`) to (workspace, tab)
    /// indexes. Positional forms (`<workspace_id>:N`, `t_…`) are rejected for
    /// the same reason as in `parse_workspace_id`: tab numbers are stable and
    /// independent of tab order, positions are not.
    pub(crate) fn parse_tab_id(&self, id: &str) -> Option<(usize, usize)> {
        let public_id = id.parse::<shepr_protocol::PublicTabId>().ok()?;
        self.resolve_tab_id(&public_id)
    }

    pub(crate) fn resolve_tab_id(
        &self,
        public_id: &shepr_protocol::PublicTabId,
    ) -> Option<(usize, usize)> {
        let ws_idx = self.parse_workspace_id(public_id.workspace_id())?;
        let tab_idx = self
            .state
            .workspaces
            .get(ws_idx)?
            .tabs()
            .iter()
            .position(|tab| tab.number == public_id.number())?;
        Some((ws_idx, tab_idx))
    }

    /// Resolves a public pane id (`<workspace_id>:p<n>`, or the pre-move id of
    /// a pane that moved to another workspace) to (workspace index, pane).
    ///
    /// Raw internal pane ids (`p_<raw>`) are not accepted: they restart every
    /// process, so after a server restart they name a different pane. The
    /// `<workspace>-N` form is gone too; nothing emits it.
    pub(crate) fn parse_pane_id(&self, id: &str) -> Option<(usize, shepr_core::layout::PaneId)> {
        let public_id = id.parse::<shepr_protocol::PublicPaneId>().ok()?;
        let current_id = (|| {
            let ws_idx = self.parse_workspace_id(public_id.workspace_id())?;
            let pane_number = public_id.number();
            let ws = self.state.workspaces.get(ws_idx)?;
            let pane_id = ws.pane_id_for_public_number(pane_number)?;
            Some((ws_idx, pane_id))
        })();
        current_id.or_else(|| {
            let alias = self.state.public_pane_id_aliases.get(&public_id).copied()?;
            self.find_pane(alias).map(|(ws_idx, _)| (ws_idx, alias))
        })
    }

    pub(crate) fn parse_current_public_pane_id(
        &self,
        id: &str,
    ) -> Option<(usize, shepr_core::layout::PaneId)> {
        let (ws_idx, pane_id) = self.parse_pane_id(id)?;
        (self.public_pane_id(ws_idx, pane_id).as_deref() == Some(id)).then_some((ws_idx, pane_id))
    }
}

#[cfg(test)]
mod tests {
    use shepr_mux::workspace::Workspace;

    fn test_app_with_workspaces(names: &[&str]) -> super::App {
        let mut app = super::App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::TEST,
            tokio::sync::mpsc::unbounded_channel().1,
            shepr_api::EventHub::default(),
        );
        app.state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app
    }

    #[test]
    fn public_ids_resolve() {
        let mut app = test_app_with_workspaces(&["a", "b"]);
        let second = app.state.workspaces[1].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let ws_id = app.state.workspaces[1].id.clone();

        assert_eq!(app.parse_workspace_id(&ws_id), Some(1));
        let tab_id = app.public_tab_id(1, 0).expect("public tab id");
        assert_eq!(app.parse_tab_id(&tab_id), Some((1, 0)));
        let pane_id = app.public_pane_id(1, second).expect("public pane id");
        assert_eq!(app.parse_pane_id(&pane_id), Some((1, second)));
    }

    #[test]
    fn canonical_public_pane_id_wins_over_a_colliding_alias() {
        let mut app = test_app_with_workspaces(&["a", "b"]);
        let current_pane = app.state.workspaces[0].tabs()[0].root_pane;
        let moved_pane = app.state.workspaces[1].tabs()[0].root_pane;
        let current_id = app
            .public_pane_id(0, current_pane)
            .expect("test precondition");
        app.state
            .public_pane_id_aliases
            .insert(current_id.clone().into(), moved_pane);

        assert_eq!(app.parse_pane_id(&current_id), Some((0, current_pane)));
        assert_eq!(app.parse_pane_id("old-workspace:p9"), None);

        app.state
            .public_pane_id_aliases
            .insert("old-workspace:p9".into(), moved_pane);
        assert_eq!(app.parse_pane_id("old-workspace:p9"), Some((1, moved_pane)));
    }

    #[test]
    fn positional_and_raw_ids_are_rejected() {
        let app = test_app_with_workspaces(&["a", "b"]);
        let ws_id = app.state.workspaces[0].id.clone();
        let root = app.state.workspaces[0].tabs()[0].root_pane;

        for id in ["1", "2", "w_1", "w_2"] {
            assert_eq!(app.parse_workspace_id(id), None, "workspace id {id:?}");
        }
        for id in [
            format!("{ws_id}:1"),
            "t_1_1".to_string(),
            "1:t1".to_string(),
        ] {
            assert_eq!(app.parse_tab_id(&id), None, "tab id {id:?}");
        }
        for id in [
            format!("p_{}", root.raw()),
            format!("p_1_{}", root.raw()),
            format!("{ws_id}-1"),
            "1:p1".to_string(),
        ] {
            assert_eq!(app.parse_pane_id(&id), None, "pane id {id:?}");
        }
    }
}
