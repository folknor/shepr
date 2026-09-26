use std::sync::Mutex;

use tokio::sync::mpsc;
use tracing::warn;

use super::agent_detection::{
    AGENT_ABSENCE_STARTUP_HOLD, AGENT_PENDING_IDLE_RECHECK, AGENT_STARTUP_GRACE_WINDOW,
    DetectionPublishDecision, DetectionScreenReadDecision, DetectionScreenReadInput,
    PendingIdleConfirmation, ScreenDetectionPublishInput, decide_detection_screen_read,
    decide_screen_detection_publish, withhold_agent_absence,
};
use super::cwd::UsableCwd;
use super::launch::LaunchPurpose;
use super::terminal::PaneTerminal;
use crate::detect::{Agent, AgentState};
use crate::events::AppEvent;
use crate::layout::PaneId;

pub(super) const RELEASE_REACQUIRE_SUPPRESSION: std::time::Duration =
    std::time::Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
pub(super) struct PendingAgentRelease {
    pub(super) agent: Agent,
    pub(super) until: std::time::Instant,
}

pub(super) fn active_pending_release(
    pending_release: &Mutex<Option<PendingAgentRelease>>,
    now: std::time::Instant,
) -> Option<Agent> {
    let mut pending_release = crate::ghostty::lock_auxiliary(pending_release);
    match *pending_release {
        Some(pending) if now < pending.until => Some(pending.agent),
        Some(_) => {
            *pending_release = None;
            None
        }
        None => None,
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct StateChangedUpdate {
    pub(super) agent: Option<Agent>,
    pub(super) state: AgentState,
    pub(super) visible_blocker: bool,
    pub(super) process_exited: bool,
    pub(super) observed_at: std::time::Instant,
}

pub(super) async fn publish_state_changed_event(
    state_events: mpsc::Sender<AppEvent>,
    pane_id: PaneId,
    update: StateChangedUpdate,
) {
    // This runs on the async detector task, not the PTY reader thread.
    // Waiting for queue space here preserves correctness-critical state transitions
    // without blocking pane I/O.
    if let Err(e) = state_events
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
            err = %e,
            "failed to deliver StateChanged event"
        );
    }
}

pub(super) async fn publish_agent_process_detected_event(
    state_events: mpsc::Sender<AppEvent>,
    pane_id: PaneId,
    agent: Agent,
    observed_at: std::time::Instant,
) {
    if let Err(e) = state_events
        .send(AppEvent::AgentProcessDetected {
            pane_id,
            agent,
            observed_at,
        })
        .await
    {
        warn!(
            pane = pane_id.raw(),
            err = %e,
            "failed to deliver AgentProcessDetected event"
        );
    }
}

pub(super) const AGENT_MISS_CONFIRMATION_ATTEMPTS: u8 = 6;
const PROCESS_RECHECK_IDENTIFIED: std::time::Duration = std::time::Duration::from_secs(5);
const PROCESS_RECHECK_MISSING_FOREGROUND_GROUP: std::time::Duration =
    std::time::Duration::from_secs(30);
const PROCESS_ACQUISITION_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);
const PROCESS_ACQUISITION_FAST_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);
const PROCESS_ACQUISITION_FAST_RECHECK: std::time::Duration = std::time::Duration::from_millis(500);
const PROCESS_ACQUISITION_SLOW_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);
const PROCESS_ACQUISITION_IDLE_RESET: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub(super) struct AgentDetectionPresence {
    current_agent: Option<Agent>,
    consecutive_misses: u8,
}

pub(super) fn absolute_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    crate::detect::process_cwd(pid).filter(|cwd| cwd.is_absolute())
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
    let job = crate::detect::foreground_job(shell_pid)?;
    for process in job.processes {
        if process.pid == shell_pid {
            continue;
        }
        let Some(cwd) = absolute_process_cwd(process.pid) else {
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
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ForegroundShellProbe {
    pub(super) previous_agent: Option<Agent>,
    pub(super) identified_agent: Option<Agent>,
    pub(super) foreground_is_pane_shell: bool,
    pub(super) process_exit_reported: bool,
}

pub(super) fn foreground_shell_agent_action(
    probe: ForegroundShellProbe,
) -> ForegroundShellAgentAction {
    let Some(previous_agent) = probe.previous_agent else {
        return ForegroundShellAgentAction::ObserveProbe;
    };
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
        // transition for the previous agent so notifications and wait-agent callers
        // observe completion before the pane becomes unknown.
        return ForegroundShellAgentAction::ReportProcessExit;
    }

    ForegroundShellAgentAction::ObserveProbe
}

/// Drops retained OSC evidence when changing away from an identified agent.
/// First acquisition keeps bytes that the newly identified process may have
/// emitted before the process probe recognized it.
pub(super) fn clear_osc_evidence_for_agent_transition(
    terminal: &PaneTerminal,
    previous_agent: Option<Agent>,
) {
    if previous_agent.is_some() {
        terminal.clear_agent_osc_state();
    }
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
    pub(super) suppressed_agent: Option<Agent>,
    pub(super) observed_foreground_group: Option<u32>,
    pub(super) lifecycle_authority_active: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ProcessProbeScheduleInput {
    now: std::time::Instant,
    agent: Option<Agent>,
    suppressed_agent: Option<Agent>,
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
    release_was_active: bool,
}

impl ProcessProbeScheduler {
    pub(super) fn new(now: std::time::Instant) -> Self {
        Self {
            last_check: now,
            last_foreground_group: None,
            has_probe: false,
            acquisition_started_at: None,
            last_content_change_at: None,
            release_was_active: false,
        }
    }

    pub(super) fn reset(&mut self) {
        self.last_foreground_group = None;
        self.has_probe = false;
        self.acquisition_started_at = None;
        self.last_content_change_at = None;
        self.release_was_active = false;
    }

    pub(super) fn observe_release(&mut self, active: bool) {
        if !active && self.release_was_active {
            self.has_probe = false;
            self.acquisition_started_at = None;
            self.last_content_change_at = None;
        }
        self.release_was_active = active;
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

        let lifecycle_authority_can_skip = input.lifecycle_authority_active
            && input.observed_foreground_group.is_some()
            && !input.shell_clear_pending
            && input.suppressed_agent.is_none()
            && self.has_probe
            && !group_changed;
        if lifecycle_authority_can_skip {
            return ProbeScheduleDecision::Skip {
                foreground_group_changed: group_changed,
            };
        }

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
            && input.suppressed_agent.is_none()
            && !group_changed
            && !acquisition_due
            && acquisition_age.is_some_and(|age| age <= PROCESS_ACQUISITION_WINDOW)
        {
            return ProbeScheduleDecision::Skip {
                foreground_group_changed: group_changed,
            };
        }

        let should_probe = if input.shell_clear_pending {
            true
        } else if input.suppressed_agent.is_some() {
            !self.has_probe || group_changed
        } else if acquisition_due {
            true
        } else if input.agent.is_none() {
            !self.has_probe
                || group_changed
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
        suppressed_agent: Option<Agent>,
        group_changed: bool,
        changed: bool,
    ) {
        if agent.is_some() || suppressed_agent.is_some() || group_changed {
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
    pub(super) detection_content_seq: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScreenPublishContext {
    pub(super) now: std::time::Instant,
    pub(super) process_exited: bool,
    pub(super) agent_changed: bool,
}

pub(super) struct PromptObservationInput<'a> {
    pub(super) agent: Option<Agent>,
    pub(super) content: &'a str,
    pub(super) detection: Option<&'a crate::detect::AgentDetection>,
    pub(super) process_exited: bool,
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
    pub(super) clear_pending_release: bool,
}

/// The detector's mutable state, independent of the PTY runtime and terminal.
/// Its transitions can be exercised with fake times and process observations.
pub(super) struct DetectorState {
    agent_presence: AgentDetectionPresence,
    state: AgentState,
    last_visible_idle: bool,
    last_visible_blocker: bool,
    last_visible_working: bool,
    last_visible_signal_refresh: Option<std::time::Instant>,
    scheduler: ProcessProbeScheduler,
    pending_foreground_shell_clear: bool,
    foreground_shell_exit_reported: bool,
    last_detection_text: String,
    last_screen_scan_detection_content_seq: Option<u64>,
    agent_startup_grace_until: Option<std::time::Instant>,
    pending_idle: PendingIdleConfirmation,
    last_prompt_observation: Option<(Agent, bool)>,
    agent_absence_hold_until: Option<std::time::Instant>,
}

impl DetectorState {
    pub(super) fn new(now: std::time::Instant, purpose: LaunchPurpose) -> Self {
        let agent_absence_hold_until = match purpose {
            LaunchPurpose::Fresh => None,
            LaunchPurpose::AgentResume => now.checked_add(AGENT_ABSENCE_STARTUP_HOLD),
        };
        Self {
            agent_presence: AgentDetectionPresence::from_agent(None),
            state: AgentState::Idle,
            last_visible_idle: false,
            last_visible_blocker: false,
            last_visible_working: false,
            last_visible_signal_refresh: None,
            scheduler: ProcessProbeScheduler::new(now),
            pending_foreground_shell_clear: false,
            foreground_shell_exit_reported: false,
            last_detection_text: String::new(),
            last_screen_scan_detection_content_seq: None,
            agent_startup_grace_until: None,
            pending_idle: PendingIdleConfirmation::default(),
            last_prompt_observation: None,
            agent_absence_hold_until,
        }
    }

    pub(super) fn current_agent(&self) -> Option<Agent> {
        self.agent_presence.current_agent()
    }

    pub(super) fn tick_interval(
        &self,
        pending_release_active: bool,
        transient_color_override: bool,
    ) -> std::time::Duration {
        if pending_release_active || transient_color_override {
            std::time::Duration::from_millis(50)
        } else if self.pending_idle.active() {
            AGENT_PENDING_IDLE_RECHECK
        } else if self.current_agent().is_none() {
            std::time::Duration::from_millis(500)
        } else {
            std::time::Duration::from_millis(300)
        }
    }

    pub(super) fn reset(&mut self) {
        self.agent_presence = AgentDetectionPresence::from_agent(None);
        self.state = AgentState::Unknown;
        self.last_visible_idle = false;
        self.scheduler.reset();
        self.pending_foreground_shell_clear = false;
        self.foreground_shell_exit_reported = false;
        self.last_visible_blocker = false;
        self.last_visible_working = false;
        self.last_visible_signal_refresh = None;
        self.last_detection_text.clear();
        self.last_screen_scan_detection_content_seq = None;
        self.agent_startup_grace_until = None;
        self.pending_idle.clear();
    }

    pub(super) fn observe_release(&mut self, active: bool) {
        self.scheduler.observe_release(active);
    }

    pub(super) fn schedule_process_probe(
        &self,
        request: &ProcessProbeRequest,
    ) -> ProbeScheduleDecision {
        self.scheduler.schedule(ProcessProbeScheduleInput {
            now: request.now,
            agent: self.current_agent(),
            suppressed_agent: request.suppressed_agent,
            observed_foreground_group: request.observed_foreground_group,
            lifecycle_authority_active: request.lifecycle_authority_active,
            shell_clear_pending: self.pending_foreground_shell_clear,
        })
    }

    pub(super) fn probe_started(&mut self, now: std::time::Instant) {
        self.scheduler.probe_started(now);
    }

    pub(super) fn observe_process_probe(
        &mut self,
        probe: &ProcessProbeResult,
        now: std::time::Instant,
        observed_foreground_group: Option<u32>,
        suppressed_agent: Option<Agent>,
        schedule: ProbeScheduleDecision,
    ) -> AgentProcessChange {
        let process_name = probe.process_name().map(str::to_owned);
        let process_group_id = probe.process_group_id();
        let foreground_is_pane_shell = probe.foreground_is_pane_shell();
        let mut identified_agent = probe.agent();
        let clear_pending_release = if let Some(suppressed_agent) = suppressed_agent {
            if identified_agent == Some(suppressed_agent) {
                identified_agent = None;
                false
            } else {
                true
            }
        } else {
            false
        };

        let previous_agent = self.current_agent();
        let action = foreground_shell_agent_action(ForegroundShellProbe {
            previous_agent,
            identified_agent,
            foreground_is_pane_shell,
            process_exit_reported: self.foreground_shell_exit_reported,
        });
        let agent_changed = match action {
            ForegroundShellAgentAction::ReportReplacementProcess => {
                self.pending_foreground_shell_clear = false;
                self.foreground_shell_exit_reported = false;
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
                self.agent_presence.clear_current_agent()
            }
            ForegroundShellAgentAction::ObserveProbe => {
                self.pending_foreground_shell_clear = false;
                self.foreground_shell_exit_reported = false;
                self.agent_presence.observe_process_probe(identified_agent)
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
            self.last_prompt_observation = None;
            self.last_screen_scan_detection_content_seq = None;
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
            clear_pending_release,
        }
    }

    pub(super) fn clear_pending_idle(&mut self) {
        self.pending_idle.clear();
    }

    pub(super) fn process_exited(&self, agent: Option<Agent>) -> bool {
        self.pending_foreground_shell_clear
            && agent.is_some()
            && !self.foreground_shell_exit_reported
    }

    pub(super) fn may_scan_screen(&mut self, gate: ScreenScanGate) -> bool {
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
                return false;
            }
        }
        true
    }

    pub(super) fn should_read_screen(&self, request: ScreenReadRequest) -> bool {
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

    pub(super) fn detection_content(
        &mut self,
        agent: Option<Agent>,
        detection_content_seq: Option<u64>,
        identified_agent_text: Option<String>,
    ) -> (String, bool) {
        let (content, changed) = if agent.is_some() {
            let content = identified_agent_text.unwrap_or_default();
            let changed = content != self.last_detection_text;
            self.last_detection_text.clone_from(&content);
            (content, changed)
        } else {
            let changed = self.last_screen_scan_detection_content_seq != detection_content_seq;
            self.last_detection_text.clear();
            (String::new(), changed)
        };
        self.last_screen_scan_detection_content_seq = detection_content_seq;
        (content, changed)
    }

    pub(super) fn note_content_change(
        &mut self,
        now: std::time::Instant,
        suppressed_agent: Option<Agent>,
        group_changed: bool,
        changed: bool,
    ) {
        self.scheduler.content_changed(
            now,
            self.current_agent(),
            suppressed_agent,
            group_changed,
            changed,
        );
    }

    pub(super) fn withhold_agent_absence(
        &mut self,
        agent: Option<Agent>,
        now: std::time::Instant,
    ) -> bool {
        withhold_agent_absence(agent, &mut self.agent_absence_hold_until, now)
    }

    pub(super) fn screen_publish_decision(
        &mut self,
        screen_detection: crate::detect::AgentDetection,
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

    pub(super) async fn publish_prompt_observation(
        &mut self,
        state_events: &mpsc::Sender<AppEvent>,
        pane_id: PaneId,
        input: PromptObservationInput<'_>,
    ) {
        let prompt_agent = input.agent.filter(|agent| agent.prompt_observation());
        let ready = prompt_agent.is_some_and(|agent| {
            !input.process_exited
                && input
                    .detection
                    .is_some_and(|detection| detection.state == AgentState::Unknown)
                && agent.prompt_ready(input.content)
        });
        let next = prompt_agent
            .or_else(|| self.last_prompt_observation.map(|(agent, _)| agent))
            .map(|agent| (agent, ready));
        if next == self.last_prompt_observation {
            return;
        }
        self.last_prompt_observation = next;
        if let Some((agent, ready)) = next
            && let Err(err) = state_events
                .send(AppEvent::AgentPromptObserved {
                    pane_id,
                    agent,
                    ready,
                })
                .await
        {
            warn!(pane = pane_id.raw(), %err, "failed to deliver agent prompt observation");
        }
    }

    pub(super) async fn apply_publish_update(
        &mut self,
        state_events: mpsc::Sender<AppEvent>,
        pane_id: PaneId,
        agent: Option<Agent>,
        update: AgentDetectionPublishUpdate,
        observed_at: std::time::Instant,
    ) {
        self.state = update.state;
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
        publish_state_changed_event(
            state_events,
            pane_id,
            StateChangedUpdate {
                agent,
                state: update.state,
                visible_blocker: update.visible_blocker,
                process_exited: update.process_exited,
                observed_at,
            },
        )
        .await;
    }
}

pub(super) fn agent_hint_for_foreground_job_members(
    job: &crate::detect::ForegroundJob,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<Agent> {
    read_hint(job.process_group_id)
        .or_else(|| agent_hint_for_non_leader_foreground_job_members(job, read_hint))
}

fn agent_hint_for_non_leader_foreground_job_members(
    job: &crate::detect::ForegroundJob,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<Agent> {
    job.processes
        .iter()
        .filter(|process| process.pid != job.process_group_id)
        .find_map(|process| read_hint(process.pid))
}

fn identify_process_group_leader_in_job(
    job: &crate::detect::ForegroundJob,
) -> Option<(Agent, String)> {
    let leader = job
        .processes
        .iter()
        .find(|process| process.pid == job.process_group_id)?;
    let leader_job = crate::detect::ForegroundJob {
        process_group_id: job.process_group_id,
        processes: vec![leader.clone()],
    };
    crate::detect::identify_agent_in_job(&leader_job)
}

fn process_probe_result(
    job: &crate::detect::ForegroundJob,
    pid: u32,
    agent: Agent,
    process_name: String,
) -> ProcessProbeResult {
    ProcessProbeResult {
        process_group_id: Some(job.process_group_id),
        foreground_is_pane_shell: job.processes.iter().any(|process| process.pid == pid),
        identity: ProcessProbeIdentity::Agent {
            agent,
            process_name,
        },
    }
}

fn hinted_process_probe_result(
    job: &crate::detect::ForegroundJob,
    pid: u32,
    read_hint: impl Fn(u32) -> Option<Agent>,
) -> Option<ProcessProbeResult> {
    let agent = agent_hint_for_foreground_job_members(job, read_hint)?;
    Some(process_probe_result(
        job,
        pid,
        agent,
        crate::detect::agent_label(agent).to_string(),
    ))
}

pub(super) fn probe_foreground_process_from_jobs(
    pid: u32,
    foreground_pgid: Option<u32>,
    leader_job: Option<&crate::detect::ForegroundJob>,
    foreground_job: impl FnOnce() -> Option<crate::detect::ForegroundJob>,
    read_hint: impl Fn(u32) -> Option<Agent> + Copy,
) -> ProcessProbeResult {
    if let Some(job) = leader_job {
        if let Some(hinted) = hinted_process_probe_result(job, pid, read_hint) {
            return hinted;
        }
        if let Some((agent, process_name)) = crate::detect::identify_agent_in_job(job) {
            return process_probe_result(job, pid, agent, process_name);
        }
    }

    let foreground_job = foreground_job();
    if let Some(job) = foreground_job.as_ref() {
        if let Some(agent) = read_hint(job.process_group_id) {
            return process_probe_result(
                job,
                pid,
                agent,
                crate::detect::agent_label(agent).to_string(),
            );
        }
        if let Some((agent, process_name)) = identify_process_group_leader_in_job(job) {
            return process_probe_result(job, pid, agent, process_name);
        }
        if let Some(agent) = agent_hint_for_non_leader_foreground_job_members(job, read_hint) {
            return process_probe_result(
                job,
                pid,
                agent,
                crate::detect::agent_label(agent).to_string(),
            );
        }

        let identified = crate::detect::identify_agent_in_job(job);
        return ProcessProbeResult {
            process_group_id: Some(job.process_group_id),
            foreground_is_pane_shell: job.processes.iter().any(|process| process.pid == pid),
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
        identity: ProcessProbeIdentity::Unidentified,
    }
}

pub(super) fn probe_foreground_process(
    pid: u32,
    foreground_pgid: Option<u32>,
) -> ProcessProbeResult {
    probe_foreground_process_from_jobs(
        pid,
        foreground_pgid,
        foreground_pgid
            .and_then(crate::detect::foreground_group_leader_job)
            .as_ref(),
        || crate::detect::foreground_job(pid),
        crate::detect::process_agent_hint,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule_input(
        now: std::time::Instant,
        agent: Option<Agent>,
        suppressed_agent: Option<Agent>,
        observed_foreground_group: Option<u32>,
        lifecycle_authority_active: bool,
        shell_clear_pending: bool,
    ) -> ProcessProbeScheduleInput {
        ProcessProbeScheduleInput {
            now,
            agent,
            suppressed_agent,
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
            scheduler.schedule(schedule_input(now, None, None, None, false, false)),
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
                None,
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
                    None,
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
                    None,
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
                    None,
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
                    None,
                    true,
                    false,
                ))
                .should_probe()
        );
    }

    #[test]
    fn stable_release_suppression_waits_for_a_group_change() {
        let now = std::time::Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.probe_started(now);
        scheduler.last_foreground_group = Some(42);

        assert!(
            !scheduler
                .schedule(schedule_input(
                    now + std::time::Duration::from_millis(50),
                    None,
                    Some(Agent::Codex),
                    Some(42),
                    false,
                    false,
                ))
                .should_probe()
        );
        assert!(
            scheduler
                .schedule(schedule_input(
                    now + std::time::Duration::from_millis(50),
                    None,
                    Some(Agent::Codex),
                    Some(43),
                    false,
                    false,
                ))
                .should_probe()
        );
    }

    #[test]
    fn scheduler_rechecks_acquisition_quickly_and_resets_after_quiet() {
        let now = std::time::Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.content_changed(now, None, None, false, true);

        assert!(
            !scheduler
                .schedule(schedule_input(
                    now + PROCESS_ACQUISITION_FAST_RECHECK - std::time::Duration::from_millis(1),
                    None,
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
                    None,
                    false,
                    false,
                ))
                .should_probe()
        );

        scheduler.content_changed(
            now + PROCESS_ACQUISITION_WINDOW + PROCESS_ACQUISITION_IDLE_RESET,
            None,
            None,
            false,
            false,
        );
        assert!(scheduler.acquisition_started_at.is_none());
    }

    #[test]
    fn detector_state_needs_no_runtime_to_apply_launch_and_screen_rules() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::AgentResume);
        assert_eq!(detector.current_agent(), None);
        assert_eq!(
            detector.tick_interval(false, false),
            std::time::Duration::from_millis(500)
        );
        assert!(detector.withhold_agent_absence(None, now));
        assert!(detector.should_read_screen(ScreenReadRequest {
            agent: None,
            agent_changed: false,
            process_exited: false,
            detection_content_seq: Some(1),
        }));
        let (content, changed) = detector.detection_content(None, Some(1), None);
        assert!(content.is_empty());
        assert!(changed);
        assert!(!detector.should_read_screen(ScreenReadRequest {
            agent: None,
            agent_changed: false,
            process_exited: false,
            detection_content_seq: Some(1),
        }));
        detector.reset();
        assert!(detector.should_read_screen(ScreenReadRequest {
            agent: None,
            agent_changed: false,
            process_exited: false,
            detection_content_seq: Some(2),
        }));
    }

    #[test]
    fn detector_state_accepts_process_identity_without_a_runtime() {
        let now = std::time::Instant::now();
        let mut detector = DetectorState::new(now, LaunchPurpose::Fresh);
        let request = ProcessProbeRequest {
            now,
            suppressed_agent: None,
            observed_foreground_group: Some(25),
            lifecycle_authority_active: false,
        };
        let schedule = detector.schedule_process_probe(&request);
        assert!(schedule.should_probe());
        detector.probe_started(now);

        let probe = ProcessProbeResult {
            process_group_id: Some(25),
            foreground_is_pane_shell: false,
            identity: ProcessProbeIdentity::Agent {
                agent: Agent::Claude,
                process_name: "claude".to_string(),
            },
        };
        let change = detector.observe_process_probe(&probe, now, Some(25), None, schedule);

        assert_eq!(change.agent, Some(Agent::Claude));
        assert_eq!(change.process_detected, Some(Agent::Claude));
        assert!(!change.should_clear_osc_evidence);
    }

    #[test]
    fn unidentified_probe_cannot_carry_a_process_name() {
        let result = ProcessProbeResult {
            process_group_id: Some(17),
            foreground_is_pane_shell: false,
            identity: ProcessProbeIdentity::Unidentified,
        };
        assert_eq!(result.agent(), None);
        assert_eq!(result.process_name(), None);
    }
}
