use super::{App, CheckpointGeneration};
use shepr_agent::{Agent, AgentState};
use shepr_core::layout::PaneId;
use shepr_mux::events::{AppEvent, RuntimeEvent, RuntimeGeneration};
use std::time::Instant;

/// Events the pure data reducer can apply. Runtime removal, Git completion and
/// clipboard delivery stay at the App boundary, outside this type.
#[derive(Debug)]
pub(crate) enum StateEvent {
    AgentProcessDetected {
        pane_id: PaneId,
        agent: Agent,
        observed_at: Instant,
    },
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        detection: shepr_detect::Detection,
        process_exited: bool,
        observed_at: Instant,
    },
    HookStateReported {
        pane_id: PaneId,
        sample: shepr_detect::ownership::HookClockSample,
        origin: shepr_agent::ReportOrigin,
        state: AgentState,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
    },
    AgentSessionReported {
        pane_id: PaneId,
        sample: shepr_detect::ownership::HookClockSample,
        origin: shepr_agent::ReportOrigin,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        session_start_source: shepr_agent::resume::ReportedSessionStart,
    },
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: shepr_mux::UsableCwd,
    },
}

/// A hook report that arrived through the JSON API. It has no runtime
/// envelope: the API has already resolved the pane and validated the report's
/// origin, so this is the one entry through which a report reaches the
/// reducer, and it cannot name any other state event.
#[derive(Debug)]
pub(crate) struct ApiReport(StateEvent);

impl ApiReport {
    pub(crate) fn hook_state(
        pane_id: PaneId,
        sample: shepr_detect::ownership::HookClockSample,
        origin: shepr_agent::ReportOrigin,
        state: AgentState,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
    ) -> Self {
        Self(StateEvent::HookStateReported {
            pane_id,
            sample,
            origin,
            state,
            seq,
            session_ref,
        })
    }

    pub(crate) fn agent_session(
        pane_id: PaneId,
        sample: shepr_detect::ownership::HookClockSample,
        origin: shepr_agent::ReportOrigin,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        session_start_source: shepr_agent::resume::ReportedSessionStart,
    ) -> Self {
        Self(StateEvent::AgentSessionReported {
            pane_id,
            sample,
            origin,
            seq,
            session_ref,
            session_start_source,
        })
    }
}

/// A runtime payload that passed admission, bound to the pane and the runtime
/// generation that produced it. Admission is the only way to make one.
#[derive(Debug)]
pub(crate) struct AdmittedPane {
    pub(crate) pane_id: PaneId,
    generation: RuntimeGeneration,
    pub(crate) event: RuntimeEvent,
}

impl AdmittedPane {
    /// The pane's death, or the payload back when it is anything else.
    pub(crate) fn into_death(self) -> Result<PaneDeath, Self> {
        match self.event {
            RuntimeEvent::PaneDied { ending, ended_at } => Ok(PaneDeath {
                pane_id: self.pane_id,
                generation: self.generation,
                ending,
                ended_at,
            }),
            event => Err(Self {
                pane_id: self.pane_id,
                generation: self.generation,
                event,
            }),
        }
    }
}

/// What the App's reducer takes from the server socket's transport: an event
/// that passed admission, one value per admission path.
#[derive(Debug)]
pub(crate) enum Admitted {
    /// A pane runtime's payload from its current generation.
    Pane(AdmittedPane),
    /// The Git worker's answer, which has no pane runtime to check.
    GitRefreshed(shepr_git::RefreshOutcome<shepr_protocol::WorkspaceId>),
}

/// A pane's child process exit, as its admitted runtime reported it.
#[derive(Debug)]
pub(crate) struct PaneDeath {
    pane_id: PaneId,
    generation: RuntimeGeneration,
    ending: shepr_mux::pane::PaneEnding,
    ended_at: Instant,
}

impl PaneDeath {
    /// The death back in the envelope it arrived in.
    pub(crate) fn into_envelope(self) -> AppEvent {
        RuntimeEvent::PaneDied {
            ending: self.ending,
            ended_at: self.ended_at,
        }
        .enveloped(self.pane_id, self.generation)
    }
}

/// The checkpoint decision `App::prepare_pane_exit` made for a pane exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointDecision {
    /// Removed without a checkpoint of its own.
    Unchecked,
    /// Checkpointed, and the checkpoint is already settled: removal can
    /// follow at once.
    Settled,
    /// Checkpointed, and held until the checkpoint of this generation is
    /// durable.
    Held(CheckpointGeneration),
}

/// A pane death bound to the checkpoint decision made for it. Only
/// `App::prepare_pane_exit` decides, and a replayed hold is made from the
/// death it was held for, so a death cannot be applied with another death's
/// preparation. `App::handle_prepared_pane_exit` finishes the exit as decided
/// even if the pane's core breaks in between.
#[derive(Debug)]
pub(crate) struct PreparedPaneExit {
    death: PaneDeath,
    decision: CheckpointDecision,
}

impl PreparedPaneExit {
    /// A death that was held for the checkpoint of `generation` and is
    /// replayed now: it was decided as checkpointed.
    pub(crate) fn replayed(death: PaneDeath, generation: CheckpointGeneration) -> Self {
        Self {
            death,
            decision: CheckpointDecision::Held(generation),
        }
    }

    /// The checkpoint generation the exit is held for, if it is held.
    pub(crate) fn held_generation(&self) -> Option<CheckpointGeneration> {
        match self.decision {
            CheckpointDecision::Held(generation) => Some(generation),
            CheckpointDecision::Unchecked | CheckpointDecision::Settled => None,
        }
    }

    /// The death back in the envelope it arrived in, to hold it.
    pub(crate) fn into_envelope(self) -> AppEvent {
        self.death.into_envelope()
    }

    fn checkpointed(&self) -> bool {
        !matches!(self.decision, CheckpointDecision::Unchecked)
    }
}

/// What `App::prepare_pane_exit` returns: the prepared exit, and whether
/// publishing the process exit changed the shell projection (the agent going
/// idle changes the sidebar even when the pane stays).
#[derive(Debug)]
pub(crate) struct PaneExitPrepared {
    pub(crate) prepared: PreparedPaneExit,
    pub(crate) projection_changed: bool,
}

impl App {
    fn live_workspace_identity_cwd(
        &self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<std::path::PathBuf> {
        let workspace = self.state.workspaces().get(workspace_id)?;
        Some(
            workspace
                .resolved_identity_cwd(&self.terminal_runtimes)
                .into_path_buf(),
        )
    }

    /// The server side of a Git refresh: completes the scheduled refresh,
    /// logs the read errors the worker saw first, and applies each status to
    /// the workspace it was asked for, while that workspace still resolves to
    /// the cwd the status was read for.
    fn handle_git_status_refreshed(
        &mut self,
        outcome: shepr_git::RefreshOutcome<shepr_protocol::WorkspaceId>,
    ) -> bool {
        self.git_refresh.finish(self.clock.now);
        for error in &outcome.new_read_errors {
            shepr_platform::structured_log!(WARN, event = git.read, outcome = "error", %error, "git status read failed");
        }
        let results = outcome
            .statuses
            .into_iter()
            .map(|result| {
                let resolved_identity_cwd = self.live_workspace_identity_cwd(&result.owner);
                (result, resolved_identity_cwd)
            })
            .collect();
        self.state.apply_workspace_git_statuses(results)
    }

    /// Check the producer before publication, checkpointing, or forwarding.
    /// A discarded resume attempt can finish after a successor runtime starts.
    ///
    /// Only events produced without a pane runtime (the Git worker) are
    /// admitted bare; every runtime-produced kind must arrive in its runtime's
    /// envelope, and the transport type has no bare form of one. API reports
    /// enter through `handle_api_report`.
    pub(crate) fn admit_event(&self, ev: AppEvent) -> Option<Admitted> {
        match ev {
            AppEvent::Runtime {
                pane_id,
                generation,
                event,
            } => self
                .runtime_generation_is_current(pane_id, generation)
                .then_some(Admitted::Pane(AdmittedPane {
                    pane_id,
                    generation,
                    event: *event,
                })),
            AppEvent::GitStatusRefreshed { outcome } => Some(Admitted::GitRefreshed(outcome)),
        }
    }

    fn runtime_generation_is_current(
        &self,
        pane_id: PaneId,
        generation: RuntimeGeneration,
    ) -> bool {
        self.terminal_runtimes
            .get(&pane_id)
            .is_some_and(|runtime| runtime.generation() == generation)
    }

    /// Applies an API report: the one entry for the reports the JSON API
    /// admits, which carry no runtime envelope.
    pub(crate) fn handle_api_report(&mut self, report: ApiReport) -> bool {
        let (changed, projection_changed) = self.observe_projection_change(|app| {
            let changed =
                app.state.handle_state_event(report.0) != super::actions::StateUpdate::Unchanged;
            app.apply_lifecycle_authority_changes();
            changed
        });
        changed || projection_changed
    }

    /// Applies an event that has already passed `admit_event`.
    pub(crate) fn handle_admitted_event(&mut self, admitted: Admitted) -> bool {
        let (changed, projection_changed) =
            self.observe_projection_change(|app| app.apply_admitted(admitted));
        changed || projection_changed
    }

    /// Publishes the process exit once, then asks the App's session policy
    /// whether the death must wait for a checkpoint before removal. The
    /// decision comes back bound to the death for `handle_prepared_pane_exit`.
    pub(crate) fn prepare_pane_exit(&mut self, death: PaneDeath) -> PaneExitPrepared {
        let (prepared, projection_changed) =
            self.observe_projection_change(|app| app.decide_pane_exit(death));
        PaneExitPrepared {
            prepared,
            projection_changed,
        }
    }

    fn decide_pane_exit(&mut self, death: PaneDeath) -> PreparedPaneExit {
        let pane_id = death.pane_id;
        // A core that broke after the pane ended has nothing new to give a
        // checkpoint. The ending carries that answer from here on: a prepared
        // exit decides once and keeps it through the checkpoint decision.
        let core_intact = !self
            .terminal_runtimes
            .get(&pane_id)
            .is_some_and(shepr_mux::pane::PaneRuntime::terminal_core_broken);
        let ending = death.ending.with_core_intact(core_intact);
        // Published before the checkpoint is requested: resolving the pane's
        // saved identity (its own, or one a detector release took just
        // before) marks the session dirty, so no older checkpoint can settle
        // this exit without it.
        self.publish_pane_process_exit(pane_id, ending, death.ended_at);
        // This probe decides whether to checkpoint before removal is allowed.
        // The ending itself says whether it asks for one (it records whether
        // the terminal core could still be read). The removal itself happens
        // when the event is applied, since a held exit can outlive
        // intervening workspace changes.
        let decision = if ending.needs_checkpoint() && self.state.pane(pane_id).is_some() {
            self.request_pane_exit_checkpoint()
                .map_or(CheckpointDecision::Settled, CheckpointDecision::Held)
        } else {
            CheckpointDecision::Unchecked
        };
        PreparedPaneExit { death, decision }
    }

    /// Finishes a pane exit whose publication and checkpoint decision the App
    /// already made, as that decision says. The death's runtime generation is
    /// checked again here, next to the removal it guards. On the server's
    /// path this repeats the admission the same pass just made (a held exit
    /// waits as an envelope and is admitted afresh when replayed), so in
    /// production it never refuses; it stays because a `PreparedPaneExit` is
    /// a value a caller could keep past its runtime, and admission is a pure
    /// lookup with no effect to apply twice.
    pub(crate) fn handle_prepared_pane_exit(&mut self, prepared: &PreparedPaneExit) -> bool {
        if prepared
            .held_generation()
            .is_some_and(|generation| !self.pane_exit_checkpoint_generation_settled(generation))
        {
            return false;
        }
        if !self.runtime_generation_is_current(prepared.death.pane_id, prepared.death.generation) {
            return false;
        }
        let checkpointed = prepared.checkpointed();
        let pane_id = prepared.death.pane_id;
        let (changed, projection_changed) =
            self.observe_projection_change(|app| app.apply_pane_removal(pane_id, checkpointed));
        changed || projection_changed
    }

    fn apply_admitted(&mut self, admitted: Admitted) -> bool {
        match admitted {
            Admitted::GitRefreshed(outcome) => self.handle_git_status_refreshed(outcome),
            Admitted::Pane(AdmittedPane { pane_id, event, .. }) => {
                self.apply_pane_event(pane_id, event)
            }
        }
    }

    fn apply_pane_event(&mut self, pane_id: PaneId, event: RuntimeEvent) -> bool {
        match event {
            // The headless server forwards a clipboard write to the clients;
            // there is no state to change. Pane removal requires the
            // preparation path, including its durable hold:
            // `handle_prepared_pane_exit`.
            RuntimeEvent::ClipboardWrite { .. } | RuntimeEvent::PaneDied { .. } => false,
            RuntimeEvent::PaneLaunchSettled { settlement } => {
                self.handle_pane_launch_settled(pane_id, settlement)
            }
            event @ (RuntimeEvent::AgentProcessDetected { .. }
            | RuntimeEvent::StateChanged { .. }) => {
                // A detector tick can finish before the watcher publishes
                // PaneDied. Once any observer decided the pane ended, or the
                // child exited before its watcher records that decision, only
                // the exit reason can decide whether to release the resume
                // identity. Ignore queued detector updates, including the
                // identity-clear tick following its process-exit report.
                if self
                    .terminal_runtimes
                    .get(&pane_id)
                    .is_some_and(shepr_mux::pane::PaneRuntime::detector_observations_ended)
                {
                    return false;
                }
                self.apply_runtime_state_event(pane_id, event)
            }
            event @ RuntimeEvent::TerminalCwdReported { .. } => {
                self.apply_runtime_state_event(pane_id, event)
            }
        }
    }

    /// Applies a runtime payload that is a state-level event.
    fn apply_runtime_state_event(&mut self, pane_id: PaneId, event: RuntimeEvent) -> bool {
        let Some(event) = StateEvent::from_runtime(pane_id, event) else {
            return false;
        };
        let terminal_cwd_reported = matches!(event, StateEvent::TerminalCwdReported { .. });
        // A cwd report changes only the projection (the state update reports
        // Unchanged), so the projection revision is what says the cwd moved.
        let projection_before = self.state.shell_projection_revision();
        let state_changed =
            self.state.handle_state_event(event) != super::actions::StateUpdate::Unchanged;
        self.apply_lifecycle_authority_changes();
        let cwd_moved =
            terminal_cwd_reported && !self.state.shell_projection_is_current(projection_before);
        if cwd_moved {
            self.request_git_identity_refresh(self.clock.now);
        }
        state_changed || cwd_moved
    }

    /// Removes a dead pane from the layout. `checkpointed` is the prepared
    /// exit's recorded decision.
    fn apply_pane_removal(&mut self, pane_id: PaneId, checkpointed: bool) -> bool {
        let session_was_dirty = self.state.session_dirty();
        let outcome = self.state.remove_pane(pane_id);
        let removed = outcome.is_some();
        // A pane already gone has not completed the checkpointed exit whose
        // save state this method advances.
        if removed && checkpointed {
            self.finish_checkpointed_pane_exit_after_event(session_was_dirty);
        }
        self.apply_lifecycle_authority_changes();
        if let Some(outcome) = &outcome {
            self.shutdown_detached_pane_runtimes(&outcome.removed);
        }
        removed
    }

    fn publish_pane_process_exit(
        &mut self,
        pane_id: shepr_core::layout::PaneId,
        ending: shepr_mux::pane::PaneEnding,
        ended_at: std::time::Instant,
    ) {
        self.state
            .publish_pane_process_exit(pane_id, ending, ended_at);
        self.apply_lifecycle_authority_changes();
    }

    /// Mirrors full-lifecycle authority into the runtime of every pane an
    /// update touched. The runtime keeps it in an atomic the detector task
    /// reads off the app thread, so it cannot be derived there on demand;
    /// this drain and `install_runtime` are its only writers, and
    /// both read the live `full_lifecycle_hook_authority_active()`. Writing an
    /// unchanged value is cheap: the runtime only notifies on a transition.
    pub(super) fn apply_lifecycle_authority_changes(&mut self) {
        // Keep the runtime mirror: ownership is plain app-thread state, while
        // detection must read its pause gate without an app lock. Installation
        // seeds a new runtime from existing ownership; this drain propagates
        // later mutations. Neither can replace the other, including on restore.
        for pane_id in self.state.drain_lifecycle_authority_dirty() {
            if let (Some(terminal), Some(runtime)) = (
                self.state.terminal(pane_id),
                self.terminal_runtimes.get(&pane_id),
            ) {
                runtime.set_full_lifecycle_authority_active(
                    terminal.ownership().full_lifecycle_hook_authority_active(),
                );
            }
        }
    }

    /// Registers `runtime` as the live runtime of `pane_id`, which must be in
    /// the state: the runtime is dropped, and so torn down, when it is not.
    pub(super) fn install_runtime(
        &mut self,
        pane_id: PaneId,
        runtime: shepr_mux::pane::PaneRuntime,
    ) {
        let Some(terminal) = self.state.terminal(pane_id) else {
            shepr_platform::structured_log!(
                ERROR, event = pane.runtime_install, outcome = "missing_pane",
                pane = %pane_id,
                "a runtime was installed for a pane that is not in the state; dropping it"
            );
            return;
        };
        runtime.set_full_lifecycle_authority_active(
            terminal.ownership().full_lifecycle_hook_authority_active(),
        );
        self.terminal_runtimes.insert(pane_id, runtime);
    }

    pub(super) fn abandon_agent_resume(
        &mut self,
        pane_id: PaneId,
        failure: shepr_mux::terminal::PaneStartFailure,
        now: std::time::Instant,
    ) {
        self.state.abandon_pane_agent_resume(pane_id, failure, now);
        self.apply_lifecycle_authority_changes();
    }

    /// Tells one pane it gained or lost terminal focus. Which panes hold focus
    /// is decided per client on the server (`sync_pane_focus`); a pane with no
    /// live runtime is skipped.
    pub(crate) fn send_pane_focus_event(
        &self,
        pane_id: shepr_core::layout::PaneId,
        event: shepr_vt::FocusEvent,
    ) {
        let Some(runtime) = self.lookup_runtime(pane_id) else {
            return;
        };
        runtime.try_send_focus_event(event);
    }
}

#[cfg(test)]
impl PreparedPaneExit {
    /// A death whose checkpoint is already settled.
    pub(crate) fn settled(death: PaneDeath) -> Self {
        Self {
            death,
            decision: CheckpointDecision::Settled,
        }
    }

    /// Whether the exit was checkpointed and the checkpoint was already
    /// settled when it was prepared.
    pub(crate) fn is_settled(&self) -> bool {
        self.decision == CheckpointDecision::Settled
    }
}

#[cfg(test)]
impl App {
    /// Admits a transport event and applies it. A pane's death is never
    /// applied this way: its removal needs `prepare_pane_exit`.
    pub(crate) fn handle_internal_event_with_view_change(&mut self, ev: AppEvent) -> bool {
        match self.admit_event(ev) {
            Some(admitted) => self.handle_admitted_event(admitted),
            None => false,
        }
    }

    pub(crate) fn handle_internal_event(&mut self, ev: AppEvent) {
        let _ = self.handle_internal_event_with_view_change(ev);
    }

    /// The death of `pane_id` as its current test runtime reports it. Panics
    /// if the pane has no runtime.
    pub(crate) fn test_pane_death(
        &self,
        pane_id: PaneId,
        reason: shepr_mux::pane::PaneEndReason,
        ended_at: Instant,
    ) -> PaneDeath {
        PaneDeath {
            pane_id,
            generation: self.test_runtime(pane_id).generation(),
            ending: shepr_mux::pane::PaneEnding::new(reason),
            ended_at,
        }
    }
}

#[cfg(test)]
mod pane_exit_event_tests {
    use super::*;
    use crate::test_support::{PaneRuntimeFixture as _, WorkspaceFixture as _};
    use shepr_core::layout::Direction;
    use shepr_mux::pane::{PaneEndReason, PaneEnding};
    use shepr_mux::workspace::Workspace;

    fn app_with_workspaces(names: &[&str]) -> crate::app::TestApp {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        app.state
            .test_set_workspaces(names.iter().map(|name| Workspace::test_new(name)).collect());
        app
    }

    fn report_pane_exit(app: &mut App, pane_id: PaneId) {
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let generation = runtime.generation();
        app.insert_test_runtime(pane_id, runtime);
        crate::server::headless::HeadlessServer::replay_test_exit_for_app(
            app,
            RuntimeEvent::PaneDied {
                ending: PaneEnding::new(PaneEndReason::Exited),
                ended_at: std::time::Instant::now(),
            }
            .enveloped(pane_id, generation),
        );
    }

    #[test]
    fn pane_exit_removes_its_workspace_through_the_app_event_path() {
        let mut app = app_with_workspaces(&["a", "dying", "c"]);
        app.state.seed_bookmark_index(Some(2));
        let pane_id = app.state.ws(1).tree().root();
        app.state.test_clear_session_dirty();

        report_pane_exit(&mut app, pane_id);

        assert_eq!(app.state.workspaces().len(), 2);
        assert_eq!(
            app.state
                .ws(app.state.bookmark_index().expect("bookmarked"))
                .name(),
            "c"
        );
        assert!(app.state.terminal(pane_id).is_none());
        assert!(app.state.session_dirty());
    }

    #[test]
    fn pane_exit_clears_the_bookmark_when_it_removes_the_last_workspace() {
        let mut app = app_with_workspaces(&["only"]);
        app.state.seed_bookmark_index(Some(0));
        let pane_id = app.state.ws(0).tree().root();

        report_pane_exit(&mut app, pane_id);

        assert!(app.state.workspaces().is_empty());
        assert_eq!(app.state.workspaces().bookmark(), None);
    }

    #[test]
    fn pane_exit_keeps_a_workspace_that_still_has_a_pane() {
        let mut app = app_with_workspaces(&["test"]);
        let second_id = app.state.test_split_workspace(0, Direction::Horizontal);

        report_pane_exit(&mut app, second_id);

        assert_eq!(app.state.workspaces().len(), 1);
        assert_eq!(app.state.ws(0).tree().len(), 1);
    }

    #[test]
    fn pane_exit_for_an_unknown_pane_is_a_noop() {
        let mut app = app_with_workspaces(&["test"]);
        let fake_id = shepr_test_fixtures::fixed_pane_id(9999);

        // A pane outside the layout has no runtime that could report its
        // exit; a late report from one that went with its pane is dropped.
        assert!(
            !app.handle_internal_event_with_view_change(
                RuntimeEvent::PaneDied {
                    ending: PaneEnding::new(PaneEndReason::Exited),
                    ended_at: std::time::Instant::now(),
                }
                .enveloped(fake_id, shepr_mux::events::RuntimeGeneration::alloc())
            )
        );

        assert_eq!(app.state.workspaces().len(), 1);
    }
}

#[cfg(test)]
mod runtime_generation_tests {
    use super::*;
    use crate::test_support::*;
    use shepr_mux::pane::{PaneEndReason, PaneEnding};

    #[test]
    fn an_update_that_reports_nothing_still_resyncs_the_detector_pause() {
        let _env = IsolatedEnv::new();
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("authority");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        app.insert_test_runtime(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b""),
        );
        let runtime_authority = |app: &App| {
            app.terminal_runtimes
                .get(&pane_id)
                .expect("runtime")
                .full_lifecycle_authority_active()
        };
        assert!(!runtime_authority(&app));
        let now = app.clock.now;
        let sample = app.clock.hook_sample();
        // The update grants full-lifecycle authority but returns nothing, so
        // only the touched-terminal sync can carry the change to the runtime.
        app.state.update_terminal_state(pane_id, |terminal| {
            let ownership = terminal.ownership_mut();
            let _ = ownership.set_detected_state_with_screen_signals_at(
                Some(shepr_agent::Agent::Omp),
                shepr_agent::AgentState::Idle,
                false,
                false,
                now,
            );
            // Only a full-lifecycle source (Omp, not Codex) pauses detection,
            // and it owns the state only once a session anchors it.
            ownership.set_persisted_agent_session(
                shepr_agent::resume::PersistedAgentSession::new(
                    shepr_agent::AgentSource::parse("shepr:omp").expect("bundled test source"),
                    shepr_agent::resume::AgentSessionRef::id("session").expect("session"),
                )
                .expect("official identity"),
            );
            let _ = ownership.set_hook_report_at(
                shepr_agent::ReportOrigin::parse("shepr:omp").expect("test origin"),
                shepr_agent::AgentState::Idle,
                shepr_agent::resume::AgentSessionRef::id("session"),
                None,
                sample,
            );
            None
        });
        assert!(
            app.state
                .terminal(pane_id)
                .expect("terminal")
                .ownership()
                .full_lifecycle_hook_authority_active()
        );
        app.apply_lifecycle_authority_changes();
        assert!(runtime_authority(&app));

        // An ending reported through the ordinary path withdraws it again.
        app.publish_pane_process_exit(pane_id, PaneEnding::new(PaneEndReason::Exited), now);
        assert!(!runtime_authority(&app));
    }

    #[test]
    fn agent_release_before_shell_death_preserves_exit_checkpoint_identity() {
        let _env = IsolatedEnv::new();
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("kill-ordering");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let session = shepr_agent::resume::PersistedAgentSession::new(
            shepr_agent::AgentSource::parse("shepr:codex").expect("bundled test source"),
            shepr_agent::resume::AgentSessionRef::id("killed-agent").expect("session"),
        )
        .expect("official identity");
        let now = app.clock.now;
        let terminal = app.state.terminal_mut(pane_id);
        terminal
            .ownership_mut()
            .set_persisted_agent_session(session.clone());
        terminal
            .ownership_mut()
            .set_detected_agent_process_at(shepr_agent::Agent::Codex, now);
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let generation = runtime.generation();
        app.insert_test_runtime(pane_id, runtime);
        app.handle_internal_event(
            RuntimeEvent::StateChanged {
                agent: Some(shepr_agent::Agent::Codex),
                detection: shepr_detect::Detection::new(shepr_agent::AgentState::Idle, false),
                process_exited: true,
                observed_at: now,
            }
            .enveloped(pane_id, generation),
        );
        // The agent is released at once ...
        assert_eq!(
            app.state
                .terminal(pane_id)
                .expect("terminal")
                .ownership()
                .current_session_identity_for_persistence(),
            None
        );
        // ... and the shell's signal death right after brings its identity
        // back for the checkpoint.
        let death = app.test_pane_death(
            pane_id,
            PaneEndReason::Signalled,
            now + std::time::Duration::from_millis(100),
        );
        let _ = app.prepare_pane_exit(death);
        // Preparation publishes the final exit before the checkpoint captures
        // the still-present pane; removal happens only after that checkpoint.
        assert!(app.state.ws(0).tree().contains(pane_id));
        assert_eq!(
            app.state
                .terminal(pane_id)
                .expect("terminal")
                .ownership()
                .current_session_identity_for_persistence(),
            Some(session),
        );
    }

    #[tokio::test]
    async fn discarded_and_replaced_runtimes_cannot_remove_a_restored_pane() {
        let _env = IsolatedEnv::new();
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let session = shepr_agent::resume::PersistedAgentSession::new(
            shepr_agent::AgentSource::parse("shepr:codex").expect("bundled test source"),
            shepr_agent::resume::AgentSessionRef::id("restored").expect("session"),
        )
        .expect("persisted identity");
        let terminal = app.state.terminal_mut(pane_id);
        terminal
            .ownership_mut()
            .set_persisted_agent_session(session.clone());
        terminal.plan_agent_resume(test_codex_plan("restored", vec!["codex".into()]));
        let discarded = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let generation = discarded.generation();
        drop(discarded);
        let died = || PaneDeath {
            pane_id,
            generation,
            ending: PaneEnding::new(PaneEndReason::Signalled),
            ended_at: std::time::Instant::now(),
        };
        assert!(!app.handle_internal_event_with_view_change(died().into_envelope()));
        assert!(app.state.ws(0).tree().contains(pane_id));
        assert_eq!(
            app.state
                .terminal(pane_id)
                .expect("terminal")
                .ownership()
                .persisted_agent_session(),
            Some(&session)
        );
        assert!(
            app.state
                .terminal(pane_id)
                .expect("terminal")
                .agent_resume()
                .is_pending()
        );

        let replacement = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let replacement_generation = replacement.generation();
        assert_ne!(generation, replacement_generation);
        app.insert_test_runtime(pane_id, replacement);
        // This also covers a checkpointed exit replayed after replacement:
        // its original envelope is checked again before any removal.
        assert!(!app.handle_prepared_pane_exit(&PreparedPaneExit::settled(died())));
        assert!(app.state.ws(0).tree().contains(pane_id));
        assert!(
            app.admit_event(
                RuntimeEvent::PaneDied {
                    ending: PaneEnding::new(PaneEndReason::Signalled),
                    ended_at: std::time::Instant::now(),
                }
                .enveloped(pane_id, replacement_generation)
            )
            .is_some()
        );
    }

    #[test]
    fn admission_binds_a_runtime_payload_to_its_pane_and_passes_the_git_answer() {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("admission");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let generation = runtime.generation();
        app.insert_test_runtime(pane_id, runtime);
        let Some(Admitted::Pane(admitted)) = app.admit_event(
            RuntimeEvent::ClipboardWrite {
                content: Vec::new(),
            }
            .enveloped(pane_id, generation),
        ) else {
            panic!("a current runtime's payload is admitted as a pane event");
        };
        assert_eq!(admitted.pane_id, pane_id);
        assert!(matches!(
            admitted.event,
            RuntimeEvent::ClipboardWrite { .. }
        ));
        assert!(matches!(
            app.admit_event(AppEvent::GitStatusRefreshed {
                outcome: shepr_git::RefreshOutcome::empty(),
            }),
            Some(Admitted::GitRefreshed(_))
        ));
    }

    #[test]
    fn an_admitted_death_is_separated_from_other_payloads_and_keeps_its_envelope() {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("death");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let generation = runtime.generation();
        app.insert_test_runtime(pane_id, runtime);
        let admit = |app: &App, event: RuntimeEvent| match app
            .admit_event(event.enveloped(pane_id, generation))
        {
            Some(Admitted::Pane(admitted)) => admitted,
            other => panic!("expected an admitted pane event, got {other:?}"),
        };
        let state_changed = admit(
            &app,
            RuntimeEvent::StateChanged {
                agent: Some(Agent::Codex),
                detection: shepr_detect::Detection::new(AgentState::Working, false),
                process_exited: false,
                observed_at: app.clock.now,
            },
        );
        assert!(state_changed.into_death().is_err());
        let death = admit(
            &app,
            RuntimeEvent::PaneDied {
                ending: PaneEnding::new(PaneEndReason::Exited),
                ended_at: app.clock.now,
            },
        )
        .into_death()
        .expect("a death");
        // Replayed unprepared, a death removes nothing.
        assert!(!app.handle_internal_event_with_view_change(death.into_envelope()));
        assert!(app.state.ws(0).tree().contains(pane_id));
    }

    #[tokio::test]
    async fn a_resume_runtime_replaces_the_panes_runtime_under_the_same_key() {
        let _env = IsolatedEnv::new();
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("resume-replace");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let first = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let first_generation = first.generation();
        app.insert_test_runtime(pane_id, first);
        let replacement = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let replacement_generation = replacement.generation();
        assert_ne!(first_generation, replacement_generation);

        app.install_runtime(pane_id, replacement);

        assert_eq!(
            app.test_runtime(pane_id).generation(),
            replacement_generation
        );
        assert_eq!(
            app.terminal_runtimes.values().count(),
            1,
            "the resume's runtime replaced the pane's runtime, not joined it"
        );
        assert!(!app.runtime_generation_is_current(pane_id, first_generation));
        assert!(app.runtime_generation_is_current(pane_id, replacement_generation));
    }

    #[tokio::test]
    async fn stale_runtime_cannot_forward_clipboard_or_update_detector_and_cwd() {
        let _env = IsolatedEnv::new();
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let generation = runtime.generation();
        app.insert_test_runtime(pane_id, runtime);
        let stale = shepr_mux::events::RuntimeGeneration::alloc();
        for event in [
            RuntimeEvent::ClipboardWrite {
                content: b"stale".to_vec(),
            },
            RuntimeEvent::AgentProcessDetected {
                agent: Agent::Codex,
                observed_at: app.clock.now,
            },
            RuntimeEvent::StateChanged {
                agent: Some(Agent::Codex),
                detection: shepr_detect::Detection::new(AgentState::Working, false),
                process_exited: false,
                observed_at: app.clock.now,
            },
            RuntimeEvent::TerminalCwdReported {
                cwd: shepr_mux::UsableCwd::new("/".into()).expect("root"),
            },
        ] {
            assert!(app.admit_event(event.enveloped(pane_id, stale)).is_none());
        }
        assert!(
            app.admit_event(
                RuntimeEvent::ClipboardWrite {
                    content: Vec::new()
                }
                .enveloped(pane_id, generation)
            )
            .is_some()
        );
    }
}
