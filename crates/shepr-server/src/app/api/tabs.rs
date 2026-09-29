use shepr_api::error::ApiErrorCode;

use crate::app::App;
use shepr_protocol::command::{
    EndpointReply, TabCreateParams, TabMoveParams, TabRenameParams, TabTarget,
};

use super::super::api_helpers::{active_workspace_not_found, tab_not_found, workspace_not_found};
use super::EndpointResult;
use super::responses::failure;

impl App {
    pub(super) fn handle_tab_create(&mut self, params: TabCreateParams) -> EndpointResult {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus,
            label,
            env,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return Err(workspace_not_found(&workspace_id));
            };
            ws_idx
        } else if let Some(active) = self.state.active_index() {
            active
        } else {
            return Err(active_workspace_not_found());
        };
        let cwd = match cwd.as_deref().map(super::cwd::launch_cwd).transpose()? {
            Some(cwd) => cwd,
            None => self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx)),
        };
        let (rows, cols) = self.state.pane_geometry().sole_pane_size();
        let default_shell = self.state.settings.default_shell.clone();
        let scrollback_limit_bytes = self.state.settings.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let spawn = self.pane_spawn_handles();
        let extra_env = super::env::normalize_launch_env(env)?;
        let result = self
            .state
            .workspaces
            .get(ws_idx)
            .ok_or_else(|| std::io::Error::other("workspace disappeared"))
            .and_then(|ws| {
                ws.create_tab(
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    shepr_mux::pane::PaneShellConfig::new(
                        &default_shell,
                        self.state.settings.login_shell,
                    ),
                    extra_env,
                    &spawn,
                )
            });
        match result {
            Ok((tab, terminal, runtime)) => {
                let terminal_id = terminal.id.clone();
                let Some(outcome) = self.state.commit_tab_creation(ws_idx, tab, terminal, focus)
                else {
                    drop(runtime);
                    return failure(ApiErrorCode::TabCreateFailed, "workspace disappeared");
                };
                let tab_idx = outcome.tab_index;
                self.terminal_runtimes.insert(terminal_id, runtime);
                if let Some(label) = label {
                    if let Some(workspace) = self.state.workspaces.get_mut(ws_idx) {
                        workspace.set_tab_custom_name(tab_idx, Some(label));
                    }
                    if let (Some(workspace_id), Some(tab_id)) = (
                        self.public_workspace_id(ws_idx),
                        self.public_tab_id(ws_idx, tab_idx),
                    ) {
                        crate::logging::tab_renamed(&workspace_id, &tab_id);
                    }
                }
                self.schedule_session_save();
                if self.public_tab_id(ws_idx, tab_idx).is_none() {
                    return failure(ApiErrorCode::TabCreateFailed, "new tab is unavailable");
                }
                Ok(EndpointReply::Done)
            }
            Err(err) => failure(ApiErrorCode::TabCreateFailed, err.to_string()),
        }
    }

    pub(super) fn handle_tab_focus(&mut self, target: &TabTarget) -> EndpointResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return Err(tab_not_found(&target.tab_id));
        };
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        if self.public_tab_id(ws_idx, tab_idx).is_none() {
            return Err(tab_not_found(&target.tab_id));
        }

        Ok(EndpointReply::Done)
    }

    pub(super) fn handle_tab_rename(&mut self, params: TabRenameParams) -> EndpointResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return Err(tab_not_found(&params.tab_id));
        };
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return Err(tab_not_found(&params.tab_id));
        };
        let Some(workspace_id) = self.state.workspaces.get(ws_idx).map(|ws| ws.id.clone()) else {
            return Err(tab_not_found(&params.tab_id));
        };
        let Some(workspace) = self.state.workspaces.get_mut(ws_idx) else {
            return Err(tab_not_found(&params.tab_id));
        };
        if !workspace.set_tab_custom_name(tab_idx, Some(params.label)) {
            return Err(tab_not_found(&params.tab_id));
        }
        crate::logging::tab_renamed(&workspace_id, &tab_id);
        self.schedule_session_save();

        Ok(EndpointReply::Done)
    }

    pub(super) fn handle_tab_move(&mut self, params: &TabMoveParams) -> EndpointResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return Err(tab_not_found(&params.tab_id));
        };
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Err(tab_not_found(&params.tab_id));
        };
        if params.insert_index > ws.tabs().len() {
            return failure(
                ApiErrorCode::TabMoveFailed,
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let insert_index = params.insert_index;
        let moved = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.move_tab(tab_idx, insert_index));
        if moved {
            self.state.refresh_active_tab_id();
            self.schedule_session_save();
        }

        Ok(EndpointReply::Done)
    }

    pub(super) fn handle_tab_close(&mut self, target: &TabTarget) -> EndpointResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return Err(tab_not_found(&target.tab_id));
        };
        if self.public_tab_id(ws_idx, tab_idx).is_none() {
            return Err(tab_not_found(&target.tab_id));
        }
        let Some(plan) = self.state.prepare_tab_removal(ws_idx, tab_idx) else {
            return Err(tab_not_found(&target.tab_id));
        };
        let crate::app::actions::TabRemovalCommit::Removed(outcome) =
            self.state.commit_tab_removal(&plan)
        else {
            return failure(
                ApiErrorCode::TabCloseFailed,
                format!("tab {} could not be closed", target.tab_id),
            );
        };
        self.shutdown_detached_terminal_runtimes(&outcome.detached_terminal_ids);
        self.schedule_session_save();

        Ok(EndpointReply::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::test_support::*;
    use shepr_config::Config;
    use shepr_mux::workspace::Workspace;

    #[test]
    fn api_tab_close_last_tab_closes_workspace() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let tab_id = app.public_tab_id(0, 0).expect("test precondition");

        let response = app.handle_tab_close(&TabTarget {
            tab_id: tab_id.to_string(),
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.active_index().is_none());
    }

    #[test]
    fn api_tab_close_removes_every_pane_in_the_tab() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        let mut workspace = Workspace::test_new("tabs");
        workspace.test_split(shepr_core::layout::Direction::Horizontal);
        workspace.test_add_tab(Some("survivor"));
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let survivor_root = app.state.workspaces[0].tabs()[1].root_pane();
        let tab_id = app.public_tab_id(0, 0).expect("test precondition");

        let response = app.handle_tab_close(&TabTarget {
            tab_id: tab_id.to_string(),
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        assert_eq!(app.state.workspaces[0].tabs().len(), 1);
        assert_eq!(app.state.workspaces[0].tabs()[0].root_pane(), survivor_root);
        assert_eq!(app.state.workspaces[0].tabs()[0].panes().len(), 1);
    }

    #[test]
    fn api_tab_move_reorders_tabs_in_target_workspace() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        let mut workspace = Workspace::test_new("tabs");
        workspace.test_add_tab(Some("two"));
        workspace.test_add_tab(Some("three"));
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let moved_root = app.state.workspaces[0].tabs()[0].root_pane();
        let moved_id = app.public_tab_id(0, 0).expect("test precondition");

        let response = app.handle_tab_move(&TabMoveParams {
            tab_id: moved_id.clone().to_string(),
            insert_index: 3,
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        assert_eq!(app.state.workspaces[0].tabs()[2].root_pane(), moved_root);
        assert_eq!(
            app.public_tab_id(0, 2).expect("test precondition"),
            moved_id
        );
    }

    #[tokio::test]
    async fn tab_create_follows_cached_focused_pane_cwd_without_runtime() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.settings.default_shell = exiting_test_command().into();
        app.state.settings.login_shell = false;
        let workspace = Workspace::test_new("tabs");
        let focused_pane = workspace.tabs()[0].root_pane();
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.ensure_test_terminals();
        let scratch = crate::test_support::ScratchDir::new("cached-cwd");
        let cached_cwd = scratch.to_path_buf();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_cwd(shepr_mux::UsableCwd::new(cached_cwd.clone()).expect("test cwd is usable"));

        let response = app.handle_tab_create(TabCreateParams {
            workspace_id: None,
            cwd: None,
            focus: false,
            label: None,
            env: Default::default(),
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        let created = &app.state.workspaces[0].tabs()[1];
        let created_terminal_id = created
            .terminal_id(created.root_pane())
            .expect("test precondition");
        let created_cwd = app
            .state
            .terminals
            .get(created_terminal_id)
            .expect("test precondition")
            .cwd();
        assert_eq!(
            std::fs::canonicalize(created_cwd).unwrap_or_else(|_| created_cwd.to_path_buf()),
            std::fs::canonicalize(&cached_cwd).unwrap_or_else(|_| cached_cwd.clone())
        );
        shutdown_test_runtimes(&mut app);
    }
}
