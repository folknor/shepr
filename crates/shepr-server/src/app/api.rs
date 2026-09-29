mod checkout_root;
mod cwd;
mod detect;
mod env;
mod layouts;
mod panes;
pub(super) mod responses;
pub(super) mod session;
mod tabs;
mod workspaces;

use super::{App, Outcome, RenderDemand};
use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};
use shepr_protocol::command::{EndpointCommand, EndpointReply};

/// A client-shell command's answer, before the server loop puts it on the
/// client socket.
pub(crate) type EndpointResult = Result<EndpointReply, ApiError>;

/// What one client-shell command did: its answer and the render it needs.
pub(crate) struct EndpointOutcome {
    pub(crate) result: EndpointResult,
    pub(crate) render: RenderDemand,
}

impl App {
    pub(crate) fn handle_api_request_with_render(
        &mut self,
        request: shepr_api::schema::AppRequest,
    ) -> Outcome {
        let mutates_ui = request.method.traits().mutates_ui;
        let render = if mutates_ui {
            self.state.mark_shell_projection_dirty();
            RenderDemand::Full
        } else {
            RenderDemand::None
        };
        let response = self.handle_api_request_after_internal_events_drained(request);
        Outcome { response, render }
    }

    pub(crate) fn handle_api_request_after_internal_events_drained(
        &mut self,
        request: shepr_api::schema::AppRequest,
    ) -> ApiResult {
        self.sync_pending_terminal_titles();
        use shepr_api::schema::AppMethod;

        match request.method {
            AppMethod::DetectCapture(target) => self.handle_detect_capture(&target),
            AppMethod::DetectExplain(target) => self.handle_detect_explain(&target),
            AppMethod::PaneReportAgent(params) => self.handle_pane_report_agent(params),
            AppMethod::PaneReportAgentSession(params) => {
                self.handle_pane_report_agent_session(params)
            }
        }
    }

    pub(crate) fn handle_endpoint_command_with_render(
        &mut self,
        command: EndpointCommand,
    ) -> EndpointOutcome {
        let mutates_ui = command.traits().mutates_ui;
        // These commands change scroll position, split ratios or PTY input
        // only; the shell snapshot carries none of those. Anything the agent
        // does in response arrives later through its own hook report.
        let changes_shell_projection = mutates_ui
            && !matches!(
                &command,
                EndpointCommand::PaneScroll(_)
                    | EndpointCommand::PaneClear(_)
                    | EndpointCommand::PaneResize(_)
                    | EndpointCommand::LayoutSetSplitRatio(_)
            );
        let render = if mutates_ui {
            if changes_shell_projection {
                self.state.mark_shell_projection_dirty();
            }
            RenderDemand::Full
        } else {
            RenderDemand::None
        };
        let result = self.handle_endpoint_command_after_internal_events_drained(command);
        EndpointOutcome { result, render }
    }

    pub(crate) fn handle_endpoint_command_after_internal_events_drained(
        &mut self,
        command: EndpointCommand,
    ) -> EndpointResult {
        self.sync_pending_terminal_titles();

        match command {
            // The server loop answers the surface lease itself, before any
            // command reaches the app; reaching here is a routing bug.
            EndpointCommand::ClientShellSurfaceSet(_) => {
                tracing::warn!("client_shell.surface.set routed to the app by mistake");
                responses::failure(
                    ApiErrorCode::InternalError,
                    "client_shell.surface.set is not handled by the app",
                )
            }
            EndpointCommand::WorkspaceCreate(params) => self.handle_workspace_create(params),
            EndpointCommand::WorkspaceFocus(target) => self.handle_workspace_focus(&target),
            EndpointCommand::WorkspaceRename(params) => self.handle_workspace_rename(params),
            EndpointCommand::WorkspaceCheckoutRoot(params) => {
                self.handle_workspace_checkout_root(&params)
            }
            EndpointCommand::WorkspaceMove(params) => self.handle_workspace_move(&params),
            EndpointCommand::WorkspaceClose(target) => self.handle_workspace_close(&target),
            EndpointCommand::TabCreate(params) => self.handle_tab_create(params),
            EndpointCommand::TabFocus(target) => self.handle_tab_focus(&target),
            EndpointCommand::TabRename(params) => self.handle_tab_rename(params),
            EndpointCommand::TabMove(params) => self.handle_tab_move(&params),
            EndpointCommand::TabClose(target) => self.handle_tab_close(&target),
            EndpointCommand::PaneSplit(params) => self.handle_pane_split(params),
            EndpointCommand::PaneSwap(params) => self.handle_pane_swap(&params),
            EndpointCommand::PaneZoom(params) => self.handle_pane_zoom(&params),
            EndpointCommand::LayoutSetSplitRatio(params) => {
                self.handle_layout_set_split_ratio(&params)
            }
            EndpointCommand::PaneFocusDirection(params) => {
                self.handle_pane_focus_direction(&params)
            }
            EndpointCommand::PaneResize(params) => self.handle_pane_resize(&params),
            EndpointCommand::PaneScroll(params) => self.handle_pane_scroll(&params),
            EndpointCommand::PaneClear(target) => self.handle_pane_clear(&target),
            EndpointCommand::PaneSelectionRead(params) => self.handle_pane_selection_read(params),
            EndpointCommand::PaneCopyMotion(params) => self.handle_pane_copy_motion(params),
            EndpointCommand::PaneCopySearch(params) => self.handle_pane_copy_search(params),
            EndpointCommand::PaneFocus(target) => self.handle_pane_focus(&target),
            EndpointCommand::PaneInputSet(params) => self.handle_pane_input_set(&params),
            EndpointCommand::PaneRename(params) => self.handle_pane_rename(params),
            EndpointCommand::PaneClose(target) => self.handle_pane_close(&target),
        }
    }
}

#[cfg(test)]
use shepr_mux::events::AppEvent;

#[cfg(test)]
impl App {
    pub(crate) fn handle_api_request(&mut self, request: shepr_api::schema::AppRequest) -> String {
        let id = request.id.clone();
        self.drain_all_internal_events();
        shepr_api::error::encode_result(
            id,
            self.handle_api_request_after_internal_events_drained(request),
        )
    }

    /// Runs one client-shell command the way the server loop does, after
    /// draining pending internal events.
    pub(crate) fn handle_endpoint_command(&mut self, command: EndpointCommand) -> EndpointResult {
        self.drain_all_internal_events();
        self.handle_endpoint_command_after_internal_events_drained(command)
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
    fn the_surface_lease_answered_by_the_loop_is_reported_as_misrouted() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );

        let surface = app.handle_endpoint_command(EndpointCommand::ClientShellSurfaceSet(
            shepr_protocol::command::ClientShellSurfaceSetParams { active: true },
        ));
        assert_eq!(
            surface
                .expect_err("surface lease is answered by the loop")
                .code,
            ApiErrorCode::InternalError
        );
        assert!(!app.state.should_quit);
    }

    #[test]
    fn read_only_commands_do_not_force_a_render() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );
        let read = app.handle_endpoint_command_with_render(EndpointCommand::PaneSelectionRead(
            shepr_protocol::command::PaneSelectionReadParams {
                pane_id: "w1:p1".into(),
                anchor: shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 0,
                },
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 0,
                },
            },
        ));
        assert_eq!(read.render, RenderDemand::None);

        let rename = app.handle_endpoint_command_with_render(EndpointCommand::PaneRename(
            shepr_protocol::command::PaneRenameParams {
                pane_id: "w1:p1".into(),
                label: Some("logs".into()),
            },
        ));
        assert_eq!(rename.render, RenderDemand::Full);
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
