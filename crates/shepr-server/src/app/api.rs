mod cwd;
mod detect;
mod endpoint;
mod layouts;
mod panes;
pub(super) mod responses;
pub(super) mod session;
mod workspaces;

use super::SpawnGeometry;
use super::{App, Outcome};
use shepr_api::error::ApiResult;
use shepr_protocol::WorkspaceId;
use shepr_protocol::command::{EndpointAppCommand, EndpointError, EndpointReply};

use endpoint::HandlerResult;
pub(crate) use endpoint::{EndpointEffects, Invalidation};

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
    pub(crate) invalidation: Invalidation,
}

impl App {
    fn json_pane(
        &self,
        raw: &str,
    ) -> Result<shepr_core::layout::PaneId, shepr_api::error::ApiError> {
        self.json_pane_with_id(raw).map(|(_, pane_id)| pane_id)
    }

    /// [`Self::json_pane`], also returning the parsed public id.
    fn json_pane_with_id(
        &self,
        raw: &str,
    ) -> Result<
        (shepr_protocol::PublicPaneId, shepr_core::layout::PaneId),
        shepr_api::error::ApiError,
    > {
        let public_id = raw.parse::<shepr_protocol::PublicPaneId>().map_err(|_| {
            shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::InvalidPaneId,
                format!("invalid pane id {raw:?}; expected w<workspace>:p<pane>"),
            )
        })?;
        self.state
            .resolve_pane(&public_id)
            .map(|pane| (public_id, pane.id()))
            .ok_or_else(|| shepr_api::error::ApiError::pane_not_found(raw))
    }

    /// Observes committed projection changes, including a mutation followed by
    /// a refusal. Surface-only changes travel in the mutation's effects.
    pub(super) fn observe_projection_change<T>(
        &mut self,
        apply: impl FnOnce(&mut Self) -> T,
    ) -> (T, bool) {
        let before = self.state.shell_projection_revision;
        let result = apply(self);
        (result, self.state.shell_projection_revision != before)
    }

    pub(crate) fn handle_api_request_with_render(
        &mut self,
        request: shepr_api::schema::AppRequest,
    ) -> Outcome {
        let (response, view_changed) = self.observe_projection_change(|app| {
            app.handle_api_request_after_internal_events_drained(request)
        });
        Outcome {
            response,
            view_changed,
        }
    }

    fn handle_api_request_after_internal_events_drained(
        &mut self,
        request: shepr_api::schema::AppRequest,
    ) -> ApiResult {
        use shepr_api::schema::AppMethod;

        match request.method {
            AppMethod::ServerSummary(_) => Ok(self.server_summary()),
            AppMethod::DetectCapture(target) => self.handle_detect_capture(&target),
            AppMethod::DetectExplain(target) => self.handle_detect_explain(&target),
            AppMethod::PaneReportAgent(params) => self.handle_pane_report_agent(params),
            AppMethod::PaneReportAgentSession(params) => {
                self.handle_pane_report_agent_session(params)
            }
        }
    }

    /// The session's counts for `shepr status`: workspaces, panes, and the
    /// agents the sidebar lists, with how many of them are blocked.
    fn server_summary(&self) -> shepr_api::schema::ResponseResult {
        let agents = self.collect_agent_infos();
        shepr_api::schema::ResponseResult::ServerSummary {
            workspaces: self.state.workspaces.len(),
            panes: self
                .state
                .workspaces
                .iter()
                .map(|workspace| workspace.tree().len())
                .sum(),
            agents: agents.len(),
            blocked_agents: agents
                .iter()
                .filter(|agent| agent.agent_status == shepr_protocol::AgentStatus::Blocked)
                .count(),
        }
    }

    /// Runs one client-shell command against the app state the server loop
    /// has already drained internal events into. Targets, the creation source
    /// and recorded geometry are all resolved here, against that state. The
    /// command's navigation effect is returned, not applied: navigation is per
    /// client and the loop owns it, as it owns the reconcile, the geometry
    /// settlement and the reply's focus flags that follow.
    pub(crate) fn handle_endpoint_app_command_with_render(
        &mut self,
        command: EndpointAppCommand,
        ctx: &EndpointContext,
    ) -> EndpointOutcome {
        let ((result, navigate, effects), projection_changed) =
            self.observe_projection_change(|app| {
                let (result, navigate, effects) = match app.dispatch_endpoint_command(command, ctx)
                {
                    Ok(handled) => (Ok(handled.reply), handled.navigate, handled.effects),
                    Err(error) => (Err(error.error), None, error.effects),
                };
                (result, navigate, effects)
            });
        let invalidation = effects.invalidation(projection_changed);
        EndpointOutcome {
            result,
            navigate,
            effects,
            invalidation,
        }
    }

    fn dispatch_endpoint_command(
        &mut self,
        command: EndpointAppCommand,
        ctx: &EndpointContext,
    ) -> HandlerResult {
        match command {
            EndpointAppCommand::WorkspaceCreate(params) => {
                self.handle_workspace_create(params, ctx)
            }
            EndpointAppCommand::WorkspaceFocus(target) => self.handle_workspace_focus(&target),
            EndpointAppCommand::WorkspaceRename(params) => self.handle_workspace_rename(params),
            EndpointAppCommand::WorkspaceMove(params) => self.handle_workspace_move(&params),
            EndpointAppCommand::WorkspaceClose(params) => self.handle_workspace_close(&params),
            EndpointAppCommand::PaneSplit(params) => self.handle_pane_split(&params, ctx),
            EndpointAppCommand::PaneSwap(params) => self.handle_pane_swap(&params),
            EndpointAppCommand::PaneZoom(params) => self.handle_pane_zoom(&params),
            EndpointAppCommand::LayoutSetSplitRatio(params) => {
                self.handle_layout_set_split_ratio(&params)
            }
            EndpointAppCommand::PaneFocusDirection(params) => {
                self.handle_pane_focus_direction(&params)
            }
            EndpointAppCommand::PaneResize(params) => self.handle_pane_resize(&params),
            EndpointAppCommand::PaneScroll(params) => self.handle_pane_scroll(&params),
            EndpointAppCommand::PaneClear(target) => self.handle_pane_clear(&target),
            EndpointAppCommand::PaneSelectionRead(params) => {
                self.handle_pane_selection_read(params)
            }
            EndpointAppCommand::PaneCopyMotion(params) => self.handle_pane_copy_motion(params),
            EndpointAppCommand::PaneCopySearch(params) => self.handle_pane_copy_search(&params),
            EndpointAppCommand::PaneFocus(target) => self.handle_pane_focus(&target),
            EndpointAppCommand::PaneInputSet(params) => self.handle_pane_input_set(&params),
            EndpointAppCommand::PaneRename(params) => self.handle_pane_rename(params),
            EndpointAppCommand::PaneClose(target) => self.handle_pane_close(&target),
        }
    }
}

#[cfg(test)]
use shepr_protocol::command::EndpointCommand;

#[cfg(test)]
impl EndpointOutcome {
    /// Whether the command left any render owed.
    pub(crate) fn view_changed(&self) -> bool {
        self.invalidation != Invalidation::None
    }
}

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
    /// Runs the App API handler for a test whose state already includes all
    /// internal events relevant to the request. Queue draining belongs to the
    /// headless server, which also applies server-side event forwarding.
    pub(crate) fn handle_api_request(&mut self, request: shepr_api::schema::AppRequest) -> String {
        let id = request.id.clone();
        shepr_api::error::encode_result(
            id,
            self.handle_api_request_after_internal_events_drained(request),
        )
    }

    /// Test adapter for the wire command. Production dispatch accepts only
    /// `EndpointAppCommand`, so loop-owned commands cannot reach the app.
    pub(crate) fn handle_endpoint_command_with_render(
        &mut self,
        command: EndpointCommand,
        ctx: &EndpointContext,
    ) -> EndpointOutcome {
        match command.into_app_command() {
            Ok(command) => self.handle_endpoint_app_command_with_render(command, ctx),
            Err(command) => EndpointOutcome {
                result: Err(EndpointError::Internal(format!(
                    "{} is not handled by the app",
                    command.name()
                ))),
                navigate: None,
                effects: EndpointEffects::default(),
                invalidation: Invalidation::None,
            },
        }
    }

    /// Runs the App handler for a test with explicitly prepared state and no
    /// requester geometry. Pending events must be handled by the headless
    /// server before a test relies on them.
    pub(crate) fn handle_endpoint_command(
        &mut self,
        command: EndpointCommand,
    ) -> Result<EndpointReply, EndpointError> {
        self.handle_endpoint_command_with_render(command, &EndpointContext::without_geometry())
            .result
    }

    /// As `handle_endpoint_command`, for a requester with `ctx`, returning the
    /// whole outcome (the navigation effect included).
    pub(crate) fn handle_endpoint_command_in(
        &mut self,
        command: EndpointCommand,
        ctx: &EndpointContext,
    ) -> EndpointOutcome {
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
        for (_pane_id, runtime) in runtimes {
            drop(runtime);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_agent::{Agent, AgentState};
    use shepr_protocol::PublicPaneId;

    #[test]
    fn the_viewing_request_answered_by_the_loop_is_reported_as_misrouted() {
        let mut app = App::new(&shepr_config::ServerConfig::default());

        let surface = app.handle_endpoint_command(EndpointCommand::ClientShellSurfaceSet(
            shepr_protocol::command::ClientShellSurfaceSetParams { active: true },
        ));
        assert!(matches!(
            surface.expect_err("viewing request is answered by the loop"),
            EndpointError::Internal(_)
        ));
    }

    #[test]
    fn read_only_commands_do_not_force_a_render() {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let read = app.handle_endpoint_command_with_render(
            EndpointCommand::PaneSelectionRead(shepr_protocol::command::PaneSelectionReadParams {
                pane_id: PublicPaneId::new(
                    &WorkspaceId::from_number(1).expect("number"),
                    shepr_protocol::PanePublicNumber::new(1).expect("nonzero literal"),
                ),
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
        assert!(!read.view_changed());

        let rename = app.handle_endpoint_command_with_render(
            EndpointCommand::PaneRename(shepr_protocol::command::PaneRenameParams {
                pane_id: PublicPaneId::new(
                    &WorkspaceId::from_number(1).expect("number"),
                    shepr_protocol::PanePublicNumber::new(1).expect("nonzero literal"),
                ),
                label: Some("logs".into()),
            }),
            &EndpointContext::without_geometry(),
        );
        assert!(!rename.view_changed());
        assert!(rename.result.is_err());
    }

    #[test]
    fn workspace_rename_trims_defaults_and_renders_what_it_changed() {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        app.state
            .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("rename")]);
        let workspace_id = app.state.ws(0).id();
        let directory_name = shepr_core::workspace_label::default_workspace_label(
            app.state.ws(0).identity_cwd().as_path(),
        );
        let mut rename = |label: &str| {
            let before = app.state.shell_projection_revision;
            let outcome = app.handle_endpoint_command_with_render(
                EndpointCommand::WorkspaceRename(shepr_protocol::command::WorkspaceRenameParams {
                    workspace_id,
                    label: Some(label.into()),
                }),
                &EndpointContext::without_geometry(),
            );
            assert!(outcome.result.is_ok(), "{label:?}");
            let view_changed = outcome.view_changed();
            (
                app.state.ws(0).name().to_owned(),
                outcome.effects,
                view_changed,
                before,
                app.state.shell_projection_revision,
            )
        };

        let (name, effects, render, before, after) = rename("  logs  ");
        assert_eq!(name, "logs");
        assert_eq!(effects, EndpointEffects::default());
        assert!(render);
        assert_ne!(after, before);

        let (name, effects, render, before, after) = rename("logs");
        assert_eq!(name, "logs");
        assert_eq!(effects, EndpointEffects::default());
        assert!(!render);
        assert_eq!(after, before);

        // A blank name names the workspace after its directory.
        let (name, effects, render, before, after) = rename("   ");
        assert_eq!(name, directory_name);
        assert_eq!(effects, EndpointEffects::default());
        assert!(render);
        assert_ne!(after, before);

        let (name, effects, render, before, after) = rename("");
        assert_eq!(name, directory_name);
        assert_eq!(effects, EndpointEffects::default());
        assert!(!render);
        assert_eq!(after, before);
    }

    #[test]
    fn server_summary_counts_workspaces_panes_and_blocked_agents() {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let mut first = shepr_mux::workspace::Workspace::test_new("summary-first");
        let working = first.tree().root();
        let blocked = first.test_split(shepr_core::layout::Direction::Horizontal);
        let _shell = first.test_split(shepr_core::layout::Direction::Vertical);
        app.state.test_set_workspaces(vec![
            first,
            shepr_mux::workspace::Workspace::test_new("summary-second"),
        ]);
        app.state
            .terminal_mut(working)
            .set_detected_state(Some(Agent::Codex), AgentState::Working);
        app.state
            .terminal_mut(blocked)
            .ownership_mut()
            .set_detected_state_with_screen_signals_at(
                Some(Agent::Claude),
                AgentState::Blocked,
                true,
                false,
                std::time::Instant::now(),
            );

        let response: serde_json::Value =
            serde_json::from_str(&app.handle_api_request(shepr_api::schema::AppRequest {
                id: "summary".into(),
                method: shepr_api::schema::AppMethod::ServerSummary(
                    shepr_api::schema::ServerSummaryParams::default(),
                ),
            }))
            .expect("summary JSON");
        assert_eq!(
            response["result"],
            serde_json::json!({
                "type": "server_summary",
                "workspaces": 2,
                "panes": 4,
                "agents": 2,
                "blocked_agents": 1,
            })
        );
    }

    #[test]
    fn pane_exit_keeps_the_workspace_when_other_panes_remain() {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let mut workspace = shepr_mux::workspace::Workspace::test_new("pane-exit-layout");
        let dead_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.test_set_workspaces(vec![workspace]);

        report_runtime_exit(&mut app, dead_pane);

        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.ws(0).tree().len(), 1);
    }

    #[test]
    fn pane_exit_removes_the_workspace_it_empties() {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        app.state.test_set_workspaces(vec![
            shepr_mux::workspace::Workspace::test_new("pane-exit-first"),
            shepr_mux::workspace::Workspace::test_new("pane-exit-second"),
        ]);
        let first_root = app.state.ws(0).tree().root();
        let second_root = app.state.ws(1).tree().root();

        report_runtime_exit(&mut app, first_root);
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.ws(0).tree().root(), second_root);

        report_runtime_exit(&mut app, second_root);
        assert!(app.state.workspaces.is_empty());
    }

    /// The pane's process exit, reported by a runtime installed for it.
    fn report_runtime_exit(app: &mut App, pane_id: shepr_core::layout::PaneId) {
        app.insert_idle_test_runtime(pane_id);
        let exit = app.from_pane_runtime(
            pane_id,
            shepr_mux::events::RuntimeEvent::PaneDied {
                ending: shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Exited),
                ended_at: std::time::Instant::now(),
            },
        );
        crate::server::headless::HeadlessServer::replay_test_exit_for_app(app, exit);
    }

    #[test]
    fn process_exit_releases_a_newer_hook_owned_agent() {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("stale-agent-exit");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let terminal = app.state.terminal_mut(pane_id);
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
        // The detector drops an observation older than the last one it took,
        // so the probe is stamped after the detection seeded above.
        let observed_at = std::time::Instant::now();
        terminal
            .set_hook_report_at(
                shepr_agent::ReportOrigin::parse("shepr:codex", "codex").expect("test origin"),
                AgentState::Working,
                shepr_agent::resume::AgentSessionRef::id("codex-session"),
                Some(1),
                shepr_detect::ownership::HookClockSample {
                    monotonic: observed_at + std::time::Duration::from_secs(1),
                    wall: std::time::SystemTime::now(),
                },
            )
            .expect("test precondition");

        // An official hook's state does not outlive its process: the exit
        // releases it at once, even though the report came after the probe.
        app.insert_idle_test_runtime(pane_id);
        let exit_report = app.from_pane_runtime(
            pane_id,
            shepr_mux::events::RuntimeEvent::StateChanged {
                agent: Some(Agent::Codex),
                detection: shepr_detect::Detection::new(AgentState::Idle, false),
                process_exited: true,
                observed_at,
            },
        );
        app.handle_internal_event(exit_report);

        let terminal = app.state.terminal(pane_id).expect("test precondition");
        assert_eq!(terminal.ownership().state(), AgentState::Idle);
        assert!(terminal.ownership().hook_authority().is_none());
    }
}
