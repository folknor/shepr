mod checkout_root;
mod cwd;
mod detect;
mod endpoint;
mod layouts;
mod panes;
pub(super) mod responses;
pub(super) mod session;
mod workspaces;

use super::state::SpawnGeometry;
use super::{App, Outcome, RenderDemand};
use shepr_api::error::ApiResult;
use shepr_protocol::WorkspaceId;
use shepr_protocol::command::{EndpointCommand, EndpointError, EndpointReply};

pub(crate) use endpoint::EndpointEffects;
use endpoint::{HandlerResult, rejected};

/// What the server loop knows about the requesting client that no app state
/// holds.
pub(crate) struct EndpointContext {
    /// The geometry the requesting client presents, for a workspace with no
    /// recorded geometry.
    pub(crate) requester_geometry: Option<SpawnGeometry>,
}

/// What one client-shell command did: its answer, the workspace the requesting
/// client navigates to, and the render it needs.
pub(crate) struct EndpointOutcome {
    pub(crate) result: Result<EndpointReply, EndpointError>,
    /// The workspace the command moves the requesting client to. Only a
    /// command that succeeded navigates; the server loop applies it.
    pub(crate) navigate: Option<WorkspaceId>,
    /// Shared effects committed by the handler, including changes committed
    /// before a later refusal.
    pub(crate) effects: EndpointEffects,
    pub(crate) render: RenderDemand,
}

impl App {
    pub(crate) fn handle_api_request_with_render(
        &mut self,
        request: shepr_api::schema::AppRequest,
    ) -> Outcome {
        let projection_before = self.state.shell_projection_revision;
        let response = self.handle_api_request_after_internal_events_drained(request);
        let render = if self.state.shell_projection_revision != projection_before {
            RenderDemand::Full
        } else {
            RenderDemand::None
        };
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

    /// Runs one client-shell command against the app state the server loop
    /// has already drained internal events into. Targets, the creation source
    /// and recorded geometry are all resolved here, against that state. The
    /// command's navigation effect is returned, not applied: navigation is per
    /// client and the loop owns it, as it owns the reconcile, the geometry
    /// settlement and the reply's focus flags that follow.
    pub(crate) fn handle_endpoint_command_with_render(
        &mut self,
        command: EndpointCommand,
        ctx: &EndpointContext,
    ) -> EndpointOutcome {
        // Some app operations publish their projection revision at the state
        // mutation site. Keep that signal too: a handler can commit before a
        // later reply lookup refuses the command.
        let projection_before = self.state.shell_projection_revision;
        self.sync_pending_terminal_titles();
        let (result, navigate, effects) = match self.dispatch_endpoint_command(command, ctx) {
            Ok(handled) => (Ok(handled.reply), handled.navigate, handled.effects),
            Err(error) => (Err(error.error), None, error.effects),
        };
        let projection_revision_changed = self.state.shell_projection_revision != projection_before;
        if effects.shell_projection_changed && !projection_revision_changed {
            self.state.mark_shell_projection_dirty();
        }
        let render = if effects.needs_render() || projection_revision_changed {
            RenderDemand::Full
        } else {
            RenderDemand::None
        };
        EndpointOutcome {
            result,
            navigate,
            effects,
            render,
        }
    }

    fn dispatch_endpoint_command(
        &mut self,
        command: EndpointCommand,
        ctx: &EndpointContext,
    ) -> HandlerResult {
        match command {
            // The server loop answers the surface lease itself, before any
            // command reaches the app; reaching here is a routing bug.
            EndpointCommand::ClientShellSurfaceSet(_) => {
                tracing::warn!("client_shell.surface.set routed to the app by mistake");
                rejected("client_shell.surface.set is not handled by the app")
            }
            EndpointCommand::WorkspaceCreate(params) => self.handle_workspace_create(params, ctx),
            EndpointCommand::WorkspaceFocus(target) => self.handle_workspace_focus(&target),
            EndpointCommand::WorkspaceRename(params) => self.handle_workspace_rename(params),
            EndpointCommand::WorkspaceCheckoutRoot(params) => {
                self.handle_workspace_checkout_root(&params)
            }
            EndpointCommand::WorkspaceMove(params) => self.handle_workspace_move(&params),
            EndpointCommand::WorkspaceClose(params) => self.handle_workspace_close(&params),
            EndpointCommand::PaneSplit(params) => self.handle_pane_split(&params, ctx),
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
impl EndpointContext {
    /// A requester that presents no geometry of its own.
    pub(crate) fn without_geometry() -> Self {
        Self {
            requester_geometry: None,
        }
    }
}

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
    /// draining pending internal events, for a requester that presents no
    /// geometry. Only the answer is returned.
    pub(crate) fn handle_endpoint_command(
        &mut self,
        command: EndpointCommand,
    ) -> Result<EndpointReply, EndpointError> {
        self.handle_endpoint_command_in(command, &EndpointContext::without_geometry())
            .result
    }

    /// As `handle_endpoint_command`, for a requester with `ctx`, returning the
    /// whole outcome (the navigation effect included).
    pub(crate) fn handle_endpoint_command_in(
        &mut self,
        command: EndpointCommand,
        ctx: &EndpointContext,
    ) -> EndpointOutcome {
        self.drain_all_internal_events();
        self.handle_endpoint_command_with_render(command, ctx)
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
    use shepr_protocol::PublicPaneId;

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
        assert!(matches!(
            surface.expect_err("surface lease is answered by the loop"),
            EndpointError::Rejected(_)
        ));
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
        let read = app.handle_endpoint_command_with_render(
            EndpointCommand::PaneSelectionRead(shepr_protocol::command::PaneSelectionReadParams {
                pane_id: PublicPaneId::new(&WorkspaceId::from_number(1).expect("number"), 1),
                anchor: shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 0,
                },
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 0,
                },
            }),
            &EndpointContext::without_geometry(),
        );
        assert_eq!(read.render, RenderDemand::None);

        let rename = app.handle_endpoint_command_with_render(
            EndpointCommand::PaneRename(shepr_protocol::command::PaneRenameParams {
                pane_id: PublicPaneId::new(&WorkspaceId::from_number(1).expect("number"), 1),
                label: Some("logs".into()),
            }),
            &EndpointContext::without_geometry(),
        );
        assert_eq!(rename.render, RenderDemand::None);
        assert!(rename.result.is_err());
    }

    #[test]
    fn workspace_rename_trims_and_clears_and_renders_what_it_changed() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );
        app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("rename")];
        let workspace_id = app.state.workspaces[0].id.clone();
        let mut rename = |label: &str| {
            let before = app.state.shell_projection_revision;
            let outcome = app.handle_endpoint_command_with_render(
                EndpointCommand::WorkspaceRename(shepr_protocol::command::WorkspaceRenameParams {
                    workspace_id: workspace_id.clone(),
                    label: label.into(),
                }),
                &EndpointContext::without_geometry(),
            );
            assert!(outcome.result.is_ok(), "{label:?}");
            (
                app.state.workspaces[0].custom_name.clone(),
                outcome.effects,
                outcome.render,
                before,
                app.state.shell_projection_revision,
            )
        };

        let (name, effects, render, before, after) = rename("  logs  ");
        assert_eq!(name, Some("logs".to_owned()));
        assert!(effects.shell_projection_changed);
        assert_eq!(render, RenderDemand::Full);
        assert_ne!(after, before);

        let (name, effects, render, before, after) = rename("logs");
        assert_eq!(name, Some("logs".to_owned()));
        assert_eq!(effects, EndpointEffects::default());
        assert_eq!(render, RenderDemand::None);
        assert_eq!(after, before);

        let (name, effects, render, before, after) = rename("   ");
        assert_eq!(name, None);
        assert!(effects.shell_projection_changed);
        assert_eq!(render, RenderDemand::Full);
        assert_ne!(after, before);

        let (name, effects, render, before, after) = rename("");
        assert_eq!(name, None);
        assert_eq!(effects, EndpointEffects::default());
        assert_eq!(render, RenderDemand::None);
        assert_eq!(after, before);
    }

    #[test]
    fn pane_exit_keeps_the_workspace_when_other_panes_remain() {
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
        assert_eq!(app.state.workspaces[0].pane_count(), 1);
    }

    #[test]
    fn pane_exit_removes_the_workspace_it_empties() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );
        app.state.workspaces = vec![
            shepr_mux::workspace::Workspace::test_new("pane-exit-first"),
            shepr_mux::workspace::Workspace::test_new("pane-exit-second"),
        ];
        app.state.ensure_test_terminals();
        let first_root = app.state.workspaces[0].root_pane();
        let second_root = app.state.workspaces[1].root_pane();

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: first_root,
            exit_reason: shepr_platform::ChildExitReason::Exited,
        });
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].root_pane(), second_root);

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
        let pane_id = workspace.root_pane();
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
