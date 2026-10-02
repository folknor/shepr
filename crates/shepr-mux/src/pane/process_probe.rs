pub(super) use crate::limits::AGENT_MISS_CONFIRMATION_ATTEMPTS;
use crate::limits::{
    PROCESS_ACQUISITION_FAST_RECHECK, PROCESS_ACQUISITION_FAST_WINDOW,
    PROCESS_ACQUISITION_IDLE_RESET, PROCESS_ACQUISITION_SLOW_RECHECK, PROCESS_ACQUISITION_WINDOW,
    PROCESS_RECHECK_ACTIVE_AGENT, PROCESS_RECHECK_IDENTIFIED,
    PROCESS_RECHECK_MISSING_FOREGROUND_GROUP, PROCESS_RECHECK_NO_AGENT, PROCESS_RECHECK_TRANSIENT,
    TRANSIENT_COLOR_RECHECK_WINDOW,
};
use tracing::warn;

use super::agent_detection::{
    AGENT_ABSENCE_STARTUP_HOLD, AGENT_PENDING_IDLE_RECHECK, AGENT_STARTUP_GRACE_WINDOW,
    DetectionPublishDecision, DetectionScreenReadDecision, DetectionScreenReadInput,
    PendingIdleConfirmation, ScreenDetectionPublishInput, decide_detection_screen_read,
    decide_screen_detection_publish, detection_update_for_publish_with_osc, withhold_agent_absence,
};
use super::launch::LaunchPurpose;
use super::terminal::PaneTerminal;
use crate::UsableCwd;
use crate::events::AppEvent;
use shepr_agent::detect::{Agent, AgentDetection, AgentState};
use shepr_core::layout::PaneId;

#[derive(Debug, Clone, Copy)]
pub(super) struct StateChangedUpdate {
    pub(super) agent: Option<Agent>,
    pub(super) state: AgentState,
    pub(super) visible_blocker: bool,
    // This observes agent absence, not the pane child's exit reason. The app
    // applies it under a live child; after child death the watcher decides
    // whether the resume identity belongs in the pane-exit checkpoint.
    pub(super) process_exited: bool,
    pub(super) observed_at: std::time::Instant,
}

pub(super) async fn publish_state_changed_event(
    state_events: impl Into<crate::events::EventSender>,
    pane_id: PaneId,
    update: StateChangedUpdate,
) {
    // This runs on the async detector task, not the PTY reader thread.
    // Waiting for queue space here preserves correctness-critical state transitions
    // without blocking pane I/O.
    if let Err(e) = state_events
        .into()
        .send(AppEvent::StateChanged {
            pane_id,
            agent: update.agent,
            state: update.state,
            visible_blocker: update.visible_blocker,
            process_exited: update.process_exited,
            observed_at: update.observed_at,
        })
        .await
    {
        warn!(
            pane = pane_id.raw(),
            error = %e,
            "failed to deliver StateChanged event"
        );
    }
}

pub(super) async fn publish_agent_process_detected_event(
    state_events: impl Into<crate::events::EventSender>,
    pane_id: PaneId,
    agent: Agent,
    observed_at: std::time::Instant,
) {
    if let Err(e) = state_events
        .into()
        .send(AppEvent::AgentProcessDetected {
            pane_id,
            agent,
            observed_at,
        })
        .await
    {
        warn!(
            pane = pane_id.raw(),
            error = %e,
            "failed to deliver AgentProcessDetected event"
        );
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AgentDetectionPresence {
    current_agent: Option<Agent>,
    consecutive_misses: u8,
}

pub(super) fn absolute_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    shepr_agent::detect::process_cwd(pid).filter(|cwd| cwd.is_absolute())
}

/// A process's cwd as the event loop may read it: one readlink of
/// `/proc/<pid>/cwd`, which never touches the directory's filesystem, so a
/// hung mount cannot stall the loop. A directory that was removed reads back
/// with the kernel's ` (deleted)` suffix and is not a cwd anyone can use. Other
/// unusable paths are left to whoever launches in them (the launch falls back
/// by chdir).
pub(super) fn readlink_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    absolute_process_cwd(pid).filter(|cwd| !crate::workspace::process_cwd_is_deleted(cwd))
}

pub(super) fn usable_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    absolute_process_cwd(pid)
        .and_then(UsableCwd::new)
        .map(UsableCwd::into_path_buf)
}

pub(super) fn foreground_member_cwd_different_from_shell(
    shell_pid: u32,
    shell_cwd: Option<&std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let job = shepr_agent::detect::foreground_job(shell_pid)?;
    for process in job.processes {
        if process.pid == shell_pid {
            continue;
        }
        let Some(cwd) = readlink_process_cwd(process.pid) else {
            continue;
        };
        if shell_cwd != Some(&cwd) {
            return Some(cwd);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ForegroundShellAgentAction {
    ObserveProbe,
    ReportProcessExit,
    ReportReplacementProcess,
    ClearAgent,
    /// A stopped descendant still owns the agent identity and session.
    Suspended,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ForegroundShellProbe {
    pub(super) previous_agent: Option<Agent>,
    pub(super) identified_agent: Option<Agent>,
    pub(super) foreground_is_pane_shell: bool,
    pub(super) process_exit_reported: bool,
}

fn foreground_shell_agent_action_with_suspended_agent(
    probe: ForegroundShellProbe,
    suspended_agent_is_present: bool,
) -> ForegroundShellAgentAction {
    let Some(previous_agent) = probe.previous_agent else {
        return ForegroundShellAgentAction::ObserveProbe;
    };
    if probe.foreground_is_pane_shell && suspended_agent_is_present {
        return ForegroundShellAgentAction::Suspended;
    }
    if probe.process_exit_reported {
        return if probe.identified_agent == Some(previous_agent) {
            ForegroundShellAgentAction::ReportReplacementProcess
        } else if probe.identified_agent.is_none() {
            ForegroundShellAgentAction::ClearAgent
        } else {
            ForegroundShellAgentAction::ObserveProbe
        };
    }
    if probe.identified_agent.is_some() {
        return ForegroundShellAgentAction::ObserveProbe;
    }

    if probe.foreground_is_pane_shell {
        // Do not clear identity immediately. First publish an idle process-exit
        // transition for the previous agent so state observers see completion
        // before the pane becomes unknown.
        return ForegroundShellAgentAction::ReportProcessExit;
    }

    ForegroundShellAgentAction::ObserveProbe
}

/// Drops retained OSC evidence after the detector confirms a transition away
/// from an identified agent.
pub(super) fn clear_osc_evidence_for_agent_transition(terminal: &PaneTerminal) {
    terminal.clear_agent_osc_state();
}

pub(super) fn foreground_group_changed(
    foreground_pgid: Option<u32>,
    last_foreground_pgid: Option<u32>,
) -> bool {
    foreground_pgid != last_foreground_pgid
        && (foreground_pgid.is_some() || last_foreground_pgid.is_some())
}

// Only kernel-observed foreground groups drive change detection. Remembering an
// inferred group would look like a change on every tick while the kernel stays silent.
pub(super) fn process_group_for_change_tracking(
    observed_foreground_pgid: Option<u32>,
    probed_process_group_id: Option<u32>,
) -> Option<u32> {
    observed_foreground_pgid?;
    probed_process_group_id.or(observed_foreground_pgid)
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ProcessProbeRequest {
    pub(super) now: std::time::Instant,
    pub(super) observed_foreground_group: Option<u32>,
    pub(super) lifecycle_authority_active: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ProcessProbeScheduleInput {
    now: std::time::Instant,
    agent: Option<Agent>,
    observed_foreground_group: Option<u32>,
    lifecycle_authority_active: bool,
    shell_clear_pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeScheduleDecision {
    Skip {
        foreground_group_changed: bool,
    },
    Probe {
        foreground_group_changed: bool,
        had_previous_probe: bool,
    },
}

impl ProbeScheduleDecision {
    pub(super) fn should_probe(self) -> bool {
        matches!(self, Self::Probe { .. })
    }

    pub(super) fn foreground_group_changed(self) -> bool {
        match self {
            Self::Skip {
                foreground_group_changed,
            }
            | Self::Probe {
                foreground_group_changed,
                ..
            } => foreground_group_changed,
        }
    }
}

/// Owns when process probes run and how foreground/content activity opens an
/// acquisition window. Callers provide only the observations for this tick.
#[derive(Debug)]
pub(super) struct ProcessProbeScheduler {
    last_check: std::time::Instant,
    last_foreground_group: Option<u32>,
    has_probe: bool,
    acquisition_started_at: Option<std::time::Instant>,
    last_content_change_at: Option<std::time::Instant>,
}

impl ProcessProbeScheduler {
    pub(super) fn new(now: std::time::Instant) -> Self {
        Self {
            last_check: now,
            last_foreground_group: None,
            has_probe: false,
            acquisition_started_at: None,
            last_content_change_at: None,
        }
    }

    pub(super) fn reset(&mut self) {
        self.last_foreground_group = None;
        self.has_probe = false;
        self.acquisition_started_at = None;
        self.last_content_change_at = None;
    }

    pub(super) fn foreground_group_changed(&self, observed: Option<u32>) -> bool {
        foreground_group_changed(observed, self.last_foreground_group)
    }

    pub(super) fn schedule(&self, input: ProcessProbeScheduleInput) -> ProbeScheduleDecision {
        let group_changed = self.foreground_group_changed(input.observed_foreground_group);
        let elapsed_since_check = input.now.duration_since(self.last_check);
        let acquisition_age = self
            .acquisition_started_at
            .map(|started| input.now.duration_since(started));

        let acquisition_due = acquisition_age.is_some_and(|acquisition_age| {
            let acquisition_interval = if acquisition_age <= PROCESS_ACQUISITION_FAST_WINDOW {
                PROCESS_ACQUISITION_FAST_RECHECK
            } else {
                PROCESS_ACQUISITION_SLOW_RECHECK
            };
            acquisition_age <= PROCESS_ACQUISITION_WINDOW
                && elapsed_since_check >= acquisition_interval
        });

        if !self.has_probe
            && !input.shell_clear_pending
            && !group_changed
            && !acquisition_due
            && acquisition_age.is_some_and(|age| age <= PROCESS_ACQUISITION_WINDOW)
        {
            return ProbeScheduleDecision::Skip {
                foreground_group_changed: group_changed,
            };
        }

        // Hook authority decides state arbitration, not process liveness. Keep
        // the safety cadence while an agent is identified or being reacquired.
        let should_probe = if input.shell_clear_pending || acquisition_due {
            true
        } else if input.agent.is_none() {
            !self.has_probe
                || group_changed
                || (input.lifecycle_authority_active
                    && elapsed_since_check >= PROCESS_RECHECK_IDENTIFIED)
                || (input.observed_foreground_group.is_none()
                    && elapsed_since_check >= PROCESS_RECHECK_MISSING_FOREGROUND_GROUP)
        } else {
            group_changed || elapsed_since_check >= PROCESS_RECHECK_IDENTIFIED
        };

        if should_probe {
            ProbeScheduleDecision::Probe {
                foreground_group_changed: group_changed,
                had_previous_probe: self.has_probe,
            }
        } else {
            ProbeScheduleDecision::Skip {
                foreground_group_changed: group_changed,
            }
        }
    }

    fn probe_started(&mut self, now: std::time::Instant) {
        self.last_check = now;
        self.has_probe = true;
    }

    fn probe_completed(&mut self, completion: ProcessProbeCompletion) {
        self.last_foreground_group = process_group_for_change_tracking(
            completion.observed_foreground_group,
            completion.probed_process_group,
        );
        if completion.identified_agent.is_some() {
            self.acquisition_started_at = None;
            self.last_content_change_at = None;
        } else if completion.current_agent.is_none()
            && completion.had_previous_probe
            && completion.foreground_group_changed
        {
            self.acquisition_started_at = Some(completion.now);
        }
    }

    pub(super) fn content_changed(
        &mut self,
        now: std::time::Instant,
        agent: Option<Agent>,
        group_changed: bool,
        changed: bool,
    ) {
        if agent.is_some() || group_changed {
            return;
        }

        if changed {
            let should_start = self.acquisition_started_at.is_none_or(|started| {
                now.duration_since(started) > PROCESS_ACQUISITION_WINDOW
                    && self.last_content_change_at.is_none_or(|last_change| {
                        now.duration_since(last_change) >= PROCESS_ACQUISITION_IDLE_RESET
                    })
            });
            if should_start {
                self.acquisition_started_at = Some(now);
            }
            self.last_content_change_at = Some(now);
            return;
        }

        let Some(acquisition_started) = self.acquisition_started_at else {
            return;
        };
        let Some(last_content_change) = self.last_content_change_at else {
            return;
        };

        if now.duration_since(acquisition_started) > PROCESS_ACQUISITION_WINDOW
            && now.duration_since(last_content_change) >= PROCESS_ACQUISITION_IDLE_RESET
        {
            self.acquisition_started_at = None;
            self.last_content_change_at = None;
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ProcessProbeResult {
    process_group_id: Option<u32>,
    foreground_is_pane_shell: bool,
    suspended_agents: Vec<Agent>,
    identity: ProcessProbeIdentity,
}

#[derive(Debug, Clone)]
enum ProcessProbeIdentity {
    Agent { agent: Agent, process_name: String },
    Unidentified,
}

impl ProcessProbeResult {
    pub(super) fn process_group_id(&self) -> Option<u32> {
        self.process_group_id
    }

    pub(super) fn foreground_is_pane_shell(&self) -> bool {
        self.foreground_is_pane_shell
    }

    pub(super) fn agent(&self) -> Option<Agent> {
        match &self.identity {
            ProcessProbeIdentity::Agent { agent, .. } => Some(*agent),
            ProcessProbeIdentity::Unidentified => None,
        }
    }

    pub(super) fn process_name(&self) -> Option<&str> {
        match &self.identity {
            ProcessProbeIdentity::Agent { process_name, .. } => Some(process_name),
            ProcessProbeIdentity::Unidentified => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AgentDetectionPublishUpdate {
    pub(super) state: AgentState,
    pub(super) visible_idle: bool,
    pub(super) visible_blocker: bool,
    pub(super) visible_working: bool,
    pub(super) process_exited: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScreenScanGate {
    pub(super) now: std::time::Instant,
    pub(super) lifecycle_authority_active: bool,
    pub(super) process_exited: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScreenReadRequest {
    pub(super) agent: Option<Agent>,
    pub(super) agent_changed: bool,
    pub(super) process_exited: bool,
    pub(super) detection_content_seq: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScreenPublishContext {
    pub(super) now: std::time::Instant,
    pub(super) process_exited: bool,
    pub(super) agent_changed: bool,
}

#[derive(Debug, Clone, Copy)]
struct ProcessProbeCompletion {
    now: std::time::Instant,
    observed_foreground_group: Option<u32>,
    probed_process_group: Option<u32>,
    identified_agent: Option<Agent>,
    current_agent: Option<Agent>,
    foreground_group_changed: bool,
    had_previous_probe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentProcessChange {
    pub(super) previous_agent: Option<Agent>,
    pub(super) agent: Option<Agent>,
    pub(super) process_name: Option<String>,
    pub(super) process_group_id: Option<u32>,
    pub(super) agent_changed: bool,
    pub(super) should_clear_osc_evidence: bool,
    pub(super) process_detected: Option<Agent>,
}

/// A tick may ask for I/O and be resumed with its result. Only Begin observes
/// scheduling and only Probe observes identity. Every step then runs the same
/// screen path, whose gates are idempotent for one tick's observations, so a
/// Screen resume reaches the cache miss that asked for it. The runtime passes
/// one set of observations (time, group, content sequence) through a tick.
pub(super) enum TickObservation {
    Begin,
    Probe(ProcessProbeResult),
    Screen(super::terminal::AgentDetectionInputs),
}

pub(super) struct DetectorObservations {
    pub(super) now: std::time::Instant,
    pub(super) foreground_group: Option<u32>,
    pub(super) content_seq: u64,
    pub(super) lifecycle_authority_active: bool,
    pub(super) theme_restore_candidate: bool,
    pub(super) observation: TickObservation,
}

pub(super) struct TickOutput {
    pub(super) probe: bool,
    pub(super) screen: bool,
    pub(super) process_change: Option<AgentProcessChange>,
    pub(super) state_changed: Option<StateChangedUpdate>,
    pub(super) next_wake: std::time::Duration,
}

/// The detector's mutable state, independent of the PTY runtime and terminal.
/// Its transitions can be exercised with fake times and process observations.
pub(super) struct DetectorState {
    tick_schedule: Option<ProbeScheduleDecision>,
    tick_agent_changed: bool,
    tick_group_changed: bool,
    agent_presence: AgentDetectionPresence,
    state: AgentState,
    last_visible_idle: bool,
    last_visible_blocker: bool,
    last_visible_working: bool,
    last_visible_signal_refresh: Option<std::time::Instant>,
    scheduler: ProcessProbeScheduler,
    transient_color_recheck_until: Option<std::time::Instant>,
    pending_foreground_shell_clear: bool,
    foreground_shell_exit_reported: bool,
    // Keep the confirmed-missing identity available for the required exit
    // report after the presence counter has already cleared it.
    pending_confirmed_process_exit: Option<Agent>,
    last_screen_scan_detection_content_seq: Option<u64>,
    last_screen_detection: Option<ScreenDetectionCacheEntry>,
    has_detection_baseline: bool,
    agent_startup_grace_until: Option<std::time::Instant>,
    pending_idle: PendingIdleConfirmation,
    agent_absence_hold_until: Option<std::time::Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenDetectionCacheEntry {
    agent: Option<Agent>,
    process_exited: bool,
    detection_content_seq: u64,
    result: Option<AgentDetection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScreenDetectionCacheLookup {
    Miss,
    Hit(Option<AgentDetection>),
}

impl DetectorState {
    pub(super) fn new(now: std::time::Instant, purpose: LaunchPurpose) -> Self {
        let agent_absence_hold_until = match purpose {
            LaunchPurpose::Fresh => None,
            LaunchPurpose::AgentResume => now.checked_add(AGENT_ABSENCE_STARTUP_HOLD),
        };
        Self {
            tick_schedule: None,
            tick_agent_changed: false,
            tick_group_changed: false,
            agent_presence: AgentDetectionPresence::from_agent(None),
            // Keep the pre-detection state distinct from Unknown so the first
            // absent-agent report can withdraw a restored pane's seeded agent.
            state: AgentState::Idle,
            last_visible_idle: false,
            last_visible_blocker: false,
            last_visible_working: false,
            last_visible_signal_refresh: None,
            scheduler: ProcessProbeScheduler::new(now),
            transient_color_recheck_until: None,
            pending_foreground_shell_clear: false,
            foreground_shell_exit_reported: false,
            pending_confirmed_process_exit: None,
            last_screen_scan_detection_content_seq: None,
            last_screen_detection: None,
            has_detection_baseline: false,
            agent_startup_grace_until: None,
            pending_idle: PendingIdleConfirmation::default(),
            agent_absence_hold_until,
        }
    }

    /// All scheduling, identity, screen-cache and publication transitions run
    /// here. The runtime supplies completed observations and executes requests;
    /// it does not decide which transition wins or mutate publication state.
    pub(super) fn tick(&mut self, input: &DetectorObservations) -> TickOutput {
        let mut output = TickOutput {
            probe: false,
            screen: false,
            process_change: None,
            state_changed: None,
            next_wake: std::time::Duration::ZERO,
        };
        match &input.observation {
            TickObservation::Begin => {
                self.tick_agent_changed = false;
                let schedule = self.schedule_process_probe(&ProcessProbeRequest {
                    now: input.now,
                    observed_foreground_group: input.foreground_group,
                    lifecycle_authority_active: input.lifecycle_authority_active,
                });
                self.tick_group_changed = schedule.foreground_group_changed();
                if self.tick_group_changed {
                    self.note_foreground_group_change(input.now, input.theme_restore_candidate);
                }
                self.tick_schedule = Some(schedule);
                if schedule.should_probe() {
                    self.probe_started(input.now);
                    output.probe = true;
                } else {
                    self.finish_screen_tick(input, None, &mut output);
                }
            }
            TickObservation::Probe(probe) => {
                if let Some(schedule) = self.tick_schedule.take() {
                    let change = self.observe_process_probe(
                        probe,
                        input.now,
                        input.foreground_group,
                        schedule,
                    );
                    self.tick_agent_changed = change.agent_changed;
                    output.process_change = Some(change);
                }
                self.finish_screen_tick(input, None, &mut output);
            }
            TickObservation::Screen(screen) => {
                self.finish_screen_tick(input, Some(screen), &mut output);
            }
        }
        output.next_wake = self.tick_interval(input.now, input.theme_restore_candidate);
        output
    }

    fn finish_screen_tick(
        &mut self,
        input: &DetectorObservations,
        screen: Option<&super::terminal::AgentDetectionInputs>,
        output: &mut TickOutput,
    ) {
        let agent = self.current_agent();
        let process_exited = self.process_exited(agent);
        if !self.may_scan_screen(ScreenScanGate {
            now: input.now,
            lifecycle_authority_active: input.lifecycle_authority_active,
            process_exited,
        }) || !self.should_read_screen(ScreenReadRequest {
            agent,
            agent_changed: self.tick_agent_changed,
            process_exited,
            detection_content_seq: input.content_seq,
        }) {
            return;
        }
        let (detection, changed) =
            match self.cached_screen_detection(agent, process_exited, input.content_seq) {
                ScreenDetectionCacheLookup::Hit(result) => (result, false),
                ScreenDetectionCacheLookup::Miss => {
                    if agent.is_some() && screen.is_none() {
                        output.screen = true;
                        return;
                    }
                    let changed =
                        self.observe_screen_sequence(input.content_seq) && agent.is_none();
                    let content = screen.map_or("", |screen| screen.screen_text.as_str());
                    let title = screen.map_or("", |screen| screen.osc_title.as_str());
                    let progress = screen.map_or("", |screen| screen.osc_progress.as_str());
                    let detection = detection_update_for_publish_with_osc(
                        agent,
                        content,
                        title,
                        progress,
                        process_exited,
                    );
                    self.remember_screen_detection(
                        agent,
                        process_exited,
                        input.content_seq,
                        detection,
                    );
                    (detection, changed)
                }
            };
        let Some(detection) = detection else {
            self.clear_pending_idle();
            return;
        };
        self.note_content_change(input.now, self.tick_group_changed, changed);
        if self.withhold_agent_absence(agent, input.now) {
            self.clear_pending_idle();
            return;
        }
        if let DetectionPublishDecision::Publish {
            state,
            visible_idle,
            visible_blocker,
            visible_working,
            process_exited,
        } = self.screen_publish_decision(
            detection,
            ScreenPublishContext {
                now: input.now,
                process_exited,
                agent_changed: self.tick_agent_changed,
            },
        ) {
            output.state_changed = Some(self.apply_publish_update(
                agent,
                AgentDetectionPublishUpdate {
                    state,
                    visible_idle,
                    visible_blocker,
                    visible_working,
                    process_exited,
                },
                input.now,
            ));
        }
    }

    fn current_agent(&self) -> Option<Agent> {
        // The server must receive the exit against the identity that just
        // passed miss confirmation, before the detector withdraws it.
        self.pending_confirmed_process_exit
            .or_else(|| self.agent_presence.current_agent())
    }

    fn tick_interval(
        &mut self,
        now: std::time::Instant,
        theme_restore_candidate: bool,
    ) -> std::time::Duration {
        if !theme_restore_candidate {
            self.transient_color_recheck_until = None;
        }
        if theme_restore_candidate
            && self
                .transient_color_recheck_until
                .is_some_and(|until| now < until)
        {
            PROCESS_RECHECK_TRANSIENT
        } else if self.pending_idle.active() {
            AGENT_PENDING_IDLE_RECHECK
        } else if self.current_agent().is_none() {
            PROCESS_RECHECK_NO_AGENT
        } else {
            PROCESS_RECHECK_ACTIVE_AGENT
        }
    }

    fn note_foreground_group_change(
        &mut self,
        now: std::time::Instant,
        theme_restore_candidate: bool,
    ) {
        self.transient_color_recheck_until = theme_restore_candidate
            .then(|| now.checked_add(TRANSIENT_COLOR_RECHECK_WINDOW))
            .flatten();
    }

    pub(super) fn reset(&mut self) {
        // Lifecycle authority resets screen evidence, not process evidence.
        // Reset runs when full-lifecycle hook authority becomes active, which
        // says nothing about whether the agent process is present: presence
        // with its miss count, a confirmed exit still to report, and whether
        // that exit was already reported all stay. Clearing them let the next
        // probe of a shell foreground report the same disappearance again.
        // The scheduler is reset only so the process is rechecked promptly.
        // This makes reset itself replay-free, not every exit delivery
        // idempotent: the suspended-presence path in `observe_process_probe`
        // clears the exit bookkeeping without publishing a presence event, so
        // the same agent can be reported gone again later.
        self.state = AgentState::Unknown;
        self.last_visible_idle = false;
        self.scheduler.reset();
        self.transient_color_recheck_until = None;
        self.last_visible_blocker = false;
        self.last_visible_working = false;
        self.last_visible_signal_refresh = None;
        self.last_screen_scan_detection_content_seq = None;
        self.last_screen_detection = None;
        // Reset establishes Unknown as a real baseline; new() starts from a
        // placeholder and must keep reading until its first report publishes.
        self.has_detection_baseline = true;
        self.agent_startup_grace_until = None;
        self.pending_idle.clear();
    }

    fn schedule_process_probe(&self, request: &ProcessProbeRequest) -> ProbeScheduleDecision {
        self.scheduler.schedule(ProcessProbeScheduleInput {
            now: request.now,
            agent: self.current_agent(),
            observed_foreground_group: request.observed_foreground_group,
            lifecycle_authority_active: request.lifecycle_authority_active,
            shell_clear_pending: self.pending_foreground_shell_clear,
        })
    }

    fn probe_started(&mut self, now: std::time::Instant) {
        self.scheduler.probe_started(now);
    }

    fn observe_process_probe(
        &mut self,
        probe: &ProcessProbeResult,
        now: std::time::Instant,
        observed_foreground_group: Option<u32>,
        schedule: ProbeScheduleDecision,
    ) -> AgentProcessChange {
        let process_name = probe.process_name().map(str::to_owned);
        let process_group_id = probe.process_group_id();
        let foreground_is_pane_shell = probe.foreground_is_pane_shell();
        let identified_agent = probe.agent();

        let previous_agent = self.current_agent();
        let shell_probe = ForegroundShellProbe {
            previous_agent,
            identified_agent,
            foreground_is_pane_shell,
            process_exit_reported: self.foreground_shell_exit_reported,
        };
        let suspended_agent_is_present = foreground_is_pane_shell
            && previous_agent.is_some_and(|agent| probe.suspended_agents.contains(&agent));
        let action = foreground_shell_agent_action_with_suspended_agent(
            shell_probe,
            suspended_agent_is_present,
        );
        let agent_changed = match action {
            ForegroundShellAgentAction::ReportReplacementProcess => {
                self.pending_foreground_shell_clear = false;
                self.foreground_shell_exit_reported = false;
                self.pending_confirmed_process_exit = None;
                self.agent_presence.observe_process_probe(previous_agent);
                true
            }
            ForegroundShellAgentAction::ReportProcessExit => {
                self.pending_foreground_shell_clear = true;
                false
            }
            ForegroundShellAgentAction::ClearAgent => {
                self.pending_foreground_shell_clear = false;
                self.foreground_shell_exit_reported = false;
                let had_confirmed_exit = self.pending_confirmed_process_exit.take().is_some();
                self.agent_presence.clear_current_agent() || had_confirmed_exit
            }
            ForegroundShellAgentAction::ObserveProbe => {
                self.pending_foreground_shell_clear = false;
                self.foreground_shell_exit_reported = false;
                let changed = self.agent_presence.observe_process_probe(identified_agent);
                if changed && identified_agent.is_none() {
                    self.pending_confirmed_process_exit = previous_agent;
                    self.pending_foreground_shell_clear = previous_agent.is_some();
                    false
                } else {
                    if identified_agent.is_some() {
                        self.pending_confirmed_process_exit = None;
                    }
                    changed
                }
            }
            ForegroundShellAgentAction::Suspended => {
                self.pending_foreground_shell_clear = false;
                self.foreground_shell_exit_reported = false;
                self.pending_confirmed_process_exit = None;
                if let Some(agent) = previous_agent {
                    self.agent_presence.observe_process_probe(Some(agent));
                }
                false
            }
        };
        let agent = self.current_agent();
        self.scheduler.probe_completed(ProcessProbeCompletion {
            now,
            observed_foreground_group,
            probed_process_group: process_group_id,
            identified_agent,
            current_agent: agent,
            foreground_group_changed: schedule.foreground_group_changed(),
            had_previous_probe: matches!(
                schedule,
                ProbeScheduleDecision::Probe {
                    had_previous_probe: true,
                    ..
                }
            ),
        });

        let replacement = action == ForegroundShellAgentAction::ReportReplacementProcess;
        let should_reset_detection = agent_changed && (agent != previous_agent || replacement);
        if should_reset_detection {
            self.pending_idle.clear();
            self.last_screen_scan_detection_content_seq = None;
            self.last_screen_detection = None;
            if agent.is_some() {
                self.agent_absence_hold_until = None;
                self.agent_startup_grace_until = Some(now + AGENT_STARTUP_GRACE_WINDOW);
                self.state = AgentState::Unknown;
                self.last_visible_idle = false;
                self.last_visible_blocker = false;
                self.last_visible_working = false;
                self.last_visible_signal_refresh = None;
            } else {
                self.agent_startup_grace_until = None;
            }
        }

        AgentProcessChange {
            previous_agent,
            agent,
            process_name,
            process_group_id,
            agent_changed,
            should_clear_osc_evidence: should_reset_detection && previous_agent.is_some(),
            process_detected: if should_reset_detection { agent } else { None },
        }
    }

    fn clear_pending_idle(&mut self) {
        self.pending_idle.clear();
    }

    fn process_exited(&self, agent: Option<Agent>) -> bool {
        self.pending_foreground_shell_clear
            && agent.is_some()
            && !self.foreground_shell_exit_reported
    }

    fn may_scan_screen(&mut self, gate: ScreenScanGate) -> bool {
        if gate.lifecycle_authority_active && !gate.process_exited {
            self.pending_idle.clear();
            return false;
        }
        if let Some(until) = self.agent_startup_grace_until {
            if gate.process_exited {
                self.agent_startup_grace_until = None;
                self.last_screen_scan_detection_content_seq = None;
                self.pending_idle.clear();
            } else if gate.now < until {
                self.pending_idle.clear();
                return false;
            } else {
                self.agent_startup_grace_until = None;
                self.pending_idle.clear();
            }
        }
        true
    }

    fn should_read_screen(&self, request: ScreenReadRequest) -> bool {
        // The initial Idle value is only a transition sentinel. Do not let it
        // suppress the first screen report, especially while a restore hold is
        // waiting to expire.
        if !self.has_detection_baseline {
            return true;
        }
        matches!(
            decide_detection_screen_read(DetectionScreenReadInput {
                state: self.state,
                agent: request.agent,
                pending_idle_active: self.pending_idle.active(),
                agent_changed: request.agent_changed,
                process_exited: request.process_exited,
                current_detection_content_seq: request.detection_content_seq,
                last_screen_scan_detection_content_seq: self.last_screen_scan_detection_content_seq,
            }),
            DetectionScreenReadDecision::Read
        )
    }

    fn observe_screen_sequence(&mut self, sequence: u64) -> bool {
        let changed = self.last_screen_scan_detection_content_seq != Some(sequence);
        self.last_screen_scan_detection_content_seq = Some(sequence);
        changed
    }

    fn cached_screen_detection(
        &self,
        agent: Option<Agent>,
        process_exited: bool,
        detection_content_seq: u64,
    ) -> ScreenDetectionCacheLookup {
        match self.last_screen_detection {
            Some(entry)
                if entry.agent == agent
                    && entry.process_exited == process_exited
                    && entry.detection_content_seq == detection_content_seq =>
            {
                ScreenDetectionCacheLookup::Hit(entry.result)
            }
            _ => ScreenDetectionCacheLookup::Miss,
        }
    }

    fn remember_screen_detection(
        &mut self,
        agent: Option<Agent>,
        process_exited: bool,
        detection_content_seq: u64,
        result: Option<AgentDetection>,
    ) {
        self.last_screen_detection = Some(ScreenDetectionCacheEntry {
            agent,
            process_exited,
            detection_content_seq,
            result,
        });
    }

    fn note_content_change(&mut self, now: std::time::Instant, group_changed: bool, changed: bool) {
        self.scheduler
            .content_changed(now, self.current_agent(), group_changed, changed);
    }

    fn withhold_agent_absence(&mut self, agent: Option<Agent>, now: std::time::Instant) -> bool {
        withhold_agent_absence(agent, &mut self.agent_absence_hold_until, now)
    }

    fn screen_publish_decision(
        &mut self,
        screen_detection: shepr_agent::detect::AgentDetection,
        context: ScreenPublishContext,
    ) -> DetectionPublishDecision {
        decide_screen_detection_publish(
            ScreenDetectionPublishInput {
                screen_detection,
                current_state: self.state,
                last_visible_idle: self.last_visible_idle,
                last_visible_blocker: self.last_visible_blocker,
                last_visible_working: self.last_visible_working,
                last_visible_signal_refresh: self.last_visible_signal_refresh,
                process_exited: context.process_exited,
                agent_changed: context.agent_changed,
                now: context.now,
            },
            &mut self.pending_idle,
        )
    }

    fn apply_publish_update(
        &mut self,
        agent: Option<Agent>,
        update: AgentDetectionPublishUpdate,
        observed_at: std::time::Instant,
    ) -> StateChangedUpdate {
        self.state = update.state;
        self.has_detection_baseline = true;
        self.last_visible_idle = update.visible_idle;
        self.last_visible_blocker = update.visible_blocker;
        self.last_visible_working = update.visible_working;
        self.last_visible_signal_refresh = if update.visible_blocker || update.visible_working {
            Some(observed_at)
        } else {
            None
        };
        if update.process_exited {
            self.foreground_shell_exit_reported = true;
        }
        StateChangedUpdate {
            agent,
            state: update.state,
            visible_blocker: update.visible_blocker,
            process_exited: update.process_exited,
            observed_at,
        }
    }
}

fn process_probe_result(
    job: &shepr_agent::detect::ForegroundJob,
    pid: u32,
    agent: Agent,
    process_name: String,
) -> ProcessProbeResult {
    ProcessProbeResult {
        process_group_id: Some(job.process_group_id),
        foreground_is_pane_shell: job.processes.iter().any(|process| process.pid == pid),
        suspended_agents: Vec::new(),
        identity: ProcessProbeIdentity::Agent {
            agent,
            process_name,
        },
    }
}

pub(super) fn probe_foreground_process_from_jobs(
    pid: u32,
    foreground_pgid: Option<u32>,
    leader_job: Option<&shepr_agent::detect::ForegroundJob>,
    foreground_job: impl FnOnce() -> Option<shepr_agent::detect::ForegroundJob>,
) -> ProcessProbeResult {
    if let Some(job) = leader_job
        && let Some((agent, process_name)) = shepr_agent::detect::identify_agent_in_job(job)
    {
        return process_probe_result(job, pid, agent, process_name);
    }

    let foreground_job = foreground_job();
    if let Some(job) = foreground_job.as_ref() {
        let identified = shepr_agent::detect::identify_agent_in_job(job);
        return ProcessProbeResult {
            process_group_id: Some(job.process_group_id),
            foreground_is_pane_shell: job.processes.iter().any(|process| process.pid == pid),
            suspended_agents: Vec::new(),
            identity: identified.map_or(
                ProcessProbeIdentity::Unidentified,
                |(agent, process_name)| ProcessProbeIdentity::Agent {
                    agent,
                    process_name,
                },
            ),
        };
    }

    ProcessProbeResult {
        process_group_id: foreground_pgid,
        foreground_is_pane_shell: false,
        suspended_agents: Vec::new(),
        identity: ProcessProbeIdentity::Unidentified,
    }
}

pub(super) fn probe_foreground_process(
    pid: u32,
    foreground_pgid: Option<u32>,
) -> ProcessProbeResult {
    let mut probe = probe_foreground_process_from_jobs(
        pid,
        foreground_pgid,
        foreground_pgid
            .and_then(shepr_agent::detect::foreground_group_leader_job)
            .as_ref(),
        || shepr_agent::detect::foreground_job(pid),
    );
    if probe.foreground_is_pane_shell() {
        probe.suspended_agents = shepr_agent::detect::suspended_agent_processes(pid);
    }
    probe
}

impl AgentDetectionPresence {
    pub(super) fn from_agent(current_agent: Option<Agent>) -> Self {
        Self {
            current_agent,
            consecutive_misses: 0,
        }
    }

    pub(super) fn current_agent(&self) -> Option<Agent> {
        self.current_agent
    }

    pub(super) fn clear_current_agent(&mut self) -> bool {
        if self.current_agent.is_none() {
            self.consecutive_misses = 0;
            return false;
        }
        self.current_agent = None;
        self.consecutive_misses = 0;
        true
    }

    pub(super) fn observe_process_probe(&mut self, identified_agent: Option<Agent>) -> bool {
        match identified_agent {
            Some(agent) => {
                self.consecutive_misses = 0;
                if Some(agent) == self.current_agent {
                    return false;
                }
                self.current_agent = Some(agent);
                true
            }
            None => {
                if self.current_agent.is_none() {
                    self.consecutive_misses = 0;
                    return false;
                }
                self.consecutive_misses = self.consecutive_misses.saturating_add(1);
                if self.consecutive_misses < AGENT_MISS_CONFIRMATION_ATTEMPTS {
                    return false;
                }
                self.current_agent = None;
                self.consecutive_misses = 0;
                true
            }
        }
    }
}

/// The action for a probe with no suspended agent, as the runtime tests
/// exercise the foreground-shell rules on their own.
#[cfg(test)]
pub(super) fn foreground_shell_agent_action(
    probe: ForegroundShellProbe,
) -> ForegroundShellAgentAction {
    foreground_shell_agent_action_with_suspended_agent(probe, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick_input(now: std::time::Instant, observation: TickObservation) -> DetectorObservations {
        DetectorObservations {
            now,
            foreground_group: Some(25),
            content_seq: 1,
            lifecycle_authority_active: false,
            theme_restore_candidate: false,
            observation,
        }
    }

    fn tick_probe(agent: Option<Agent>) -> TickObservation {
        TickObservation::Probe(ProcessProbeResult {
            process_group_id: Some(25),
            foreground_is_pane_shell: false,
            suspended_agents: Vec::new(),
            identity: agent.map_or(ProcessProbeIdentity::Unidentified, |agent| {
                ProcessProbeIdentity::Agent {
                    agent,
                    process_name: "agent".into(),
                }
            }),
        })
    }

    #[test]
    fn tick_retries_the_initial_absence_report_until_restore_hold_expires() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::AgentResume);
        assert!(
            detector
                .tick(&tick_input(now, TickObservation::Begin))
                .probe
        );
        let held = detector.tick(&tick_input(now, tick_probe(None)));
        assert!(!held.screen);
        assert!(held.state_changed.is_none());
        let deadline = now + AGENT_ABSENCE_STARTUP_HOLD;
        let released = detector.tick(&tick_input(deadline, TickObservation::Begin));
        assert!(!released.probe);
        assert!(!released.screen);
        let update = released
            .state_changed
            .expect("absence publishes without a core read");
        assert_eq!(update.agent, None);
        assert_eq!(update.state, AgentState::Unknown);
    }

    #[test]
    fn tick_acquisition_grace_cache_and_authority_share_one_transition_path() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        assert!(
            detector
                .tick(&tick_input(now, TickObservation::Begin))
                .probe
        );
        let acquired = detector.tick(&tick_input(now, tick_probe(Some(Agent::Claude))));
        assert_eq!(
            acquired.process_change.expect("identity").process_detected,
            Some(Agent::Claude)
        );
        assert!(!acquired.screen);
        assert!(acquired.state_changed.is_none());

        let ready = now + AGENT_STARTUP_GRACE_WINDOW;
        let begin = detector.tick(&tick_input(ready, TickObservation::Begin));
        let ready_output = if begin.probe {
            detector.tick(&tick_input(ready, tick_probe(Some(Agent::Claude))))
        } else {
            begin
        };
        assert!(ready_output.screen);
        let result = detector.tick(&tick_input(
            ready,
            TickObservation::Screen(super::super::terminal::AgentDetectionInputs {
                screen_text: "* Waiting for 1 background agent to finish".into(),
                ..Default::default()
            }),
        ));
        assert_eq!(
            result.state_changed.expect("working report").state,
            AgentState::Working
        );
        let cached = detector.tick(&tick_input(
            ready + std::time::Duration::from_millis(1),
            TickObservation::Begin,
        ));
        assert!(!cached.screen);
        let mut authority = tick_input(
            ready + std::time::Duration::from_millis(2),
            TickObservation::Begin,
        );
        authority.lifecycle_authority_active = true;
        authority.content_seq = 2;
        let authoritative = detector.tick(&authority);
        assert!(!authoritative.screen);
        assert!(authoritative.state_changed.is_none());
    }

    #[test]
    fn tick_confirmed_misses_publish_exit_before_identity_withdrawal() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));
        for attempt in 1..=AGENT_MISS_CONFIRMATION_ATTEMPTS {
            let at = now + PROCESS_RECHECK_IDENTIFIED * u32::from(attempt);
            assert!(detector.tick(&tick_input(at, TickObservation::Begin)).probe);
            let mut result = detector.tick(&tick_input(at, tick_probe(None)));
            if result.screen {
                result =
                    detector.tick(&tick_input(at, TickObservation::Screen(Default::default())));
            }
            if attempt == AGENT_MISS_CONFIRMATION_ATTEMPTS {
                let update = result.state_changed.expect("confirmed exit");
                assert_eq!(update.agent, Some(Agent::Pi));
                assert!(update.process_exited);
                assert_eq!(update.state, AgentState::Idle);
            } else {
                assert!(
                    result
                        .state_changed
                        .is_none_or(|update| !update.process_exited)
                );
            }
        }
        let at =
            now + PROCESS_RECHECK_IDENTIFIED * (u32::from(AGENT_MISS_CONFIRMATION_ATTEMPTS) + 1);
        assert!(detector.tick(&tick_input(at, TickObservation::Begin)).probe);
        let cleared = detector.tick(&tick_input(at, tick_probe(None)));
        assert_eq!(detector.current_agent(), None);
        assert_eq!(
            cleared.state_changed.expect("withdraw identity").agent,
            None
        );
    }

    fn schedule_input(
        now: std::time::Instant,
        agent: Option<Agent>,
        observed_foreground_group: Option<u32>,
        lifecycle_authority_active: bool,
        shell_clear_pending: bool,
    ) -> ProcessProbeScheduleInput {
        ProcessProbeScheduleInput {
            now,
            agent,
            observed_foreground_group,
            lifecycle_authority_active,
            shell_clear_pending,
        }
    }

    #[test]
    fn scheduler_probes_initially_then_waits_for_activity_or_safety_interval() {
        let now = std::time::Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        assert!(matches!(
            scheduler.schedule(schedule_input(now, None, None, false, false)),
            ProbeScheduleDecision::Probe {
                had_previous_probe: false,
                ..
            }
        ));

        scheduler.probe_started(now);
        assert!(
            !scheduler
                .schedule(schedule_input(
                    now + PROCESS_RECHECK_MISSING_FOREGROUND_GROUP
                        - std::time::Duration::from_millis(1),
                    None,
                    None,
                    false,
                    false,
                ))
                .should_probe()
        );
        assert!(
            scheduler
                .schedule(schedule_input(
                    now + PROCESS_RECHECK_MISSING_FOREGROUND_GROUP,
                    None,
                    None,
                    false,
                    false,
                ))
                .should_probe()
        );
    }

    #[test]
    fn scheduler_uses_foreground_changes_and_lifecycle_authority_together() {
        let now = std::time::Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.probe_started(now);
        scheduler.last_foreground_group = Some(42);

        assert!(matches!(
            scheduler.schedule(schedule_input(
                now + std::time::Duration::from_millis(300),
                Some(Agent::Pi),
                Some(42),
                true,
                false,
            )),
            ProbeScheduleDecision::Skip {
                foreground_group_changed: false
            }
        ));
        assert!(
            scheduler
                .schedule(schedule_input(
                    now + std::time::Duration::from_millis(300),
                    Some(Agent::Pi),
                    Some(43),
                    true,
                    false,
                ))
                .should_probe()
        );
        assert!(
            scheduler
                .schedule(schedule_input(
                    now + std::time::Duration::from_millis(300),
                    Some(Agent::Pi),
                    Some(42),
                    true,
                    true,
                ))
                .should_probe()
        );
    }

    #[test]
    fn scheduler_keeps_identified_safety_probes_without_a_foreground_group() {
        let now = std::time::Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.probe_started(now);

        assert!(
            !scheduler
                .schedule(schedule_input(
                    now + PROCESS_RECHECK_IDENTIFIED - std::time::Duration::from_millis(1),
                    Some(Agent::Pi),
                    None,
                    true,
                    false,
                ))
                .should_probe()
        );
        assert!(
            scheduler
                .schedule(schedule_input(
                    now + PROCESS_RECHECK_IDENTIFIED,
                    Some(Agent::Pi),
                    None,
                    true,
                    false,
                ))
                .should_probe()
        );
    }

    #[test]
    fn lifecycle_authority_rechecks_identified_and_unidentified_processes_on_a_timer() {
        let now = std::time::Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.probe_started(now);
        scheduler.last_foreground_group = Some(42);

        assert!(
            !scheduler
                .schedule(schedule_input(
                    now + PROCESS_RECHECK_IDENTIFIED - std::time::Duration::from_millis(1),
                    Some(Agent::Pi),
                    Some(42),
                    true,
                    false,
                ))
                .should_probe()
        );
        assert!(
            scheduler
                .schedule(schedule_input(
                    now + PROCESS_RECHECK_IDENTIFIED,
                    Some(Agent::Pi),
                    Some(42),
                    true,
                    false,
                ))
                .should_probe()
        );

        let reacquisition_started = now + PROCESS_RECHECK_IDENTIFIED;
        scheduler.probe_started(reacquisition_started);
        assert!(
            !scheduler
                .schedule(schedule_input(
                    reacquisition_started + PROCESS_RECHECK_IDENTIFIED
                        - std::time::Duration::from_millis(1),
                    None,
                    Some(42),
                    true,
                    false,
                ))
                .should_probe()
        );
        assert!(
            scheduler
                .schedule(schedule_input(
                    reacquisition_started + PROCESS_RECHECK_IDENTIFIED,
                    None,
                    Some(42),
                    true,
                    false,
                ))
                .should_probe()
        );
    }

    #[test]
    fn scheduler_rechecks_acquisition_quickly_and_resets_after_quiet() {
        let now = std::time::Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.content_changed(now, None, false, true);

        assert!(
            !scheduler
                .schedule(schedule_input(
                    now + PROCESS_ACQUISITION_FAST_RECHECK - std::time::Duration::from_millis(1),
                    None,
                    None,
                    false,
                    false,
                ))
                .should_probe()
        );
        assert!(
            scheduler
                .schedule(schedule_input(
                    now + PROCESS_ACQUISITION_FAST_RECHECK,
                    None,
                    None,
                    false,
                    false,
                ))
                .should_probe()
        );

        assert!(
            !scheduler
                .schedule(schedule_input(
                    now + PROCESS_ACQUISITION_SLOW_RECHECK - std::time::Duration::from_millis(1),
                    None,
                    None,
                    false,
                    false,
                ))
                .should_probe()
        );
        assert!(
            scheduler
                .schedule(schedule_input(
                    now + PROCESS_ACQUISITION_SLOW_RECHECK,
                    None,
                    None,
                    false,
                    false,
                ))
                .should_probe()
        );

        scheduler.content_changed(
            now + PROCESS_ACQUISITION_WINDOW + PROCESS_ACQUISITION_IDLE_RESET,
            None,
            false,
            false,
        );
        assert!(scheduler.acquisition_started_at.is_none());
    }

    #[test]
    fn agent_detection_does_not_skip_before_first_published_report() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::AgentResume);
        assert_eq!(detector.current_agent(), None);
        assert_eq!(
            detector.tick_interval(now, false),
            std::time::Duration::from_millis(500)
        );
        assert!(detector.withhold_agent_absence(None, now));
        assert!(detector.should_read_screen(ScreenReadRequest {
            agent: None,
            agent_changed: false,
            process_exited: false,
            detection_content_seq: 1,
        }));
        assert!(detector.observe_screen_sequence(1));
        assert!(detector.should_read_screen(ScreenReadRequest {
            agent: None,
            agent_changed: false,
            process_exited: false,
            detection_content_seq: 1,
        }));
        detector.reset();
        assert!(detector.should_read_screen(ScreenReadRequest {
            agent: None,
            agent_changed: false,
            process_exited: false,
            detection_content_seq: 2,
        }));
    }

    #[test]
    fn detector_state_accepts_process_identity_without_a_runtime() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        let request = ProcessProbeRequest {
            now,
            observed_foreground_group: Some(25),
            lifecycle_authority_active: false,
        };
        let schedule = detector.schedule_process_probe(&request);
        assert!(schedule.should_probe());
        detector.probe_started(now);

        let probe = ProcessProbeResult {
            process_group_id: Some(25),
            foreground_is_pane_shell: false,
            suspended_agents: Vec::new(),
            identity: ProcessProbeIdentity::Agent {
                agent: Agent::Claude,
                process_name: "claude".to_string(),
            },
        };
        let change = detector.observe_process_probe(&probe, now, Some(25), schedule);

        assert_eq!(change.agent, Some(Agent::Claude));
        assert_eq!(change.process_detected, Some(Agent::Claude));
        assert!(!change.should_clear_osc_evidence);
    }

    #[test]
    fn reset_keeps_process_evidence_for_the_next_lifecycle_probe() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Claude));
        detector.pending_foreground_shell_clear = true;

        detector.reset();

        assert_eq!(detector.current_agent(), Some(Agent::Claude));
        // The exit still to report survives the reset.
        assert!(detector.process_exited(detector.current_agent()));
        assert_eq!(detector.state, AgentState::Unknown);
        assert!(
            detector
                .schedule_process_probe(&ProcessProbeRequest {
                    now,
                    observed_foreground_group: Some(42),
                    lifecycle_authority_active: true,
                })
                .should_probe()
        );
    }

    #[test]
    fn reset_does_not_rereport_an_exit_already_reported() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(None);
        detector.pending_confirmed_process_exit = Some(Agent::Pi);
        detector.pending_foreground_shell_clear = true;
        detector.foreground_shell_exit_reported = true;

        detector.reset();

        assert_eq!(detector.current_agent(), Some(Agent::Pi));
        assert!(!detector.process_exited(detector.current_agent()));
        // Presence is not rebuilt from the exited identity.
        assert_eq!(detector.agent_presence.current_agent(), None);
        let probe = ProcessProbeResult {
            process_group_id: Some(25),
            foreground_is_pane_shell: true,
            suspended_agents: Vec::new(),
            identity: ProcessProbeIdentity::Unidentified,
        };
        detector.observe_process_probe(
            &probe,
            now + std::time::Duration::from_secs(1),
            Some(25),
            ProbeScheduleDecision::Probe {
                foreground_group_changed: false,
                had_previous_probe: true,
            },
        );
        assert!(!detector.process_exited(detector.current_agent()));
    }

    #[test]
    fn confirmed_process_misses_publish_exit_before_clearing_identity() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));
        let probe = ProcessProbeResult {
            process_group_id: Some(25),
            foreground_is_pane_shell: false,
            suspended_agents: Vec::new(),
            identity: ProcessProbeIdentity::Unidentified,
        };

        for attempt in 1..=AGENT_MISS_CONFIRMATION_ATTEMPTS {
            let change = detector.observe_process_probe(
                &probe,
                now + std::time::Duration::from_secs(u64::from(attempt)),
                Some(25),
                ProbeScheduleDecision::Probe {
                    foreground_group_changed: false,
                    had_previous_probe: true,
                },
            );
            if attempt < AGENT_MISS_CONFIRMATION_ATTEMPTS {
                assert!(!detector.process_exited(detector.current_agent()));
                assert!(!change.agent_changed);
                assert_eq!(detector.current_agent(), Some(Agent::Pi));
            } else {
                assert!(detector.process_exited(detector.current_agent()));
                assert!(!change.agent_changed);
                assert_eq!(detector.current_agent(), Some(Agent::Pi));
            }
        }
    }

    #[test]
    fn suspended_agent_is_not_reported_as_a_process_exit_or_replacement() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Claude));
        let suspended_probe = ProcessProbeResult {
            process_group_id: Some(25),
            foreground_is_pane_shell: true,
            suspended_agents: vec![Agent::Claude],
            identity: ProcessProbeIdentity::Unidentified,
        };

        let change = detector.observe_process_probe(
            &suspended_probe,
            now,
            Some(25),
            ProbeScheduleDecision::Probe {
                foreground_group_changed: true,
                had_previous_probe: true,
            },
        );

        assert_eq!(detector.current_agent(), Some(Agent::Claude));
        assert!(!detector.process_exited(detector.current_agent()));
        assert!(!change.agent_changed);
        assert_eq!(change.process_detected, None);

        let resumed_probe = ProcessProbeResult {
            process_group_id: Some(27),
            foreground_is_pane_shell: false,
            suspended_agents: Vec::new(),
            identity: ProcessProbeIdentity::Agent {
                agent: Agent::Claude,
                process_name: "claude".to_string(),
            },
        };
        let resumed = detector.observe_process_probe(
            &resumed_probe,
            now + std::time::Duration::from_secs(1),
            Some(27),
            ProbeScheduleDecision::Probe {
                foreground_group_changed: true,
                had_previous_probe: true,
            },
        );

        assert_eq!(detector.current_agent(), Some(Agent::Claude));
        assert!(!resumed.agent_changed);
        assert_eq!(resumed.process_detected, None);
    }

    #[test]
    fn agent_detection_allows_scan_at_startup_grace_deadline() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        let deadline = now + AGENT_STARTUP_GRACE_WINDOW;
        detector.agent_startup_grace_until = Some(deadline);

        assert!(!detector.may_scan_screen(ScreenScanGate {
            now: deadline - std::time::Duration::from_millis(1),
            lifecycle_authority_active: false,
            process_exited: false,
        }));
        assert!(detector.may_scan_screen(ScreenScanGate {
            now: deadline,
            lifecycle_authority_active: false,
            process_exited: false,
        }));
        assert_eq!(detector.agent_startup_grace_until, None);
    }

    #[test]
    fn unidentified_probe_cannot_carry_a_process_name() {
        let result = ProcessProbeResult {
            process_group_id: Some(17),
            foreground_is_pane_shell: false,
            suspended_agents: Vec::new(),
            identity: ProcessProbeIdentity::Unidentified,
        };
        assert_eq!(result.agent(), None);
        assert_eq!(result.process_name(), None);
    }
}
