use shepr_api::error::{ApiErrorCode, ApiResult};

use crate::app::{App, actions::PaneContextFallback};
use shepr_api::schema::{
    EventData, EventEnvelope, ResponseResult, WorkspaceCloseParams, WorkspaceCreateParams,
    WorkspaceMoveBlockParams, WorkspaceMoveParams, WorkspaceRenameParams,
    WorkspaceReportMetadataParams, WorkspaceTarget,
};

use super::super::api_helpers::{
    normalize_metadata_source, normalize_metadata_ttl, workspace_not_found,
};
use super::responses::{failure, success};

impl App {
    pub(super) fn handle_workspace_list(&mut self) -> ApiResult {
        success(ResponseResult::WorkspaceList {
            workspaces: self.workspace_list_info(),
        })
    }

    pub(super) fn handle_workspace_get(&mut self, target: &WorkspaceTarget) -> ApiResult {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return Err(workspace_not_found(&target.workspace_id));
        };
        let Some(workspace) = self.workspace_info(index) else {
            return Err(workspace_not_found(&target.workspace_id));
        };

        success(ResponseResult::WorkspaceInfo { workspace })
    }

    pub(super) fn handle_workspace_create(&mut self, params: WorkspaceCreateParams) -> ApiResult {
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
                    shepr_platform::logging::workspace_renamed(&workspace.id);
                }
                self.emit_workspace_open_events(index);
                match self.workspace_created_result(index) {
                    Some(result) => success(result),
                    None => failure(
                        ApiErrorCode::WorkspaceCreateFailed,
                        "new workspace is unavailable",
                    ),
                }
            }
            Err(err) => failure(ApiErrorCode::WorkspaceCreateFailed, err.to_string()),
        }
    }

    pub(super) fn handle_workspace_focus(&mut self, target: &WorkspaceTarget) -> ApiResult {
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

        success(ResponseResult::WorkspaceInfo { workspace })
    }

    pub(super) fn handle_workspace_rename(&mut self, params: WorkspaceRenameParams) -> ApiResult {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        let Some(workspace_id) = self.public_workspace_id(index) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        let Some(ws) = self.state.workspaces.get_mut(index) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        ws.set_custom_name(params.label.clone());
        shepr_platform::logging::workspace_renamed(&ws.id);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            data: EventData::WorkspaceRenamed {
                workspace_id,
                label: params.label,
            },
        });
        let Some(workspace) = self.workspace_info(index) else {
            return Err(workspace_not_found(&params.workspace_id));
        };

        success(ResponseResult::WorkspaceInfo { workspace })
    }

    pub(super) fn handle_workspace_move(&mut self, params: &WorkspaceMoveParams) -> ApiResult {
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

        let Some(workspace_id) = self.public_workspace_id(index) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        let insert_index = params.insert_index;
        let moved = self.state.move_workspace(index, insert_index);
        let workspaces = self.workspace_list_info();
        if moved {
            self.emit_event(EventEnvelope {
                data: EventData::WorkspaceMoved {
                    workspace_id,
                    insert_index,
                    workspaces: workspaces.clone(),
                },
            });
        }

        success(ResponseResult::WorkspaceList { workspaces })
    }

    pub(super) fn handle_workspace_move_block(
        &mut self,
        params: WorkspaceMoveBlockParams,
    ) -> ApiResult {
        if params.workspace_ids.is_empty() {
            return failure(
                ApiErrorCode::WorkspaceMoveBlockFailed,
                "workspace_ids must not be empty",
            );
        }

        let mut workspace_ids = Vec::with_capacity(params.workspace_ids.len());
        let mut seen_ids = std::collections::HashSet::new();
        for requested_id in &params.workspace_ids {
            let Some(index) = self.parse_workspace_id(requested_id) else {
                return Err(workspace_not_found(requested_id));
            };
            let Some(workspace) = self.state.workspaces.get(index) else {
                return Err(workspace_not_found(requested_id));
            };
            if !seen_ids.insert(workspace.id.to_string()) {
                return failure(
                    ApiErrorCode::WorkspaceMoveBlockFailed,
                    format!("workspace {requested_id} appears more than once"),
                );
            }
            workspace_ids.push(workspace.id.to_string());
        }

        let before_workspace_id = match params.before_workspace_id {
            Some(requested_id) => {
                let Some(index) = self.parse_workspace_id(&requested_id) else {
                    return Err(workspace_not_found(&requested_id));
                };
                let Some(workspace) = self.state.workspaces.get(index) else {
                    return Err(workspace_not_found(&requested_id));
                };
                if seen_ids.contains(workspace.id.as_str()) {
                    return failure(
                        ApiErrorCode::WorkspaceMoveBlockFailed,
                        "before_workspace_id must not be part of workspace_ids",
                    );
                }
                Some(workspace.id.to_string())
            }
            None => None,
        };

        let moved = self
            .state
            .move_workspace_block(&workspace_ids, before_workspace_id.as_deref());
        let workspaces = self.workspace_list_info();
        if moved {
            self.emit_event(EventEnvelope {
                data: EventData::WorkspaceReordered {
                    workspace_ids,
                    before_workspace_id,
                    workspaces: workspaces.clone(),
                },
            });
        }

        success(ResponseResult::WorkspaceList { workspaces })
    }

    pub(super) fn handle_workspace_report_metadata(
        &mut self,
        params: WorkspaceReportMetadataParams,
    ) -> ApiResult {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        let source = match normalize_metadata_source(&params.source) {
            Ok(source) => source,
            Err(message) => return failure(ApiErrorCode::InvalidMetadataSource, message),
        };
        let ttl = match normalize_metadata_ttl(params.ttl_ms) {
            Ok(ttl) => ttl,
            Err(message) => return failure(ApiErrorCode::InvalidMetadataTtl, message),
        };
        let tokens = match super::super::api_helpers::normalize_metadata_tokens(params.tokens) {
            Ok(tokens) => tokens,
            Err(message) => return failure(ApiErrorCode::InvalidMetadataToken, message),
        };
        let Some(workspace) = self.state.workspaces.get_mut(index) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        let now = self.clock.now;
        if !shepr_mux::terminal::metadata_tokens::sequence_is_fresh(
            &workspace.metadata_token_sequences,
            &source,
            params.seq,
            now,
        ) {
            return success(ResponseResult::Ok {});
        }
        if workspace.metadata_tokens.key_count_after_patch(&tokens)
            > super::super::api_helpers::MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
        {
            return failure(
                ApiErrorCode::MetadataTokenLimit,
                format!(
                    "workspace metadata may contain at most {} tokens",
                    super::super::api_helpers::MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
                ),
            );
        }
        match shepr_mux::terminal::metadata_tokens::accept_sequence(
            &mut workspace.metadata_token_sequences,
            &source,
            params.seq,
            now,
        ) {
            Ok(true) => {}
            Ok(false) => return success(ResponseResult::Ok {}),
            Err(()) => {
                return failure(
                    ApiErrorCode::MetadataSequenceSourceLimit,
                    format!(
                        "workspace metadata may track at most {} sequenced sources",
                        shepr_mux::terminal::metadata_tokens::MAX_SEQUENCE_SOURCES
                    ),
                );
            }
        }
        let changed = workspace.metadata_tokens.patch(tokens, ttl, now);
        if changed {
            self.sync_agent_metadata_deadline();
            self.emit_workspace_token_updated(index);
        }
        success(ResponseResult::Ok {})
    }

    pub(super) fn handle_workspace_close(&mut self, params: &WorkspaceCloseParams) -> ApiResult {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return Err(workspace_not_found(&params.workspace_id));
        };
        if self.state.workspaces.get(index).is_none() {
            return Err(workspace_not_found(&params.workspace_id));
        }
        let close_events = self.workspace_close_events(index);
        self.state.close_workspace_at(index);
        self.shutdown_detached_terminal_runtimes();
        self.emit_events(close_events);

        success(ResponseResult::Ok {})
    }

    fn workspace_list_info(&self) -> Vec<shepr_api::schema::WorkspaceInfo> {
        (0..self.state.workspaces.len())
            .filter_map(|idx| self.workspace_info(idx))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_api::schema::{ErrorResponse, SuccessResponse};
    use shepr_config::Config;
    use shepr_mux::workspace::Workspace;

    // `new_cwd = follow` must anchor on the focused pane for every creation
    // surface. Splits and tabs already do; a new workspace must follow the
    // focused pane too, not the source workspace's first-tab root pane.
    #[tokio::test]
    async fn workspace_create_follows_focused_pane_cwd_not_first_tab_root() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            shepr_api::EventHub::default(),
        );
        app.state.settings.default_shell = exiting_test_command().into();
        app.state.settings.login_shell = false;
        app.state.workspaces = vec![Workspace::test_new("spaces")];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.ensure_test_terminals();

        // Second tab becomes the focused pane, away from tab 1's root pane.
        let response = app.handle_tab_create(shepr_api::schema::TabCreateParams {
            workspace_id: None,
            cwd: None,
            focus: true,
            label: None,
            env: Default::default(),
        });
        let _: SuccessResponse = crate::test_support::test_success(&response);
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

        let success: SuccessResponse = crate::test_support::test_success(&response);
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceCreated { .. }
        ));
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
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            shepr_api::EventHub::default(),
        );
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
            source_workspace_id: Some(source_workspace_id),
            cwd: None,
            focus: false,
            label: None,
            env: Default::default(),
        });
        let success: SuccessResponse = crate::test_support::test_success(&response);
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceCreated { .. }
        ));
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
        let error: ErrorResponse = crate::test_support::test_error(&invalid);
        assert_eq!(error.error.code, "workspace_not_found");

        let captured = app.handle_workspace_create(WorkspaceCreateParams {
            source_workspace_id: Some("w_999".into()),
            cwd: Some(source_cwd.display().to_string()),
            focus: false,
            label: None,
            env: Default::default(),
        });
        let success: SuccessResponse = crate::test_support::test_success(&captured);
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceCreated { .. }
        ));
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
    fn workspace_metadata_tokens_patch_clear_and_emit_snapshot() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("one")];
        let workspace_id = app.public_workspace_id(0).expect("test precondition");

        for (tokens, expected) in [
            (
                std::collections::HashMap::from([
                    ("summary".into(), Some("reviewing auth".into())),
                    ("jj_status".into(), Some("2 changes".into())),
                ]),
                std::collections::HashMap::from([
                    ("summary".into(), "reviewing auth".into()),
                    ("jj_status".into(), "2 changes".into()),
                ]),
            ),
            (
                std::collections::HashMap::from([
                    ("summary".into(), Some("done".into())),
                    ("jj_status".into(), None),
                ]),
                std::collections::HashMap::from([("summary".into(), "done".into())]),
            ),
        ] {
            let response = app.handle_api_request(shepr_api::schema::Request {
                id: "req".into(),
                method: shepr_api::schema::Method::WorkspaceReportMetadata(
                    WorkspaceReportMetadataParams {
                        workspace_id: workspace_id.clone(),
                        source: "user:test".into(),
                        tokens,
                        seq: None,
                        ttl_ms: None,
                    },
                ),
            });
            let success: SuccessResponse = crate::test_support::test_success(&response);
            assert_eq!(success.result, ResponseResult::Ok {});
            assert_eq!(
                app.workspace_info(0).expect("test precondition").tokens,
                expected
            );
        }

        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::WorkspaceMetadataUpdated { workspace }
                if workspace.tokens.get("summary").map(String::as_str) == Some("done")
                    && !workspace.tokens.contains_key("jj_status")
        )));
    }

    #[test]
    fn workspace_token_ttl_expires_through_runtime_and_emits_update() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("one")];
        let workspace_id = app.public_workspace_id(0).expect("test precondition");
        let response = app.handle_workspace_report_metadata(WorkspaceReportMetadataParams {
            workspace_id,
            source: "user:test".into(),
            tokens: std::collections::HashMap::from([("summary".into(), Some("temporary".into()))]),
            seq: None,
            ttl_ms: Some(1),
        });
        let _: SuccessResponse = crate::test_support::test_success(&response);
        let deadline = app.agent_metadata_deadline.expect("token deadline");

        app.expire_metadata_at(deadline, deadline);

        assert!(
            app.workspace_info(0)
                .expect("test precondition")
                .tokens
                .is_empty()
        );
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::WorkspaceMetadataUpdated { workspace } if workspace.tokens.is_empty()
        )));
    }

    #[test]
    fn workspace_info_for_a_stale_index_is_none_and_emits_nothing() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("one")];

        assert!(app.workspace_info(0).is_some());
        assert!(app.workspace_info(1).is_none());
        app.emit_workspace_token_updated(1);
        assert!(event_hub.events_after(0).is_empty());
    }

    #[test]
    fn api_workspace_move_reorders_workspaces() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
        ];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let moved_id = app.public_workspace_id(0).expect("test precondition");

        let response = app.handle_workspace_move(&WorkspaceMoveParams {
            workspace_id: moved_id.clone(),
            insert_index: 3,
        });

        let success: SuccessResponse = crate::test_support::test_success(&response);
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(workspaces[2].workspace_id, moved_id);
        assert_eq!(app.state.workspaces[2].display_name(), "one");
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceMoved {
                    workspace_id,
                    insert_index: 3,
                    workspaces,
                } if workspace_id == &moved_id
                    && workspaces[2].workspace_id == moved_id
            )
        }));
    }

    #[test]
    fn api_workspace_move_block_reorders_atomically() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![
            Workspace::test_new("child"),
            Workspace::test_new("normal"),
            Workspace::test_new("parent"),
            Workspace::test_new("tail"),
        ];
        let parent_id = app.public_workspace_id(2).expect("test precondition");
        let child_id = app.public_workspace_id(0).expect("test precondition");
        let tail_id = app.public_workspace_id(3).expect("test precondition");

        let response = app.handle_workspace_move_block(WorkspaceMoveBlockParams {
            workspace_ids: vec![parent_id.clone(), child_id.clone()],
            before_workspace_id: Some(tail_id.clone()),
        });

        let success: SuccessResponse = crate::test_support::test_success(&response);
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(shepr_mux::workspace::Workspace::display_name)
                .collect::<Vec<_>>(),
            ["normal", "parent", "child", "tail"]
        );
        assert_eq!(workspaces[1].workspace_id, parent_id);
        assert_eq!(workspaces[2].workspace_id, child_id);
        let events = event_hub.events_after(0);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].1.data,
            EventData::WorkspaceReordered {
                workspace_ids,
                before_workspace_id,
                workspaces,
            } if workspace_ids.first() == Some(&parent_id)
                && workspace_ids.get(1) == Some(&child_id)
                && workspace_ids.len() == 2
                && before_workspace_id.as_deref() == Some(tail_id.as_str())
                && workspaces[1].workspace_id == parent_id
        ));
    }

    #[test]
    fn api_workspace_close_announces_panes_and_tabs_before_the_workspace() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            event_hub.clone(),
        );
        let mut closing = Workspace::test_new("closing");
        closing.test_add_tab(Some("second"));
        app.state.workspaces = vec![closing, Workspace::test_new("survivor")];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let workspace_id = app.public_workspace_id(0).expect("test precondition");
        let pane_ids = app.state.workspaces[0]
            .tabs()
            .iter()
            .map(|tab| {
                app.public_pane_id(0, tab.root_pane())
                    .expect("test precondition")
            })
            .collect::<Vec<_>>();
        let tab_ids = [
            app.public_tab_id(0, 0).expect("test precondition"),
            app.public_tab_id(0, 1).expect("test precondition"),
        ];

        let response = app.handle_workspace_close(&WorkspaceCloseParams {
            workspace_id: workspace_id.clone(),
        });

        let success: SuccessResponse = crate::test_support::test_success(&response);
        assert_eq!(success.result, ResponseResult::Ok {});
        let events = event_hub
            .events_after(0)
            .into_iter()
            .map(|(_, event)| event.data)
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 5);
        for (index, tab_id) in tab_ids.iter().enumerate() {
            assert!(matches!(
                &events[index * 2],
                EventData::PaneClosed { pane_id, .. } if pane_id == &pane_ids[index]
            ));
            assert!(matches!(
                &events[index * 2 + 1],
                EventData::TabClosed { tab_id: closed, .. } if closed == tab_id
            ));
        }
        assert!(matches!(
            &events[4],
            EventData::WorkspaceClosed { workspace_id: closed, .. } if closed == &workspace_id
        ));
    }

    #[test]
    fn api_workspace_move_noop_does_not_emit_event() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        let moved_id = app.public_workspace_id(0).expect("test precondition");

        let response = app.handle_workspace_move(&WorkspaceMoveParams {
            workspace_id: moved_id.clone(),
            insert_index: 1,
        });

        let success: SuccessResponse = crate::test_support::test_success(&response);
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(workspaces[0].workspace_id, moved_id);
        assert!(event_hub.events_after(0).is_empty());
    }
}
