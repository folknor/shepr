mod agents;
mod env;
mod layouts;
mod panes;
pub(super) mod responses;
mod session;
mod tabs;
mod workspaces;

use super::App;
#[cfg(test)]
use crate::events::AppEvent;

impl App {
    #[cfg(test)]
    pub(crate) fn handle_api_request(&mut self, request: crate::api::schema::Request) -> String {
        self.drain_all_internal_events();
        self.handle_api_request_after_internal_events_drained(request)
    }

    pub(crate) fn handle_api_request_after_internal_events_drained(
        &mut self,
        request: crate::api::schema::Request,
    ) -> String {
        self.sync_pending_terminal_titles();
        use crate::api::schema::{Method, ResponseResult, SuccessResponse};

        let method_name = crate::api::api_method_name(&request.method);
        let response = match request.method {
            // Every one of these is answered before a request reaches the app:
            // the API server handles ping, SSH agent leases, subscriptions and
            // waits (including `agent.wait`) on the connection thread and
            // rejects `client_shell.surface.set`; the headless server
            // intercepts window titles and `agent.prompt` before calling this
            // function. Reaching here is a routing bug, reported as such.
            Method::Ping(_)
            | Method::ServerStop(_)
            | Method::ServerSshAgentRegister(_)
            | Method::ClientWindowTitleSet(_)
            | Method::ClientWindowTitleClear(_)
            | Method::ClientShellSurfaceSet(_)
            | Method::AgentPrompt(_)
            | Method::AgentWait(_)
            | Method::EventsSubscribe(_)
            | Method::EventsWait(_)
            | Method::PaneWaitForOutput(_) => {
                tracing::warn!(
                    method = method_name,
                    "api request routed to the app by mistake"
                );
                return responses::encode_error(
                    request.id,
                    "internal_error",
                    format!("{method_name} is not handled by the app"),
                );
            }
            Method::ServerAgentManifests(_) => {
                self.state.refresh_agent_manifest_summaries();
                SuccessResponse {
                    id: request.id,
                    result: ResponseResult::AgentManifestStatus {
                        manifests: self
                            .state
                            .agent_manifest_summaries
                            .clone()
                            .into_iter()
                            .map(agent_manifest_info)
                            .collect(),
                    },
                }
            }
            Method::ServerReloadAgentManifests(_) => {
                let summaries = crate::detect::manifest::reload_manifests(self.paths.config_dir());
                self.state.agent_manifest_summaries = summaries.clone();
                self.reset_all_agent_detection_runtimes();
                SuccessResponse {
                    id: request.id,
                    result: ResponseResult::AgentManifestReload {
                        manifests: summaries.into_iter().map(agent_manifest_info).collect(),
                    },
                }
            }
            Method::SessionSnapshot(_) => return self.handle_session_snapshot(request.id),
            Method::WorkspaceList(_) => return self.handle_workspace_list(request.id),
            Method::WorkspaceGet(target) => return self.handle_workspace_get(request.id, &target),
            Method::WorkspaceCreate(params) => {
                return self.handle_workspace_create(request.id, params);
            }
            Method::WorkspaceFocus(target) => {
                return self.handle_workspace_focus(request.id, &target);
            }
            Method::WorkspaceRename(params) => {
                return self.handle_workspace_rename(request.id, params);
            }
            Method::WorkspaceMove(params) => {
                return self.handle_workspace_move(request.id, &params);
            }
            Method::WorkspaceMoveBlock(params) => {
                return self.handle_workspace_move_block(request.id, params);
            }
            Method::WorkspaceReportMetadata(params) => {
                return self.handle_workspace_report_metadata(request.id, params);
            }
            Method::WorkspaceClose(target) => {
                return self.handle_workspace_close(request.id, &target);
            }
            Method::TabList(params) => return self.handle_tab_list(request.id, params),
            Method::TabGet(target) => return self.handle_tab_get(request.id, &target),
            Method::TabCreate(params) => return self.handle_tab_create(request.id, params),
            Method::TabFocus(target) => return self.handle_tab_focus(request.id, &target),
            Method::TabRename(params) => return self.handle_tab_rename(request.id, params),
            Method::TabMove(params) => return self.handle_tab_move(request.id, &params),
            Method::TabClose(target) => return self.handle_tab_close(request.id, &target),
            Method::AgentList(_) => return self.handle_agent_list(request.id),
            Method::AgentGet(target) => return self.handle_agent_get(request.id, &target),
            Method::AgentFocus(target) => return self.handle_agent_focus(request.id, &target),
            Method::AgentRename(params) => return self.handle_agent_rename(request.id, params),
            Method::AgentStart(params) => return self.handle_agent_start(request.id, params),
            Method::AgentRead(params) => return self.handle_agent_read(request.id, &params),
            Method::AgentExplain(target) => return self.handle_agent_explain(request.id, &target),
            Method::AgentSendKeys(params) => {
                return self.handle_agent_send_keys(request.id, &params);
            }
            Method::PaneSplit(params) => return self.handle_pane_split(request.id, params),
            Method::PaneSwap(params) => return self.handle_pane_swap(request.id, params),
            Method::PaneMove(params) => return self.handle_pane_move(request.id, params),
            Method::PaneZoom(params) => return self.handle_pane_zoom(request.id, &params),
            Method::PaneLayout(params) => return self.handle_pane_layout(request.id, &params),
            Method::PaneProcessInfo(params) => {
                return self.handle_pane_process_info(request.id, &params);
            }
            Method::LayoutExport(params) => {
                return self.handle_layout_export(request.id, &params);
            }
            Method::LayoutApply(params) => return self.handle_layout_apply(request.id, &params),
            Method::LayoutSetSplitRatio(params) => {
                return self.handle_layout_set_split_ratio(request.id, params);
            }
            Method::PaneNeighbor(params) => return self.handle_pane_neighbor(request.id, &params),
            Method::PaneEdges(params) => return self.handle_pane_edges(request.id, &params),
            Method::PaneFocusDirection(params) => {
                return self.handle_pane_focus_direction(request.id, &params);
            }
            Method::PaneResize(params) => return self.handle_pane_resize(request.id, &params),
            Method::PaneScroll(params) => return self.handle_pane_scroll(request.id, &params),
            Method::PaneClear(target) => return self.handle_pane_clear(request.id, &target),
            Method::PaneSelectionRead(params) => {
                return self.handle_pane_selection_read(request.id, params);
            }
            Method::PaneCopyMotion(params) => {
                return self.handle_pane_copy_motion(request.id, params);
            }
            Method::PaneCopySearch(params) => {
                return self.handle_pane_copy_search(request.id, params);
            }
            Method::PaneList(params) => return self.handle_pane_list(request.id, &params),
            Method::PaneCurrent(params) => return self.handle_pane_current(request.id, &params),
            Method::PaneGet(target) => return self.handle_pane_get(request.id, &target),
            Method::PaneFocus(target) => return self.handle_pane_focus(request.id, &target),
            Method::PaneInputSet(params) => return self.handle_pane_input_set(request.id, &params),
            Method::PaneRename(params) => return self.handle_pane_rename(request.id, params),
            Method::PaneRead(params) => return self.handle_pane_read(request.id, &params),
            Method::PaneReportAgent(params) => {
                return self.handle_pane_report_agent(request.id, params);
            }
            Method::PaneReportAgentSession(params) => {
                return self.handle_pane_report_agent_session(request.id, params);
            }
            Method::PaneReportMetadata(params) => {
                return self.handle_pane_report_metadata(request.id, params);
            }
            Method::PaneClearAgentAuthority(params) => {
                return self.handle_pane_clear_agent_authority(request.id, params);
            }
            Method::PaneReleaseAgent(params) => {
                return self.handle_pane_release_agent(request.id, params);
            }
            Method::PaneSendText(params) => return self.handle_pane_send_text(request.id, params),
            Method::PaneSendInput(params) => {
                return self.handle_pane_send_input(request.id, &params);
            }
            Method::PaneClose(target) => return self.handle_pane_close(request.id, &target),
            Method::PaneSendKeys(params) => return self.handle_pane_send_keys(request.id, &params),
        };

        responses::encode_success(response.id, response.result)
    }
}

fn agent_manifest_info(
    summary: crate::detect::manifest::AgentManifestSummary,
) -> crate::api::schema::AgentManifestInfo {
    crate::api::schema::AgentManifestInfo {
        agent: crate::detect::agent_label(summary.agent).to_string(),
        source: summary.active_source.label(),
        source_kind: summary.active_source.kind().to_string(),
        warning: summary.warning,
    }
}

#[cfg(test)]
pub(super) mod test_support {
    pub(crate) fn exiting_test_command() -> &'static str {
        "/usr/bin/true"
    }

    pub(crate) fn shutdown_test_runtimes(app: &mut crate::app::App) {
        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            runtime.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Agent, AgentState};

    #[tokio::test]
    async fn server_reload_agent_manifests_resets_detection_runtimes() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("manifest-reload")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let reset_notify = runtime.agent_detection_reset_notify_for_test();
        app.terminal_runtimes.insert(terminal_id, runtime);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "reload_manifests".into(),
            method: crate::api::schema::Method::ServerReloadAgentManifests(
                crate::api::schema::EmptyParams::default(),
            ),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response["result"]["type"], "agent_manifest_reload");
        assert!(
            !response["result"]["manifests"]
                .as_array()
                .expect("test precondition")
                .is_empty()
        );

        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reset_notify.notified(),
        )
        .await
        .expect("manual manifest reload should reset detection runtimes");
    }

    #[tokio::test]
    async fn server_agent_manifests_reports_status_without_resetting_runtimes() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("manifest-status")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let reset_notify = runtime.agent_detection_reset_notify_for_test();
        app.terminal_runtimes.insert(terminal_id, runtime);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "manifest_status".into(),
            method: crate::api::schema::Method::ServerAgentManifests(
                crate::api::schema::EmptyParams::default(),
            ),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response["result"]["type"], "agent_manifest_status");
        assert!(
            !response["result"]["manifests"]
                .as_array()
                .expect("test precondition")
                .is_empty()
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                reset_notify.notified(),
            )
            .await
            .is_err(),
            "status request should not reset detection runtimes"
        );
    }

    #[tokio::test]
    async fn agent_explain_evaluates_with_server_manifest_cache() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("agent-explain")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .detected_agent = Some(Agent::Codex);
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(
            80,
            24,
            b"press enter to confirm or esc to cancel",
        );
        app.terminal_runtimes.insert(terminal_id, runtime);
        let target = app.public_pane_id(0, pane_id).expect("test precondition");

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "agent_explain".into(),
            method: crate::api::schema::Method::AgentExplain(crate::api::schema::AgentTarget {
                target,
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "agent_explain");
        assert_eq!(response["result"]["explain"]["state"], "blocked");
        assert_eq!(
            response["result"]["explain"]["matched_rule"]["id"],
            "live_strong_blocker"
        );
    }

    #[tokio::test]
    async fn agent_explain_rejects_hook_only_full_lifecycle_authority() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("agent-explain-omp")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_hook_authority(
                "shepr:omp".to_string(),
                "omp".to_string(),
                AgentState::Working,
                None,
                Some(1),
            );
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        app.terminal_runtimes.insert(terminal_id, runtime);
        let target = app.public_pane_id(0, pane_id).expect("test precondition");

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "agent_explain_omp".into(),
            method: crate::api::schema::Method::AgentExplain(crate::api::schema::AgentTarget {
                target,
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["error"]["code"], "agent_not_found");
    }

    #[tokio::test]
    async fn pane_process_info_returns_response_for_existing_pane() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("process-info")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id, runtime);
        let target = app.public_pane_id(0, pane_id).expect("test precondition");

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "process_info".into(),
            method: crate::api::schema::Method::PaneProcessInfo(
                crate::api::schema::PaneProcessInfoParams {
                    pane_id: Some(target.clone()),
                },
            ),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "pane_process_info");
        assert_eq!(response["result"]["process_info"]["pane_id"], target);
    }

    #[test]
    fn methods_answered_before_the_app_are_reported_as_misrouted() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );

        for method in [
            crate::api::schema::Method::ClientWindowTitleClear(
                crate::api::schema::EmptyParams::default(),
            ),
            crate::api::schema::Method::AgentWait(crate::api::schema::AgentWaitParams {
                target: "reviewer".into(),
                until: Vec::new(),
                timeout_ms: None,
            }),
            crate::api::schema::Method::Ping(crate::api::schema::PingParams::default()),
        ] {
            let name = crate::api::api_method_name(&method);
            let response = app.handle_api_request(crate::api::schema::Request {
                id: "misrouted".into(),
                method,
            });
            let response: serde_json::Value =
                serde_json::from_str(&response).expect("test precondition");
            assert_eq!(response["id"], "misrouted", "{name}");
            assert_eq!(response["error"]["code"], "internal_error", "{name}");
        }
        assert!(!app.state.should_quit);
    }

    #[test]
    fn pane_exit_emits_layout_updated_when_tab_survives() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let mut workspace = crate::workspace::Workspace::test_new("pane-exit-layout");
        let dead_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let tab_id = app.public_tab_id(0, 0).expect("test precondition");

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });

        let events = event_hub.events_after(0);
        let pane_exited = events
            .iter()
            .position(|(_, event)| event.data.kind() == crate::api::schema::EventKind::PaneExited)
            .expect("pane.exited should be emitted");
        let layout_updated = events
            .iter()
            .position(|(_, event)| {
                event.data.kind() == crate::api::schema::EventKind::LayoutUpdated
            })
            .expect("layout.updated should be emitted");
        assert!(pane_exited < layout_updated);
        assert!(matches!(
            &events[layout_updated].1.data,
            crate::api::schema::EventData::LayoutUpdated { layout }
                if layout.tab_id == tab_id && layout.panes.len() == 1
        ));
    }

    #[test]
    fn pane_state_update_resolves_workspace_after_an_earlier_workspace_closes() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let first = crate::workspace::Workspace::test_new("closing");
        let target = crate::workspace::Workspace::test_new("target");
        let pane_id = target.tabs[0].root_pane;
        let workspace_id = target.id.clone();
        app.state.workspaces = vec![first, target];
        app.state.ensure_test_terminals();
        let presentation = crate::terminal::EffectivePresentation {
            title: None,
            display_agent: None,
            state_labels: std::collections::HashMap::new(),
        };
        let update = crate::app::actions::PaneStateUpdate {
            pane_id,
            workspace_id: workspace_id.clone(),
            previous_agent_label: None,
            previous_known_agent: None,
            previous_state: AgentState::Unknown,
            previous_presentation: presentation.clone(),
            agent_label: Some("codex".into()),
            known_agent: Some(Agent::Codex),
            state: AgentState::Working,
            presentation,
            agent_name_changed: false,
            agent_released: false,
        };

        app.state.close_workspace_at(0);
        app.emit_pane_state_update(&update);

        let pane_id = app.public_pane_id(0, pane_id).expect("live pane id");
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            crate::api::schema::EventData::PaneAgentDetected {
                pane_id: emitted_pane_id,
                workspace_id: emitted_workspace_id,
                ..
            } if emitted_pane_id == &pane_id && emitted_workspace_id == &workspace_id
        )));
    }

    #[test]
    fn pane_exit_announces_the_tab_and_workspace_it_empties() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let mut workspace = crate::workspace::Workspace::test_new("pane-exit-tab");
        workspace.test_add_tab(Some("second"));
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let first_root = app.state.workspaces[0].tabs[0].root_pane;
        let second_root = app.state.workspaces[0].tabs[1].root_pane;
        let first_tab = app.public_tab_id(0, 0).expect("test precondition");
        let second_tab = app.public_tab_id(0, 1).expect("test precondition");
        let workspace_id = app.public_workspace_id(0);

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: first_root,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });
        // Only the removal events are this test's subject.
        let removals = |hub: &crate::api::EventHub, after: u64| {
            hub.events_after(after)
                .into_iter()
                .map(|(_, event)| event)
                .filter(|event| {
                    matches!(
                        event.data.kind(),
                        crate::api::schema::EventKind::PaneExited
                            | crate::api::schema::EventKind::PaneClosed
                            | crate::api::schema::EventKind::TabClosed
                            | crate::api::schema::EventKind::WorkspaceClosed
                    )
                })
                .collect::<Vec<_>>()
        };
        let events = removals(&event_hub, 0);
        assert_eq!(
            events
                .iter()
                .map(|event| event.data.kind())
                .collect::<Vec<_>>(),
            [
                crate::api::schema::EventKind::PaneExited,
                crate::api::schema::EventKind::TabClosed
            ]
        );
        assert!(matches!(
            &events[1].data,
            crate::api::schema::EventData::TabClosed { tab_id, .. } if tab_id == &first_tab
        ));

        let before = event_hub.current_sequence();
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: second_root,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });
        let events = removals(&event_hub, before);
        assert_eq!(
            events
                .iter()
                .map(|event| event.data.kind())
                .collect::<Vec<_>>(),
            [
                crate::api::schema::EventKind::PaneExited,
                crate::api::schema::EventKind::TabClosed,
                crate::api::schema::EventKind::WorkspaceClosed
            ]
        );
        assert!(matches!(
            &events[1].data,
            crate::api::schema::EventData::TabClosed { tab_id, .. } if tab_id == &second_tab
        ));
        assert!(matches!(
            &events[2].data,
            crate::api::schema::EventData::WorkspaceClosed { workspace_id: closed, .. }
                if closed == &workspace_id
        ));
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn idle_agent_exit_emits_release_event_without_a_state_change() {
        for agent_name in [None, Some("reviewer")] {
            let event_hub = crate::api::EventHub::default();
            let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &crate::config::Config::default(),
                crate::app::AppPolicy::TEST,
                api_rx,
                event_hub.clone(),
            );
            let workspace = crate::workspace::Workspace::test_new("idle-agent-exit");
            let pane_id = workspace.tabs[0].root_pane;
            let terminal_id = workspace
                .terminal_id(pane_id)
                .cloned()
                .expect("test precondition");
            app.state.workspaces = vec![workspace];
            app.state.ensure_test_terminals();
            let terminal = app
                .state
                .terminals
                .get_mut(&terminal_id)
                .expect("test precondition");
            terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
            if let Some(agent_name) = agent_name {
                terminal.set_agent_name(agent_name.into());
            }

            app.handle_internal_event(AppEvent::StateChanged {
                pane_id,
                agent: Some(Agent::Pi),
                state: AgentState::Idle,
                visible_blocker: false,
                process_exited: true,
                observed_at: std::time::Instant::now(),
            });

            // The release event is this test's subject; the name outliving the
            // observation is pinned by
            // `a_process_exit_observation_alone_does_not_free_the_name`.
            assert_eq!(
                app.state.terminals[&terminal_id].agent_name.as_deref(),
                agent_name
            );
            assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
                &event.data,
                crate::api::schema::EventData::PaneAgentDetected {
                    released: true,
                    final_status: Some(crate::api::schema::AgentStatus::Idle),
                    ..
                }
            )));
        }
    }

    #[test]
    fn process_exit_releases_a_newer_hook_owned_agent() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let workspace = crate::workspace::Workspace::test_new("stale-agent-exit");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let observed_at = std::time::Instant::now();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
        terminal
            .set_hook_authority_at(
                "shepr:codex".into(),
                "codex".into(),
                AgentState::Working,
                None,
                None,
                Some(1),
                observed_at + std::time::Duration::from_secs(1),
            )
            .expect("test precondition");
        terminal.set_agent_name("reviewer".into());

        app.handle_internal_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Codex),
            state: AgentState::Idle,
            visible_blocker: false,
            process_exited: true,
            observed_at,
        });

        let terminal = &app.state.terminals[&terminal_id];
        assert_eq!(terminal.state, AgentState::Idle);
        // Releasing the registration does not free the name yet; a wrong
        // observation must not cost a live agent the handle its owner gave it.
        assert_eq!(terminal.agent_name.as_deref(), Some("reviewer"));
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            event.data,
            crate::api::schema::EventData::PaneAgentDetected { released: true, .. }
        )));
    }
}
