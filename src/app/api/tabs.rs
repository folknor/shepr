use shepr_api::error::{ApiErrorCode, ApiResult};
use std::path::PathBuf;

use crate::app::App;
#[cfg(test)]
use shepr_api::schema::EventKind;
use shepr_api::schema::{
    EventData, EventEnvelope, ResponseResult, TabCreateParams, TabListParams, TabMoveParams,
    TabRenameParams, TabTarget,
};

use super::responses::{failure, success};

impl App {
    pub(super) fn handle_tab_list(&mut self, id: String, params: TabListParams) -> ApiResult {
        let tabs = if let Some(workspace_id) = params.workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            let Some(_) = self.state.workspaces.get(ws_idx) else {
                return workspace_not_found(id, &workspace_id);
            };
            self.tab_list_info(ws_idx)
        } else {
            let mut tabs = Vec::new();
            for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                for tab_idx in 0..ws.tabs.len() {
                    if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                        tabs.push(tab);
                    }
                }
            }
            tabs
        };

        success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_get(&mut self, id: String, target: &TabTarget) -> ApiResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };

        success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_create(&mut self, id: String, params: TabCreateParams) -> ApiResult {
        let TabCreateParams {
            workspace_id,
            cwd,
            focus,
            label,
            env,
        } = params;
        let ws_idx = if let Some(workspace_id) = workspace_id {
            let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                return workspace_not_found(id, &workspace_id);
            };
            ws_idx
        } else if let Some(active) = self.state.active_index() {
            active
        } else {
            return failure(id, ApiErrorCode::WorkspaceNotFound, "no active workspace");
        };
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| {
            self.resolve_new_terminal_cwd(self.focused_pane_cwd_in_workspace(ws_idx))
        });
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
                    crate::pane::PaneShellConfig::new(
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
                    return failure(id, ApiErrorCode::TabCreateFailed, "workspace disappeared");
                };
                let tab_idx = outcome.tab_index;
                self.terminal_runtimes.insert(terminal_id, runtime);
                if let Some(label) = label {
                    let workspace_id = self.public_workspace_id(ws_idx);
                    let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
                        crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
                    });
                    if let Some(tab) = self
                        .state
                        .workspaces
                        .get_mut(ws_idx)
                        .and_then(|ws| ws.tabs.get_mut(tab_idx))
                    {
                        tab.set_custom_name(label);
                        shepr_platform::logging::tab_renamed(&workspace_id, &tab_id);
                    }
                }
                self.schedule_session_save();
                self.emit_tab_created_events(ws_idx, tab_idx);
                match self.tab_created_result(ws_idx, tab_idx) {
                    Some(result) => success(id, result),
                    None => failure(id, ApiErrorCode::TabCreateFailed, "new tab is unavailable"),
                }
            }
            Err(err) => failure(id, ApiErrorCode::TabCreateFailed, err.to_string()),
        }
    }

    pub(super) fn handle_tab_focus(&mut self, id: String, target: &TabTarget) -> ApiResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };

        success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_rename(&mut self, id: String, params: TabRenameParams) -> ApiResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let Some(workspace_id) = self.state.workspaces.get(ws_idx).map(|ws| ws.id.clone()) else {
            return tab_not_found(id, &params.tab_id);
        };
        let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_else(|| {
            crate::workspace::public_tab_id_for_number(&workspace_id, tab_idx + 1)
        });
        let Some(tab) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        else {
            return tab_not_found(id, &params.tab_id);
        };
        tab.set_custom_name(params.label.clone());
        shepr_platform::logging::tab_renamed(&workspace_id, &tab_id);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            data: EventData::TabRenamed {
                tab_id: tab_id.clone(),
                workspace_id: self.public_workspace_id(ws_idx),
                label: params.label,
            },
        });
        let Some(tab) = self.tab_info(ws_idx, tab_idx) else {
            return tab_not_found(id, &params.tab_id);
        };

        success(id, ResponseResult::TabInfo { tab })
    }

    pub(super) fn handle_tab_move(&mut self, id: String, params: &TabMoveParams) -> ApiResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&params.tab_id) else {
            return tab_not_found(id, &params.tab_id);
        };
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return tab_not_found(id, &params.tab_id);
        };
        if params.insert_index > ws.tabs.len() {
            return failure(
                id,
                ApiErrorCode::TabMoveFailed,
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let tab_id = self
            .public_tab_id(ws_idx, tab_idx)
            .unwrap_or_else(|| crate::workspace::public_tab_id_for_number(&ws.id, tab_idx + 1));
        let workspace_id = self.public_workspace_id(ws_idx);
        let insert_index = params.insert_index;
        let moved = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.move_tab(tab_idx, insert_index));
        let tabs = self.tab_list_info(ws_idx);
        if moved {
            self.state.refresh_active_tab_id();
            self.schedule_session_save();
            self.emit_event(EventEnvelope {
                data: EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index,
                    tabs: tabs.clone(),
                },
            });
        }

        success(id, ResponseResult::TabList { tabs })
    }

    pub(super) fn handle_tab_close(&mut self, id: String, target: &TabTarget) -> ApiResult {
        let Some((ws_idx, tab_idx)) = self.parse_tab_id(&target.tab_id) else {
            return tab_not_found(id, &target.tab_id);
        };
        if self.public_tab_id(ws_idx, tab_idx).is_none() {
            return tab_not_found(id, &target.tab_id);
        }
        let Some(plan) = self.state.prepare_tab_removal(ws_idx, tab_idx) else {
            return tab_not_found(id, &target.tab_id);
        };
        let close_events = match plan.scope {
            crate::app::actions::TabRemovalScope::Tab => self.tab_close_events(ws_idx, tab_idx),
            crate::app::actions::TabRemovalScope::Workspace => self.workspace_close_events(ws_idx),
        };
        if !matches!(
            self.state.commit_tab_removal(&plan),
            crate::app::actions::TabRemovalCommit::Removed(_)
        ) {
            return failure(
                id,
                ApiErrorCode::TabCloseFailed,
                format!("tab {} could not be closed", target.tab_id),
            );
        }
        self.shutdown_detached_terminal_runtimes();
        self.schedule_session_save();
        self.emit_events(close_events);

        success(id, ResponseResult::Ok {})
    }

    fn tab_list_info(&self, ws_idx: usize) -> Vec<shepr_api::schema::TabInfo> {
        self.state
            .workspaces
            .get(ws_idx)
            .map(|ws| {
                (0..ws.tabs.len())
                    .filter_map(|idx| self.tab_info(ws_idx, idx))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> ApiResult {
    failure(
        id,
        ApiErrorCode::WorkspaceNotFound,
        format!("workspace {workspace_id} not found"),
    )
}

fn tab_not_found(id: String, tab_id: &str) -> ApiResult {
    failure(
        id,
        ApiErrorCode::TabNotFound,
        format!("tab {tab_id} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::workspace::Workspace;
    use shepr_api::schema::SuccessResponse;
    use shepr_config::Config;

    #[test]
    fn api_tab_close_last_tab_closes_workspace_and_emits_both_events() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("tabs")];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let tab_id = app.public_tab_id(0, 0).expect("test precondition");
        let workspace_id = app.public_workspace_id(0);
        let root_pane = app.state.workspaces[0].tabs[0].root_pane;
        let pane_id = app.public_pane_id(0, root_pane).expect("test precondition");

        let response = app.handle_tab_close(
            "req".into(),
            &TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let success: SuccessResponse = shepr_api::error::test_success(&response);
        assert_eq!(success.result, ResponseResult::Ok {});
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.active_index().is_none());
        let events = event_hub.events_after(0);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.data.kind())
                .collect::<Vec<_>>(),
            [
                EventKind::PaneClosed,
                EventKind::TabClosed,
                EventKind::WorkspaceClosed
            ]
        );
        assert!(matches!(
            &events[0].1.data,
            EventData::PaneClosed {
                pane_id: closed_pane_id,
                workspace_id: closed_workspace_id,
            } if closed_pane_id == &pane_id && closed_workspace_id == &workspace_id
        ));
        assert!(matches!(
            &events[1].1.data,
            EventData::TabClosed {
                tab_id: closed_tab_id,
                workspace_id: closed_workspace_id,
            } if closed_tab_id == &tab_id && closed_workspace_id == &workspace_id
        ));
        assert!(matches!(
            &events[2].1.data,
            EventData::WorkspaceClosed {
                workspace_id: closed_workspace_id,
                workspace: Some(workspace),
            } if closed_workspace_id == &workspace_id
                && workspace.workspace_id == workspace_id
        ));
    }

    #[test]
    fn api_tab_close_announces_every_pane_it_removes() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let mut workspace = Workspace::test_new("tabs");
        let split = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.test_add_tab(Some("survivor"));
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let root = app.state.workspaces[0].tabs[0].root_pane;
        app.state
            .public_pane_id_aliases
            .insert("wOLD:p1".into(), root);
        app.state
            .public_pane_id_aliases
            .insert("wOLD:p2".into(), split);
        let closed_panes = [
            app.public_pane_id(0, root).expect("test precondition"),
            app.public_pane_id(0, split).expect("test precondition"),
        ];
        let tab_id = app.public_tab_id(0, 0).expect("test precondition");

        let response = app.handle_tab_close(
            "req".into(),
            &TabTarget {
                tab_id: tab_id.clone(),
            },
        );

        let success: SuccessResponse = shepr_api::error::test_success(&response);
        assert_eq!(success.result, ResponseResult::Ok {});
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert!(
            !app.state
                .public_pane_id_aliases
                .contains_key(&"wOLD:p1".into())
        );
        assert!(
            !app.state
                .public_pane_id_aliases
                .contains_key(&"wOLD:p2".into())
        );
        let events = event_hub.events_after(0);
        let mut pane_closed = events
            .iter()
            .filter_map(|(_, event)| match &event.data {
                EventData::PaneClosed { pane_id, .. } => Some(pane_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        pane_closed.sort();
        let mut expected = closed_panes.to_vec();
        expected.sort();
        assert_eq!(pane_closed, expected);
        assert!(matches!(
            &events.last().expect("tab close event").1.data,
            EventData::TabClosed { tab_id: closed, .. } if closed == &tab_id
        ));
    }

    #[test]
    fn api_tab_move_reorders_tabs_in_target_workspace() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let mut workspace = Workspace::test_new("tabs");
        workspace.test_add_tab(Some("two"));
        workspace.test_add_tab(Some("three"));
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let moved_root = app.state.workspaces[0].tabs[0].root_pane;
        let moved_id = app.public_tab_id(0, 0).expect("test precondition");

        let response = app.handle_tab_move(
            "req".into(),
            &TabMoveParams {
                tab_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = shepr_api::error::test_success(&response);
        let ResponseResult::TabList { tabs } = success.result else {
            panic!("expected tab list");
        };
        assert_eq!(app.state.workspaces[0].tabs[2].root_pane, moved_root);
        assert_eq!(
            tabs[2].tab_id,
            app.public_tab_id(0, 2).expect("test precondition")
        );
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::TabMoved {
                    tab_id,
                    workspace_id,
                    insert_index: 3,
                    tabs,
                } if tab_id == &moved_id
                    && workspace_id == &app.public_workspace_id(0)
                    && tabs[2].tab_id == moved_id
            )
        }));
    }

    #[tokio::test]
    async fn tab_create_follows_cached_focused_pane_cwd_without_runtime() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub,
        );
        app.state.settings.default_shell = exiting_test_command().into();
        app.state.settings.login_shell = false;
        let workspace = Workspace::test_new("tabs");
        let focused_pane = workspace.tabs[0].root_pane;
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
            .cwd = cached_cwd.clone();

        let response = app.handle_tab_create(
            "req".into(),
            TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = shepr_api::error::test_success(&response);
        assert!(matches!(success.result, ResponseResult::TabCreated { .. }));
        let created = &app.state.workspaces[0].tabs[1];
        let created_terminal_id = created
            .terminal_id(created.root_pane)
            .expect("test precondition");
        let created_cwd = &app
            .state
            .terminals
            .get(created_terminal_id)
            .expect("test precondition")
            .cwd;
        assert_eq!(
            std::fs::canonicalize(created_cwd).unwrap_or_else(|_| created_cwd.clone()),
            std::fs::canonicalize(&cached_cwd).unwrap_or_else(|_| cached_cwd.clone())
        );
        shutdown_test_runtimes(&mut app);
    }
}
