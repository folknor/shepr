mod checkout_root;
mod cwd;
mod detect;
mod env;
mod layouts;
mod panes;
pub(super) mod responses;
mod session;
mod tabs;
mod workspaces;

use super::{App, Outcome, RenderDemand};
use shepr_api::error::{ApiErrorCode, ApiResult};

impl App {
    pub(crate) fn handle_api_request_with_render(
        &mut self,
        request: shepr_api::schema::Request,
    ) -> Outcome {
        let mutates_ui = request.method.traits().mutates_ui;
        // These methods change scroll position, split ratios or PTY input
        // only; the shell snapshot carries none of those. Anything the agent
        // does in response arrives later through its own hook report.
        let changes_shell_projection = mutates_ui
            && !matches!(
                &request.method,
                shepr_api::schema::Method::PaneScroll(_)
                    | shepr_api::schema::Method::PaneClear(_)
                    | shepr_api::schema::Method::PaneResize(_)
                    | shepr_api::schema::Method::LayoutSetSplitRatio(_)
            );
        let render = if mutates_ui {
            if changes_shell_projection {
                self.state.mark_shell_projection_dirty();
            }
            RenderDemand::Full
        } else {
            RenderDemand::None
        };
        let response = self.handle_api_request_after_internal_events_drained(request);
        Outcome { response, render }
    }

    pub(crate) fn handle_api_request_after_internal_events_drained(
        &mut self,
        request: shepr_api::schema::Request,
    ) -> ApiResult {
        self.sync_pending_terminal_titles();
        use shepr_api::schema::Method;

        let method_name = shepr_api::api_method_name(&request.method);
        match request.method {
            // Every one of these is answered before a request reaches the app:
            // the API server handles ping and SSH agent leases on the
            // connection thread and rejects `client_shell.surface.set`.
            // Reaching here is a routing bug, reported as such.
            Method::Ping(_)
            | Method::ServerStop(_)
            | Method::ServerSshAgentRegister(_)
            | Method::ClientShellSurfaceSet(_) => {
                tracing::warn!(
                    method = method_name,
                    "api request routed to the app by mistake"
                );
                responses::failure(
                    ApiErrorCode::InternalError,
                    format!("{method_name} is not handled by the app"),
                )
            }
            Method::SessionSnapshot(_) => self.handle_session_snapshot(),
            Method::WorkspaceCreate(params) => self.handle_workspace_create(params),
            Method::WorkspaceFocus(target) => self.handle_workspace_focus(&target),
            Method::WorkspaceRename(params) => self.handle_workspace_rename(params),
            Method::WorkspaceCheckoutRoot(params) => self.handle_workspace_checkout_root(&params),
            Method::WorkspaceMove(params) => self.handle_workspace_move(&params),
            Method::WorkspaceClose(target) => self.handle_workspace_close(&target),
            Method::TabCreate(params) => self.handle_tab_create(params),
            Method::TabFocus(target) => self.handle_tab_focus(&target),
            Method::TabRename(params) => self.handle_tab_rename(params),
            Method::TabMove(params) => self.handle_tab_move(&params),
            Method::TabClose(target) => self.handle_tab_close(&target),
            Method::DetectCapture(target) => self.handle_detect_capture(&target),
            Method::DetectExplain(target) => self.handle_detect_explain(&target),
            Method::PaneSplit(params) => self.handle_pane_split(params),
            Method::PaneSwap(params) => self.handle_pane_swap(params),
            Method::PaneZoom(params) => self.handle_pane_zoom(&params),
            Method::LayoutSetSplitRatio(params) => self.handle_layout_set_split_ratio(&params),
            Method::PaneFocusDirection(params) => self.handle_pane_focus_direction(&params),
            Method::PaneResize(params) => self.handle_pane_resize(&params),
            Method::PaneScroll(params) => self.handle_pane_scroll(&params),
            Method::PaneClear(target) => self.handle_pane_clear(&target),
            Method::PaneSelectionRead(params) => self.handle_pane_selection_read(params),
            Method::PaneCopyMotion(params) => self.handle_pane_copy_motion(params),
            Method::PaneCopySearch(params) => self.handle_pane_copy_search(params),
            Method::PaneFocus(target) => self.handle_pane_focus(&target),
            Method::PaneInputSet(params) => self.handle_pane_input_set(&params),
            Method::PaneRename(params) => self.handle_pane_rename(params),
            Method::PaneReportAgent(params) => self.handle_pane_report_agent(params),
            Method::PaneReportAgentSession(params) => self.handle_pane_report_agent_session(params),
            Method::PaneClose(target) => self.handle_pane_close(&target),
        }
    }
}

#[cfg(test)]
use shepr_mux::events::AppEvent;

#[cfg(test)]
impl App {
    pub(crate) fn handle_api_request(&mut self, request: shepr_api::schema::Request) -> String {
        let id = request.id.clone();
        self.drain_all_internal_events();
        shepr_api::error::encode_result(
            id,
            self.handle_api_request_after_internal_events_drained(request),
        )
    }
}

#[cfg(test)]
pub(super) mod test_support {
    /// A program that exits 0 at once: the fixture program with no script.
    pub(crate) fn exiting_test_command() -> &'static str {
        shepr_test_support::fixture::path_str()
    }

    pub(crate) fn shutdown_test_runtimes(app: &mut crate::app::App) {
        use crate::test_support::PaneRuntimeRegistryFixture as _;
        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            drop(runtime);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_agent::detect::{Agent, AgentState};

    #[test]
    fn methods_answered_before_the_app_are_reported_as_misrouted() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );

        for method in [
            shepr_api::schema::Method::ServerStop(shepr_api::schema::ServerStopParams::default()),
            shepr_api::schema::Method::Ping(shepr_api::schema::PingParams::default()),
        ] {
            let name = shepr_api::api_method_name(&method);
            let response = app.handle_api_request(shepr_api::schema::Request {
                id: "misrouted".into(),
                method,
            });
            let response: serde_json::Value =
                serde_json::from_str(&crate::test_support::test_json(&response))
                    .expect("test precondition");
            assert_eq!(response["id"], "misrouted", "{name}");
            assert_eq!(response["error"]["code"], "internal_error", "{name}");
        }
        assert!(!app.state.should_quit);
    }

    #[test]
    fn pane_exit_keeps_the_tab_when_other_panes_remain() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );
        let mut workspace = shepr_mux::workspace::Workspace::test_new("pane-exit-layout");
        let dead_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: shepr_platform::ChildExitReason::Exited,
        });

        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].tabs().len(), 1);
        assert_eq!(app.state.workspaces[0].tabs()[0].layout().pane_count(), 1);
    }

    #[test]
    fn pane_exit_removes_the_tab_and_workspace_it_empties() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );
        let mut workspace = shepr_mux::workspace::Workspace::test_new("pane-exit-tab");
        workspace.test_add_tab(Some("second"));
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let first_root = app.state.workspaces[0].tabs()[0].root_pane();
        let second_root = app.state.workspaces[0].tabs()[1].root_pane();

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: first_root,
            exit_reason: shepr_platform::ChildExitReason::Exited,
        });
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].tabs().len(), 1);
        assert_eq!(app.state.workspaces[0].tabs()[0].root_pane(), second_root);

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: second_root,
            exit_reason: shepr_platform::ChildExitReason::Exited,
        });
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn process_exit_releases_a_newer_hook_owned_agent() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );
        let workspace = shepr_mux::workspace::Workspace::test_new("stale-agent-exit");
        let pane_id = workspace.tabs()[0].root_pane();
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
    }
}
