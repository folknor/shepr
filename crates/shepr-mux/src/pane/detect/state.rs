//! The detector's state and the tick protocol that drives it.
//!
//! A tick may need I/O the detector does not perform: a process probe, then
//! the screen text. `begin` returns `Done`, `NeedsProbe` or `NeedsScreen`; the
//! token it returns carries what that tick learned so far and is consumed by
//! `resume` with the I/O result. A token cannot be resumed twice or outlive its
//! tick, so no per-tick scratch lives in the detector.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::probe::{AgentDetectionPresence, AgentExitPhase, AgentProcessChange};
use super::publish::{ScreenDetectionCacheEntry, ScreenStep, StateChangedUpdate};
use super::schedule::{ProbeScheduleDecision, ProcessProbeScheduler};
use crate::limits::{
    AGENT_ABSENCE_STARTUP_HOLD, AGENT_PENDING_IDLE_CAP, AGENT_PENDING_IDLE_RECHECK,
    PROCESS_RECHECK_ACTIVE_AGENT, PROCESS_RECHECK_NO_AGENT, PROCESS_RECHECK_TRANSIENT,
    TRANSIENT_COLOR_RECHECK_WINDOW,
};
use crate::pane::agent_detection::PendingIdleConfirmation;
use crate::pane::launch::LaunchKind;
use crate::pane::process_probe::ProcessProbeResult;
use crate::pane::terminal::AgentDetectionInputs;
use shepr_agent::Agent;
use shepr_detect::Detection;
use shepr_platform::Pgid;

/// One tick's observations. The runtime passes one set (time, group, content
/// sequence) through a tick; the detector adds what the tick itself decides,
/// so every gate and decision of the tick reads the same context.
#[derive(Debug, Clone, Copy)]
pub(in crate::pane) struct TickContext {
    pub(super) now: Instant,
    pub(super) foreground_group: Option<Pgid>,
    pub(super) content_seq: u64,
    pub(super) lifecycle_authority_active: bool,
    pub(super) theme_restore_candidate: bool,
    /// Set by the schedule decision.
    pub(super) group_changed: bool,
    /// Set by a probe that changed the identified agent.
    pub(super) agent_changed: bool,
}

impl TickContext {
    pub(in crate::pane) fn new(
        now: Instant,
        foreground_group: Option<Pgid>,
        content_seq: u64,
        lifecycle_authority_active: bool,
        theme_restore_candidate: bool,
    ) -> Self {
        Self {
            now,
            foreground_group,
            content_seq,
            lifecycle_authority_active,
            theme_restore_candidate,
            group_changed: false,
            agent_changed: false,
        }
    }
}

pub(in crate::pane) struct TickOutput {
    /// Filled in by the caller from the probe step, which is published first.
    pub(in crate::pane) process_change: Option<AgentProcessChange>,
    pub(in crate::pane) state_changed: Option<StateChangedUpdate>,
    pub(in crate::pane) next_wake: Duration,
}

/// A mux gate that can temporarily keep a fresh screen verdict from changing
/// the pane's published state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorGate {
    PendingIdleConfirmation,
    StartupGrace,
    ResumeAbsenceHold,
}

#[derive(Clone, Default)]
pub struct DetectorGateDiagnostics(Arc<Mutex<DetectorGateSnapshot>>);

#[derive(Default)]
struct DetectorGateSnapshot {
    pending_idle_since: Option<Instant>,
    startup_grace_until: Option<Instant>,
    agent_absence_hold_until: Option<Instant>,
    agent_present: bool,
}

impl DetectorGateDiagnostics {
    /// The active gate from the detector's latest state, with timed holds
    /// checked against the current time.
    pub fn active_gate(&self) -> Option<DetectorGate> {
        self.0.lock().ok()?.active_gate(Instant::now())
    }

    pub(in crate::pane) fn update(&self, detector: &DetectorState) {
        if let Ok(mut snapshot) = self.0.lock() {
            snapshot.pending_idle_since = detector.pending_idle.started_at();
            snapshot.startup_grace_until = detector.agent_startup_grace_until;
            snapshot.agent_absence_hold_until = detector.agent_absence_hold_until;
            snapshot.agent_present = detector.current_agent().is_some();
        }
    }
}

impl DetectorGateSnapshot {
    fn active_gate(&self, now: Instant) -> Option<DetectorGate> {
        if self.startup_grace_until.is_some_and(|until| now < until) {
            return Some(DetectorGate::StartupGrace);
        }
        if !self.agent_present
            && self
                .agent_absence_hold_until
                .is_some_and(|until| now < until)
        {
            return Some(DetectorGate::ResumeAbsenceHold);
        }
        if self.pending_idle_since.is_some_and(|started_at| {
            now.saturating_duration_since(started_at) < AGENT_PENDING_IDLE_CAP
        }) {
            return Some(DetectorGate::PendingIdleConfirmation);
        }
        None
    }
}

/// The start of a tick.
pub(in crate::pane) enum Tick {
    Done(TickOutput),
    NeedsProbe(ProbeTick),
    NeedsScreen(ScreenTick),
}

/// What remains of a tick once its probe has been absorbed.
pub(in crate::pane) enum Step {
    Done(TickOutput),
    NeedsScreen(ScreenTick),
}

/// A tick waiting for a process probe.
pub(in crate::pane) struct ProbeTick {
    tick: TickContext,
    schedule: ProbeScheduleDecision,
}

/// A tick waiting for the screen text.
pub(in crate::pane) struct ScreenTick {
    tick: TickContext,
}

impl ProbeTick {
    /// Absorbs the probe. The identity change is returned beside the rest of
    /// the tick because the caller acts on it (OSC evidence, the log) before it
    /// reads the screen.
    pub(in crate::pane) fn resume(
        self,
        detector: &mut DetectorState,
        probe: &ProcessProbeResult,
    ) -> (AgentProcessChange, Step) {
        let mut tick = self.tick;
        let change = detector.observe_process_probe(&tick, probe, self.schedule);
        tick.agent_changed = change.agent_changed;
        (change, detector.screen_step(tick))
    }
}

impl ScreenTick {
    pub(in crate::pane) fn resume(
        self,
        detector: &mut DetectorState,
        screen: Option<&AgentDetectionInputs>,
    ) -> TickOutput {
        let update = if let Some(screen) = screen {
            detector.complete_screen(&self.tick, screen)
        } else {
            // Keep the previous evidence and make the next tick retry even if
            // this scan was requested only because the agent identity changed.
            detector.last_screen_scan_detection_content_seq = None;
            None
        };
        detector.finish_tick(&self.tick, update)
    }
}

/// The detector's mutable state, independent of the PTY runtime and terminal.
/// Its transitions can be exercised with fake times and process observations.
pub(in crate::pane) struct DetectorState {
    pub(super) agent_presence: AgentDetectionPresence,
    pub(super) last_published: Option<Detection>,
    pub(super) last_visible_signal_refresh: Option<Instant>,
    pub(super) scheduler: ProcessProbeScheduler,
    pub(super) transient_color_recheck_until: Option<Instant>,
    pub(super) exit_phase: AgentExitPhase,
    pub(super) last_screen_scan_detection_content_seq: Option<u64>,
    pub(super) last_screen_detection: Option<ScreenDetectionCacheEntry>,
    pub(super) agent_startup_grace_until: Option<Instant>,
    pub(super) pending_idle: PendingIdleConfirmation,
    pub(super) agent_absence_hold_until: Option<Instant>,
}

impl DetectorState {
    pub(in crate::pane) fn new(now: Instant, purpose: LaunchKind) -> Self {
        let agent_absence_hold_until = match purpose {
            LaunchKind::Fresh | LaunchKind::Restored => None,
            LaunchKind::AgentResume => now.checked_add(AGENT_ABSENCE_STARTUP_HOLD),
        };
        Self {
            agent_presence: AgentDetectionPresence::from_agent(None),
            last_published: None,
            last_visible_signal_refresh: None,
            scheduler: ProcessProbeScheduler::new(now),
            transient_color_recheck_until: None,
            exit_phase: AgentExitPhase::Observing,
            last_screen_scan_detection_content_seq: None,
            last_screen_detection: None,
            agent_startup_grace_until: None,
            pending_idle: PendingIdleConfirmation::default(),
            agent_absence_hold_until,
        }
    }

    /// All scheduling, identity, screen-cache and publication transitions run
    /// here. The runtime supplies the observations and executes the I/O a
    /// token asks for; it does not decide which transition wins or mutate
    /// publication state.
    pub(in crate::pane) fn begin(&mut self, mut tick: TickContext) -> Tick {
        let schedule = self.schedule_process_probe(&tick);
        tick.group_changed = schedule.foreground_group_changed();
        if tick.group_changed {
            self.note_foreground_group_change(&tick);
        }
        if schedule.should_probe() {
            self.scheduler.probe_started(tick.now);
            return Tick::NeedsProbe(ProbeTick { tick, schedule });
        }
        match self.screen_step(tick) {
            Step::Done(output) => Tick::Done(output),
            Step::NeedsScreen(screen) => Tick::NeedsScreen(screen),
        }
    }

    fn screen_step(&mut self, tick: TickContext) -> Step {
        match self.begin_screen(&tick) {
            ScreenStep::Finished(update) => Step::Done(self.finish_tick(&tick, update)),
            ScreenStep::NeedsScreen => Step::NeedsScreen(ScreenTick { tick }),
        }
    }

    fn finish_tick(
        &mut self,
        tick: &TickContext,
        state_changed: Option<StateChangedUpdate>,
    ) -> TickOutput {
        TickOutput {
            process_change: None,
            state_changed,
            next_wake: self.tick_interval(tick.now, tick.theme_restore_candidate),
        }
    }

    pub(in crate::pane) fn current_agent(&self) -> Option<Agent> {
        // The server must receive the exit against the identity that just
        // passed miss confirmation, before the detector withdraws it.
        self.exit_phase
            .agent()
            .or_else(|| self.agent_presence.current_agent())
    }

    pub(super) fn process_exited(&self) -> bool {
        matches!(self.exit_phase, AgentExitPhase::ReportOwed { .. })
    }

    pub(super) fn schedule_process_probe(&self, tick: &TickContext) -> ProbeScheduleDecision {
        self.scheduler
            .schedule(tick, self.current_agent(), self.exit_phase.clear_pending())
    }

    pub(super) fn tick_interval(
        &mut self,
        now: Instant,
        theme_restore_candidate: bool,
    ) -> Duration {
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

    fn note_foreground_group_change(&mut self, tick: &TickContext) {
        self.transient_color_recheck_until = tick
            .theme_restore_candidate
            .then(|| tick.now.checked_add(TRANSIENT_COLOR_RECHECK_WINDOW))
            .flatten();
    }

    pub(in crate::pane) fn reset(&mut self) {
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
        self.last_published = Some(Detection::Unknown);
        self.scheduler.reset();
        self.transient_color_recheck_until = None;
        self.last_visible_signal_refresh = None;
        self.last_screen_scan_detection_content_seq = None;
        self.last_screen_detection = None;
        // Reset establishes Unknown as a baseline; a new detector has no
        // baseline and keeps reading until its first report publishes.
        self.agent_startup_grace_until = None;
        self.pending_idle.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{
        AGENT_MISS_CONFIRMATION_ATTEMPTS, AGENT_STARTUP_GRACE_WINDOW, PROCESS_RECHECK_IDENTIFIED,
    };
    use crate::pane::process_probe::ProcessProbeIdentity;
    use shepr_agent::AgentState;

    fn pgid(value: u32) -> Pgid {
        Pgid::new(value).expect("test process group")
    }

    fn tick(now: Instant) -> TickContext {
        TickContext::new(now, Some(pgid(25)), 1, false, false)
    }

    fn probe(agent: Option<Agent>) -> ProcessProbeResult {
        ProcessProbeResult {
            process_group_id: Pgid::new(25),
            foreground_is_pane_shell: false,
            suspended_agents: Vec::new(),
            identity: agent.map_or(ProcessProbeIdentity::Unidentified, |agent| {
                ProcessProbeIdentity::Agent {
                    agent,
                    process_name: "agent".into(),
                }
            }),
        }
    }

    fn needs_probe(tick: Tick) -> ProbeTick {
        match tick {
            Tick::NeedsProbe(token) => token,
            Tick::Done(_) | Tick::NeedsScreen(_) => panic!("expected the tick to need a probe"),
        }
    }

    fn done(tick: Tick) -> TickOutput {
        match tick {
            Tick::Done(output) => output,
            Tick::NeedsProbe(_) | Tick::NeedsScreen(_) => panic!("expected the tick to be done"),
        }
    }

    #[test]
    fn diagnostics_report_startup_and_resume_absence_holds() {
        let now = Instant::now();
        let diagnostics = DetectorGateDiagnostics::default();

        let resume = DetectorState::new(now, LaunchKind::AgentResume);
        diagnostics.update(&resume);
        assert_eq!(
            diagnostics.active_gate(),
            Some(DetectorGate::ResumeAbsenceHold)
        );

        let mut startup = DetectorState::new(now, LaunchKind::Fresh);
        startup.agent_startup_grace_until = Some(now + AGENT_STARTUP_GRACE_WINDOW);
        diagnostics.update(&startup);
        assert_eq!(diagnostics.active_gate(), Some(DetectorGate::StartupGrace));
    }

    fn step_done(step: Step) -> TickOutput {
        match step {
            Step::Done(output) => output,
            Step::NeedsScreen(_) => panic!("expected the step to be done"),
        }
    }

    #[test]
    fn tick_retries_the_initial_absence_report_until_restore_hold_expires() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::AgentResume);
        let token = needs_probe(detector.begin(tick(now)));
        let (_change, step) = token.resume(&mut detector, &probe(None));
        let held = step_done(step);
        assert!(held.state_changed.is_none());
        let deadline = now + AGENT_ABSENCE_STARTUP_HOLD;
        // The hold outlasts the unidentified recheck, so the tick at the
        // deadline probes again; that probe still finds no agent and the
        // absence is published.
        let token = needs_probe(detector.begin(tick(deadline)));
        let (_change, step) = token.resume(&mut detector, &probe(None));
        let released = step_done(step);
        let update = released
            .state_changed
            .expect("absence publishes without a core read");
        assert_eq!(update.agent, None);
        assert_eq!(update.detection.state(), AgentState::Unknown);
    }

    #[test]
    fn tick_acquisition_grace_cache_and_authority_share_one_transition_path() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        let token = needs_probe(detector.begin(tick(now)));
        let (change, step) = token.resume(&mut detector, &probe(Some(Agent::Claude)));
        assert_eq!(change.process_detected, Some(Agent::Claude));
        let acquired = step_done(step);
        assert!(acquired.state_changed.is_none());

        let ready = now + AGENT_STARTUP_GRACE_WINDOW;
        let step = match detector.begin(tick(ready)) {
            Tick::NeedsProbe(token) => token.resume(&mut detector, &probe(Some(Agent::Claude))).1,
            Tick::NeedsScreen(screen) => Step::NeedsScreen(screen),
            Tick::Done(output) => Step::Done(output),
        };
        let Step::NeedsScreen(screen) = step else {
            panic!("an identified agent past its grace needs the screen");
        };
        let result = screen.resume(
            &mut detector,
            Some(&AgentDetectionInputs {
                screen_text: "* Waiting for 1 background agent to finish".into(),
                ..Default::default()
            }),
        );
        assert_eq!(
            result
                .state_changed
                .expect("working report")
                .detection
                .state(),
            AgentState::Working
        );
        let cached = done(detector.begin(tick(ready + Duration::from_millis(1))));
        assert!(cached.state_changed.is_none());
        let mut authority = tick(ready + Duration::from_millis(2));
        authority.lifecycle_authority_active = true;
        authority.content_seq = 2;
        let authoritative = done(detector.begin(authority));
        assert!(authoritative.state_changed.is_none());
    }

    #[test]
    fn failed_screen_read_does_not_publish_empty_screen_evidence() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Claude));
        detector.last_published = Some(Detection::Working { visible: false });
        detector.last_screen_scan_detection_content_seq = Some(1);
        detector.scheduler.probe_started(now);
        detector.scheduler.last_foreground_group = Some(pgid(25));

        let mut screen_tick = tick(now + Duration::from_millis(1));
        screen_tick.agent_changed = true;
        let screen = match detector.begin(screen_tick) {
            Tick::NeedsScreen(screen) => screen,
            Tick::Done(_) | Tick::NeedsProbe(_) => panic!("expected a screen read"),
        };

        let output = screen.resume(&mut detector, None);

        assert!(output.state_changed.is_none());
        assert_eq!(
            detector.last_published,
            Some(Detection::Working { visible: false })
        );
        assert_eq!(detector.last_screen_scan_detection_content_seq, None);
    }

    #[test]
    fn tick_confirmed_misses_publish_exit_before_identity_withdrawal() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));
        for attempt in 1..=AGENT_MISS_CONFIRMATION_ATTEMPTS {
            let at = now + PROCESS_RECHECK_IDENTIFIED * u32::from(attempt);
            let token = needs_probe(detector.begin(tick(at)));
            let (_change, step) = token.resume(&mut detector, &probe(None));
            let output = match step {
                Step::Done(output) => output,
                Step::NeedsScreen(screen) => {
                    screen.resume(&mut detector, Some(&Default::default()))
                }
            };
            if attempt == AGENT_MISS_CONFIRMATION_ATTEMPTS {
                let update = output.state_changed.expect("confirmed exit");
                assert_eq!(update.agent, Some(Agent::Pi));
                assert!(update.process_exited);
                assert_eq!(update.detection.state(), AgentState::Idle);
            } else {
                assert!(
                    output
                        .state_changed
                        .is_none_or(|update| !update.process_exited)
                );
            }
        }
        let at =
            now + PROCESS_RECHECK_IDENTIFIED * (u32::from(AGENT_MISS_CONFIRMATION_ATTEMPTS) + 1);
        let token = needs_probe(detector.begin(tick(at)));
        let (_change, step) = token.resume(&mut detector, &probe(None));
        let cleared = match step {
            Step::Done(output) => output,
            Step::NeedsScreen(screen) => screen.resume(&mut detector, Some(&Default::default())),
        };
        assert_eq!(detector.current_agent(), None);
        assert_eq!(
            cleared.state_changed.expect("withdraw identity").agent,
            None
        );
    }

    #[test]
    fn reset_keeps_process_evidence_for_the_next_lifecycle_probe() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Claude));
        detector.exit_phase = AgentExitPhase::ReportOwed {
            agent: Agent::Claude,
        };

        detector.reset();

        assert_eq!(detector.current_agent(), Some(Agent::Claude));
        // The exit still to report survives the reset.
        assert_eq!(
            detector.exit_phase,
            AgentExitPhase::ReportOwed {
                agent: Agent::Claude
            }
        );
        assert!(detector.process_exited());
        assert_eq!(detector.last_published, Some(Detection::Unknown));
        let mut authority = TickContext::new(now, Some(pgid(42)), 1, true, false);
        authority.lifecycle_authority_active = true;
        assert!(detector.schedule_process_probe(&authority).should_probe());
    }
}
