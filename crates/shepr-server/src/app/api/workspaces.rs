use shepr_api::error::ApiErrorCode;

use crate::app::{App, actions::PaneContextFallback};
use shepr_protocol::command::{
    EndpointReply, WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceMoveParams,
    WorkspaceRenameParams, WorkspaceTarget,
};

use super::super::api_helpers::workspace_not_found;
use super::EndpointResult;
use super::responses::failure;

impl App {
    pub(super) fn handle_workspace_create(
        &mut self,
        params: WorkspaceCreateParams,
    ) -> EndpointResult {
        let explicit_cwd = params
            .cwd
            .as_deref()
            .map(super::cwd::launch_cwd)
            .transpose()?;
        let source_context = if explicit_cwd.is_some() {
            None
        } else {
            match params.source_workspace_id.as_deref() {
                Some(workspace_id) => match self
                    .parse_workspace_id(workspace_id)
                    .filter(|index| self.state.workspaces.get(*index).is_some())
                {
                    Some(index) => self.state.resolve_pane_context(
                        None,
                        Some(index),
                        PaneContextFallback::None,
                    ),
                    None => return Err(workspace_not_found(workspace_id)),
                },
                None => self.state.resolve_pane_context(
                    None,
                    None,
                    PaneContextFallback::WorkspaceCreation,
                ),
            }
        };
        let cwd = explicit_cwd.unwrap_or_else(|| {
            source_context.map_or_else(
                || self.resolve_new_terminal_cwd(None),
                |context| {
                    self.resolved_new_workspace_cwd_from_tab(
                        context.workspace_index,
                        Some(context.tab_index),
                    )
                },
            )
        });
        let extra_env = super::env::normalize_launch_env(params.env)?;
        match self.create_workspace_with_launch_env(&cwd, params.focus, extra_env) {
            Ok(index) => {
                if let Some(label) = params.label
                    && let Some(workspace) = self.state.workspaces.get_mut(index)
                {
                    workspace.set_custom_name(label);
                    crate::logging::workspace_renamed(&workspace.id);
                }
                if self.workspace_info(index).is_none() {
                    return failure(
                        ApiErrorCode::WorkspaceCreateFailed,
                        "new workspace is unavailable",
                    );
                }
                Ok(EndpointReply::Done)
            }
            Err(err) => failure(ApiErrorCode::WorkspaceCreateFailed, err.to_string()),
        }
    }

    pub(super) fn handle_workspace_focus(&mut self, target: &WorkspaceTarget) -> EndpointResult {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return Err(workspace_not_found(&target.workspace_id));
        };
        if self.state.workspaces.get(index).is_none() {
            return Err(workspace_not_found(&target.workspace_id));
        }
        self.state.switch_workspace(index);
        let Some(workspace) = self.workspace_info(index) else {
            return Err(workspace_not_found(&target.workspace_id));
        };

        Ok(EndpointReply::WorkspaceInfo { workspace })
    }

    pub(super) fn handle_workspace_rename(
        &mut self,
        params: WorkspaceRenameParams,
    ) -> EndpointResult {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        let Some(ws) = self.state.workspaces.get_mut(index) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        ws.set_custom_name(params.label);
        crate::logging::workspace_renamed(&ws.id);
        self.schedule_session_save();
        let Some(workspace) = self.workspace_info(index) else {
            return Err(workspace_not_found(&params.workspace_id));
        };

        Ok(EndpointReply::WorkspaceInfo { workspace })
    }

    pub(super) fn handle_workspace_move(&mut self, params: &WorkspaceMoveParams) -> EndpointResult {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        if self.state.workspaces.get(index).is_none() {
            return Err(workspace_not_found(&params.workspace_id));
        }
        if params.insert_index > self.state.workspaces.len() {
            return failure(
                ApiErrorCode::WorkspaceMoveFailed,
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        // A no-op move (the workspace already sits there) still succeeds.
        self.state.move_workspace(index, params.insert_index);

        Ok(EndpointReply::Done)
    }

    pub(super) fn handle_workspace_close(
        &mut self,
        params: &WorkspaceCloseParams,
    ) -> EndpointResult {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        if self.state.workspaces.get(index).is_none() {
            return Err(workspace_not_found(&params.workspace_id));
        }
        if let Some(outcome) = self.state.close_workspace_at(index) {
            self.shutdown_detached_terminal_runtimes(&outcome.detached_terminal_ids);
        }

        Ok(EndpointReply::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_config::Config;
    use shepr_mux::workspace::Workspace;

    // `new_cwd = follow` must anchor on the focused pane for every creation
    // surface. Splits and tabs already do; a new workspace must follow the
    // focused pane too, not the source workspace's first-tab root pane.
    #[tokio::test]
    async fn workspace_create_follows_focused_pane_cwd_not_first_tab_root() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.settings.default_shell = exiting_test_command().into();
        app.state.settings.login_shell = false;
        app.state.workspaces = vec![Workspace::test_new("spaces")];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.ensure_test_terminals();

        // Second tab becomes the focused pane, away from tab 1's root pane.
        app.handle_tab_create(shepr_protocol::command::TabCreateParams {
            workspace_id: None,
            cwd: None,
            focus: true,
            label: None,
            env: Default::default(),
        })
        .expect("the tab is created");
        // Drop runtimes so cwd resolution deterministically uses cached state.
        shutdown_test_runtimes(&mut app);

        let focused_scratch = crate::test_support::ScratchDir::new("ws-follow");
        let focused_cwd = focused_scratch.to_path_buf();
        let ws = &app.state.workspaces[0];
        let root_cwd = ws.identity_cwd.clone();
        let focused_pane = ws.focused_pane_id();
        assert_ne!(focused_pane, ws.tabs()[0].root_pane());
        let terminal_id = ws
            .terminal_id(focused_pane)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_cwd(shepr_mux::UsableCwd::new(focused_cwd.clone()).expect("test cwd is usable"));

        let response = app.handle_workspace_create(WorkspaceCreateParams {
            source_workspace_id: None,
            cwd: None,
            focus: false,
            label: None,
            env: Default::default(),
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        let created_cwd = &app.state.workspaces[1].identity_cwd;
        assert_eq!(
            std::fs::canonicalize(created_cwd).unwrap_or_else(|_| created_cwd.clone()),
            std::fs::canonicalize(&focused_cwd).unwrap_or_else(|_| focused_cwd.clone())
        );
        assert_ne!(
            std::fs::canonicalize(created_cwd).unwrap_or_else(|_| created_cwd.clone()),
            std::fs::canonicalize(&root_cwd).unwrap_or_else(|_| root_cwd.clone())
        );
        shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn workspace_create_uses_explicit_source_workspace() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.settings.default_shell = exiting_test_command().into();
        app.state.settings.login_shell = false;
        app.state.workspaces = vec![Workspace::test_new("first"), Workspace::test_new("source")];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.ensure_test_terminals();
        shutdown_test_runtimes(&mut app);

        // This test leaves its unique ScratchDir in place; it performs no recursive delete.
        let source_scratch = crate::test_support::ScratchDir::new("ws-source");
        let source_cwd = source_scratch.to_path_buf();
        let pane_id = app.state.workspaces[1].focused_pane_id();
        let terminal_id = app.state.workspaces[1]
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_cwd(shepr_mux::UsableCwd::new(source_cwd.clone()).expect("test cwd is usable"));
        let source_workspace_id = app.public_workspace_id(1).expect("test precondition");

        let response = app.handle_workspace_create(WorkspaceCreateParams {
            source_workspace_id: Some(source_workspace_id.to_string()),
            cwd: None,
            focus: false,
            label: None,
            env: Default::default(),
        });
        assert_eq!(response, Ok(EndpointReply::Done));
        assert_eq!(
            std::fs::canonicalize(&app.state.workspaces[2].identity_cwd).unwrap_or_else(|_| app
                .state
                .workspaces[2]
                .identity_cwd
                .clone()),
            std::fs::canonicalize(&source_cwd).unwrap_or_else(|_| source_cwd.clone())
        );

        let invalid = app.handle_workspace_create(WorkspaceCreateParams {
            source_workspace_id: Some("w_999".into()),
            cwd: None,
            focus: false,
            label: None,
            env: Default::default(),
        });
        assert_eq!(
            invalid.expect_err("an unknown source is refused").code,
            ApiErrorCode::WorkspaceNotFound
        );

        let captured = app.handle_workspace_create(WorkspaceCreateParams {
            source_workspace_id: Some("w_999".into()),
            cwd: Some(source_cwd.display().to_string()),
            focus: false,
            label: None,
            env: Default::default(),
        });
        assert_eq!(captured, Ok(EndpointReply::Done));
        assert_eq!(
            std::fs::canonicalize(&app.state.workspaces[3].identity_cwd).unwrap_or_else(|_| app
                .state
                .workspaces[3]
                .identity_cwd
                .clone()),
            std::fs::canonicalize(&source_cwd).unwrap_or_else(|_| source_cwd.clone())
        );
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn workspace_info_for_a_stale_index_is_none() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.workspaces = vec![Workspace::test_new("one")];

        assert!(app.workspace_info(0).is_some());
        assert!(app.workspace_info(1).is_none());
    }

    #[test]
    fn api_workspace_move_reorders_workspaces() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.workspaces = vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
        ];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let moved_id = app.public_workspace_id(0).expect("test precondition");

        let response = app.handle_workspace_move(&WorkspaceMoveParams {
            workspace_id: moved_id.clone().to_string(),
            insert_index: 3,
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        assert_eq!(
            app.public_workspace_id(2).expect("test precondition"),
            moved_id
        );
        assert_eq!(app.state.workspaces[2].display_name(), "one");
    }

    #[test]
    fn api_workspace_close_removes_the_workspace_with_all_its_tabs() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        let mut closing = Workspace::test_new("closing");
        closing.test_add_tab(Some("second"));
        app.state.workspaces = vec![closing, Workspace::test_new("survivor")];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let workspace_id = app.public_workspace_id(0).expect("test precondition");

        let response = app.handle_workspace_close(&WorkspaceCloseParams {
            workspace_id: workspace_id.to_string(),
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].display_name(), "survivor");
    }

    #[test]
    fn api_workspace_move_noop_leaves_the_order_unchanged() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        let moved_id = app.public_workspace_id(0).expect("test precondition");

        let response = app.handle_workspace_move(&WorkspaceMoveParams {
            workspace_id: moved_id.clone().to_string(),
            insert_index: 1,
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        assert_eq!(
            app.public_workspace_id(0).expect("test precondition"),
            moved_id
        );
        assert_eq!(app.state.workspaces[0].display_name(), "one");
    }
}
