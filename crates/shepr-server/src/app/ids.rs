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

    /// Public id of the workspace at `ws_idx`, or `None` when that index no
    /// longer names a workspace.
    pub(crate) fn public_workspace_id(&self, ws_idx: usize) -> Option<shepr_protocol::WorkspaceId> {
        self.state.workspaces.get(ws_idx).map(|ws| ws.id.clone())
    }

    pub(crate) fn public_pane_id(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<shepr_protocol::PublicPaneId> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_number = ws.public_pane_number(pane_id)?;
        Some(shepr_protocol::PublicPaneId::new(&ws.id, pane_number))
    }

    pub(super) fn pane_launch_env(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
        extra_env: Vec<(String, String)>,
    ) -> Option<shepr_mux::pane::PaneLaunchEnv> {
        let workspace = self.state.workspaces.get(ws_idx)?;
        let pane_number = workspace.public_pane_number(pane_id)?;
        let pane_id = shepr_protocol::PublicPaneId::new(&workspace.id, pane_number);
        Some(
            shepr_mux::pane::PaneLaunchEnv::from_extra(
                extra_env,
                shepr_api::socket_path(&self.paths),
            )
            .with_pane_id(pane_id),
        )
    }

    /// Resolves a public workspace id (`w<n>`) to its current index.
    ///
    /// Only the exact stable id is accepted. Positional forms (`w_N`, bare
    /// `N`) are deliberately rejected: a mistyped or index-style id must fail
    /// rather than silently target whichever workspace sits at that position.
    pub(crate) fn parse_workspace_id(&self, id: &str) -> Option<usize> {
        let public_id = id.parse::<shepr_protocol::WorkspaceId>().ok()?;
        self.resolve_workspace_id(&public_id)
    }

    /// [`Self::parse_workspace_id`] for an id that is already typed.
    pub(crate) fn resolve_workspace_id(
        &self,
        public_id: &shepr_protocol::WorkspaceId,
    ) -> Option<usize> {
        self.state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == *public_id)
    }

    /// Resolves a public pane id (`<workspace_id>:p<n>`) to (workspace index,
    /// pane).
    ///
    /// Raw internal pane ids (`p_<raw>`) are not accepted: they restart every
    /// process, so after a server restart they name a different pane. The
    /// `<workspace>-N` form is gone too; nothing emits it.
    pub(crate) fn parse_pane_id(&self, id: &str) -> Option<(usize, shepr_core::layout::PaneId)> {
        let public_id = id.parse::<shepr_protocol::PublicPaneId>().ok()?;
        self.resolve_pane_id(&public_id)
    }

    /// [`Self::parse_pane_id`] for an id that is already typed.
    pub(crate) fn resolve_pane_id(
        &self,
        public_id: &shepr_protocol::PublicPaneId,
    ) -> Option<(usize, shepr_core::layout::PaneId)> {
        let ws_idx = self.resolve_workspace_id(public_id.workspace_id())?;
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_id = ws.pane_id_for_public_number(public_id.number())?;
        Some((ws_idx, pane_id))
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::*;
    use shepr_mux::workspace::Workspace;

    fn test_app_with_workspaces(names: &[&str]) -> super::App {
        let mut app = super::App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        app.state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app
    }

    #[test]
    fn public_ids_resolve() {
        let mut app = test_app_with_workspaces(&["a", "b"]);
        let second = app.state.workspaces[1].test_split(shepr_core::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let ws_id = app.state.workspaces[1].id.clone();

        assert_eq!(app.parse_workspace_id(&ws_id), Some(1));
        let pane_id = app.public_pane_id(1, second).expect("public pane id");
        assert_eq!(app.parse_pane_id(&pane_id), Some((1, second)));
    }

    #[test]
    fn public_workspace_id_returns_none_for_a_missing_workspace() {
        let app = test_app_with_workspaces(&["a"]);

        assert_eq!(app.public_workspace_id(1), None);
    }

    #[test]
    fn unknown_public_pane_id_does_not_resolve() {
        let app = test_app_with_workspaces(&["a", "b"]);
        let retired_id = shepr_protocol::PublicPaneId::new(&retired_workspace_id(), 9);

        assert_eq!(app.parse_pane_id(&retired_id), None);
    }

    #[test]
    fn positional_and_raw_ids_are_rejected() {
        let app = test_app_with_workspaces(&["a", "b"]);
        let ws_id = app.state.workspaces[0].id.clone();
        let root = app.state.workspaces[0].root_pane();

        for id in ["1", "2", "w_1", "w_2"] {
            assert_eq!(app.parse_workspace_id(id), None, "workspace id {id:?}");
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
