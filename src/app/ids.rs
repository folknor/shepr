use super::App;

impl App {
    pub(crate) fn find_pane(
        &self,
        pane_id: crate::layout::PaneId,
    ) -> Option<(usize, &crate::pane::PaneState)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| ws.pane_state(pane_id).map(|pane| (ws_idx, pane)))
    }

    pub(crate) fn public_workspace_id(&self, ws_idx: usize) -> String {
        self.state.workspaces[ws_idx].id.clone()
    }

    pub(crate) fn public_tab_id(&self, ws_idx: usize, tab_idx: usize) -> Option<String> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_number = ws.public_tab_number(tab_idx)?;
        Some(crate::workspace::public_tab_id_for_number(
            &ws.id, tab_number,
        ))
    }

    pub(crate) fn public_pane_id(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<String> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_number = ws.public_pane_number(pane_id)?;
        Some(crate::workspace::public_pane_id_for_number(
            &ws.id,
            pane_number,
        ))
    }

    pub(super) fn pane_launch_env(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        extra_env: Vec<(String, String)>,
    ) -> Option<crate::pane::PaneLaunchEnv> {
        let workspace_id = self.public_workspace_id(ws_idx);
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let tab_id = self.public_tab_id(ws_idx, tab_idx)?;
        let pane_id = self.public_pane_id(ws_idx, pane_id)?;
        Some(
            crate::pane::PaneLaunchEnv::from_extra(extra_env).with_identity(
                workspace_id,
                tab_id,
                pane_id,
            ),
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
        let (ws_raw, tab_raw) = id.rsplit_once(':')?;
        let ws_idx = self.parse_workspace_id(ws_raw)?;
        let encoded = tab_raw.strip_prefix('t')?;
        let tab_number = crate::workspace::decode_public_number(encoded)?;
        let tab_idx = self
            .state
            .workspaces
            .get(ws_idx)?
            .tabs
            .iter()
            .position(|tab| tab.number == tab_number)?;
        Some((ws_idx, tab_idx))
    }

    /// Resolves a public pane id (`<workspace_id>:p<n>`, or the pre-move id of
    /// a pane that moved to another workspace) to (workspace index, pane).
    ///
    /// Raw internal pane ids (`p_<raw>`) are not accepted: they restart every
    /// process, so after a server restart they name a different pane. The
    /// `<workspace>-N` form is gone too; nothing emits it.
    pub(crate) fn parse_pane_id(&self, id: &str) -> Option<(usize, crate::layout::PaneId)> {
        if let Some(alias) = self.state.public_pane_id_aliases.get(id).copied() {
            return self.find_pane(alias).map(|(ws_idx, _)| (ws_idx, alias));
        }

        let (ws_raw, pane_number_raw) = id.rsplit_once(":p")?;
        let ws_idx = self.parse_workspace_id(ws_raw)?;
        let pane_number = crate::workspace::decode_public_number(pane_number_raw)?;
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_id = ws
            .public_pane_numbers
            .iter()
            .find_map(|(pane_id, number)| (*number == pane_number).then_some(*pane_id))?;
        Some((ws_idx, pane_id))
    }

    pub(crate) fn parse_current_public_pane_id(
        &self,
        id: &str,
    ) -> Option<(usize, crate::layout::PaneId)> {
        let (ws_idx, pane_id) = self.parse_pane_id(id)?;
        (self.public_pane_id(ws_idx, pane_id).as_deref() == Some(id)).then_some((ws_idx, pane_id))
    }
}

#[cfg(test)]
mod tests {
    use crate::workspace::Workspace;

    fn test_app_with_workspaces(names: &[&str]) -> super::App {
        let mut app = super::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
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
    fn positional_and_raw_ids_are_rejected() {
        let app = test_app_with_workspaces(&["a", "b"]);
        let ws_id = app.state.workspaces[0].id.clone();
        let root = app.state.workspaces[0].tabs[0].root_pane;

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
