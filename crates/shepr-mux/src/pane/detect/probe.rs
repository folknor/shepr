//! What a process probe means for the detector: agent presence with its miss
//! confirmation, the exit owed to the server before an identity is withdrawn,
//! and the resulting identity change.

use super::schedule::{ProbeFinding, ProbeScheduleDecision};
use super::state::{DetectorState, TickContext};
use crate::limits::{
    AGENT_MISS_CONFIRMATION_ATTEMPTS, AGENT_STARTUP_GRACE_WINDOW, PROCESS_RECHECK_ACTIVE_AGENT,
    PROCESS_RECHECK_IDENTIFIED,
};
use crate::pane::process_probe::ProcessProbeResult;
use shepr_agent::Agent;
use shepr_detect::Detection;
use shepr_platform::{Pgid, ProcessInstance};

// A relaunch's session start can reach the server before the probe that sees
// the new process group. Ownership holds such a start until this module
// reports the old process's exit and then the replacement's presence, each
// within a window of the one before. The exit comes from the first probe after
// the group change: the next tick when the foreground group can be read, the
// identified-process recheck when it cannot. The presence comes from the probe
// of the tick after the exit is reported, which runs because that exit is
// still to be withdrawn.
const _: () = assert!(
    shepr_detect::ownership::REPLACEMENT_START_EXIT_WINDOW.as_millis()
        >= PROCESS_RECHECK_IDENTIFIED.as_millis() + PROCESS_RECHECK_ACTIVE_AGENT.as_millis()
);
const _: () = assert!(
    shepr_detect::ownership::REPLACEMENT_START_PRESENCE_GAP.as_millis()
        >= 2 * PROCESS_RECHECK_ACTIVE_AGENT.as_millis()
);

#[derive(Debug, Clone, Copy)]
pub(super) struct AgentDetectionPresence {
    current_agent: Option<Agent>,
    consecutive_misses: u8,
    identified_group: Option<Pgid>,
    /// The process incarnation (pid and start time) the agent was last
    /// recognized from. A background process holds the identity only if it is
    /// this one, so a new process of the same kind is a replacement.
    identified_process: Option<ProcessInstance>,
}

impl AgentDetectionPresence {
    pub(super) fn from_agent(current_agent: Option<Agent>) -> Self {
        Self {
            current_agent,
            consecutive_misses: 0,
            identified_group: None,
            identified_process: None,
        }
    }

    pub(super) fn current_agent(&self) -> Option<Agent> {
        self.current_agent
    }

    pub(super) fn identified_process(&self) -> Option<ProcessInstance> {
        self.identified_process
    }

    pub(super) fn clear_current_agent(&mut self) -> bool {
        if self.current_agent.is_none() {
            self.consecutive_misses = 0;
            return false;
        }
        self.current_agent = None;
        self.identified_group = None;
        self.identified_process = None;
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
                self.identified_group = None;
                self.identified_process = None;
                self.consecutive_misses = 0;
                true
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundShellAgentAction {
    ObserveProbe,
    ReportProcessExit,
    ReportReplacementProcess,
    ClearAgent,
    /// The identified agent process still lives outside the foreground,
    /// stopped by job control or running in the background, and still owns
    /// the agent identity and session.
    HeldInBackground,
}

#[derive(Debug, Clone, Copy)]
struct ForegroundShellProbe {
    previous_agent: Option<Agent>,
    identified_agent: Option<Agent>,
    foreground_is_pane_shell: bool,
    process_exit_reported: bool,
}

fn foreground_shell_agent_action_with_background_agent(
    probe: ForegroundShellProbe,
    held_in_background: bool,
) -> ForegroundShellAgentAction {
    let Some(previous_agent) = probe.previous_agent else {
        return ForegroundShellAgentAction::ObserveProbe;
    };
    // Whatever holds the foreground (the shell, vim, a build), the agent that
    // is not in it but alive keeps its identity. An agent in front is not
    // held: it is observed, so a different one replaces the identity.
    if held_in_background && probe.identified_agent.is_none() {
        return ForegroundShellAgentAction::HeldInBackground;
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

/// An exit is reported against its identity before that identity is withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentExitPhase {
    Observing,
    ReportOwed { agent: Agent },
    ClearOwed { agent: Agent },
}

impl AgentExitPhase {
    pub(super) fn agent(self) -> Option<Agent> {
        match self {
            Self::Observing => None,
            Self::ReportOwed { agent } | Self::ClearOwed { agent } => Some(agent),
        }
    }

    pub(super) fn clear_pending(self) -> bool {
        !matches!(self, Self::Observing)
    }

    fn reported(self) -> bool {
        matches!(self, Self::ClearOwed { .. })
    }

    pub(super) fn report(&mut self) {
        if let Self::ReportOwed { agent } = *self {
            *self = Self::ClearOwed { agent };
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::pane) struct AgentProcessChange {
    pub(in crate::pane) previous_agent: Option<Agent>,
    pub(in crate::pane) agent: Option<Agent>,
    pub(in crate::pane) process_name: Option<String>,
    pub(in crate::pane) process_group_id: Option<Pgid>,
    pub(in crate::pane) agent_changed: bool,
    pub(in crate::pane) should_clear_osc_evidence: bool,
    pub(in crate::pane) process_detected: Option<Agent>,
}

impl DetectorState {
    pub(super) fn observe_process_probe(
        &mut self,
        tick: &TickContext,
        probe: &ProcessProbeResult,
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
            process_exit_reported: self.exit_phase.reported(),
        };
        // Held only by the very process that was identified (pid and start
        // time): a background process of the same kind that is not it, or a
        // pid reused since, leaves the identity to the miss confirmation.
        let held_in_background = identified_agent.is_none()
            && previous_agent
                .zip(self.agent_presence.identified_process)
                .is_some_and(|(agent, process)| {
                    probe
                        .background_agents
                        .iter()
                        .any(|held| held.agent == agent && held.process == process)
                });
        let mut action =
            foreground_shell_agent_action_with_background_agent(shell_probe, held_in_background);
        // Production probes identify agents only from a ForegroundJob, which
        // always supplies its group, even when the PTY foreground-group read
        // failed. A probe with no group is unidentified, not evidence of an
        // agent relaunch. Keep the optional guards for missing snapshots.
        // Compare with the last identified agent group, not the scheduler's
        // foreground group: a suspended job yields the terminal to its shell
        // and resumes in its original group. A new group naming the same agent
        // is a new process even when the shell ran between probes unseen.
        if action == ForegroundShellAgentAction::ObserveProbe
            && identified_agent.is_some()
            && identified_agent == previous_agent
            && self.agent_presence.identified_group.is_some()
            && process_group_id.is_some()
            && self.agent_presence.identified_group != process_group_id
        {
            action = ForegroundShellAgentAction::ReportProcessExit;
        }
        let agent_changed = match action {
            ForegroundShellAgentAction::ReportReplacementProcess => {
                self.exit_phase = AgentExitPhase::Observing;
                self.agent_presence.observe_process_probe(previous_agent);
                true
            }
            ForegroundShellAgentAction::ReportProcessExit => {
                if let Some(agent) = previous_agent {
                    self.exit_phase = AgentExitPhase::ReportOwed { agent };
                }
                false
            }
            ForegroundShellAgentAction::ClearAgent => {
                let had_exit = self.exit_phase.clear_pending();
                self.exit_phase = AgentExitPhase::Observing;
                self.agent_presence.clear_current_agent() || had_exit
            }
            ForegroundShellAgentAction::ObserveProbe => {
                // Absence cannot cancel an exit that has not reached the app.
                // Once reported, withdraw the carried identity on the next probe.
                if identified_agent.is_none() && self.exit_phase.clear_pending() {
                    if self.exit_phase.reported() {
                        self.exit_phase = AgentExitPhase::Observing;
                        self.agent_presence.clear_current_agent();
                        true
                    } else {
                        false
                    }
                } else {
                    self.exit_phase = AgentExitPhase::Observing;
                    let changed = self.agent_presence.observe_process_probe(identified_agent);
                    if changed && identified_agent.is_none() {
                        if let Some(agent) = previous_agent {
                            self.exit_phase = AgentExitPhase::ReportOwed { agent };
                        }
                        false
                    } else {
                        changed
                    }
                }
            }
            ForegroundShellAgentAction::HeldInBackground => {
                self.exit_phase = AgentExitPhase::Observing;
                if let Some(agent) = previous_agent {
                    self.agent_presence.observe_process_probe(Some(agent));
                }
                false
            }
        };
        if identified_agent.is_some()
            && matches!(
                action,
                ForegroundShellAgentAction::ObserveProbe
                    | ForegroundShellAgentAction::ReportReplacementProcess
            )
            && (process_group_id.is_some() || agent_changed)
        {
            self.agent_presence.identified_group = process_group_id;
            self.agent_presence.identified_process = probe.agent_process();
        }
        let agent = self.current_agent();
        self.scheduler.probe_completed(
            tick,
            schedule,
            ProbeFinding {
                probed_process_group: process_group_id,
                identified_agent,
                current_agent: agent,
            },
        );

        let replacement = action == ForegroundShellAgentAction::ReportReplacementProcess;
        let should_reset_detection = agent_changed && (agent != previous_agent || replacement);
        if should_reset_detection {
            self.pending_idle.clear();
            self.last_screen_scan_detection_content_seq = None;
            self.last_screen_detection = None;
            if agent.is_some() {
                self.agent_absence_hold_until = None;
                self.agent_startup_grace_until = Some(tick.now + AGENT_STARTUP_GRACE_WINDOW);
                self.last_published = self.last_published.map(|_| Detection::Unknown);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::launch::LaunchKind;
    use crate::pane::process_probe::ProcessProbeIdentity;
    use shepr_agent::AgentState;
    use shepr_detect::BackgroundAgent;
    use std::time::{Duration, Instant};

    fn pgid(value: u32) -> Pgid {
        Pgid::new(value).expect("test process group")
    }

    fn tick(now: Instant, group: u32) -> TickContext {
        TickContext::new(now, Some(pgid(group)), 1, false, false)
    }

    fn probe_result(
        group: u32,
        is_shell: bool,
        background: Vec<BackgroundAgent>,
        identity: ProcessProbeIdentity,
    ) -> ProcessProbeResult {
        ProcessProbeResult {
            process_group_id: Pgid::new(group),
            foreground_is_pane_shell: is_shell,
            background_agents: background,
            identity,
        }
    }

    fn instance(pid: u32, start_ticks: u64) -> ProcessInstance {
        ProcessInstance {
            pid: shepr_platform::Pid::new(pid).expect("test pid"),
            start_ticks,
        }
    }

    /// The process every test agent is identified from unless it says otherwise.
    fn agent_process() -> ProcessInstance {
        instance(500, 7)
    }

    fn identified(agent: Agent) -> ProcessProbeIdentity {
        ProcessProbeIdentity::Agent {
            agent,
            process_name: agent.label().to_string(),
            process: agent_process(),
        }
    }

    fn background(agent: Agent, process: ProcessInstance) -> Vec<BackgroundAgent> {
        vec![BackgroundAgent { agent, process }]
    }

    fn probe_decision(foreground_group_changed: bool) -> ProbeScheduleDecision {
        ProbeScheduleDecision::Probe {
            foreground_group_changed,
            had_previous_probe: true,
        }
    }

    /// The action for a probe with no agent held in the background, as these
    /// tests exercise the foreground-shell rules on their own.
    fn foreground_shell_agent_action(probe: ForegroundShellProbe) -> ForegroundShellAgentAction {
        foreground_shell_agent_action_with_background_agent(probe, false)
    }

    fn shell_probe(
        previous_agent: Option<Agent>,
        identified_agent: Option<Agent>,
        foreground_is_pane_shell: bool,
        process_exit_reported: bool,
    ) -> ForegroundShellProbe {
        ForegroundShellProbe {
            previous_agent,
            identified_agent,
            foreground_is_pane_shell,
            process_exit_reported,
        }
    }

    #[test]
    fn foreground_shell_reports_process_exit_before_clearing_agent() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(Some(Agent::Codex), None, true, false)),
            ForegroundShellAgentAction::ReportProcessExit
        );
        assert_eq!(
            foreground_shell_agent_action(shell_probe(Some(Agent::Codex), None, true, true)),
            ForegroundShellAgentAction::ClearAgent
        );
    }

    #[test]
    fn same_agent_after_reported_exit_is_a_replacement_process() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(
                Some(Agent::Pi),
                Some(Agent::Pi),
                false,
                true,
            )),
            ForegroundShellAgentAction::ReportReplacementProcess
        );
    }

    #[test]
    fn unknown_non_shell_foreground_job_is_not_immediate_clear_signal() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(Some(Agent::Claude), None, false, false,)),
            ForegroundShellAgentAction::ObserveProbe
        );
    }

    #[test]
    fn reported_process_exit_clears_before_unknown_foreground_probe() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(Some(Agent::Claude), None, false, true,)),
            ForegroundShellAgentAction::ClearAgent
        );
    }

    #[test]
    fn foreground_agent_job_is_not_clear_signal() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(
                Some(Agent::Claude),
                Some(Agent::OpenCode),
                true,
                false,
            )),
            ForegroundShellAgentAction::ObserveProbe
        );
    }

    #[test]
    fn transient_process_miss_keeps_current_agent_detected() {
        let mut presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));

        let changed = presence.observe_process_probe(None);

        assert!(!changed, "one miss should not clear the detected agent");
        assert_eq!(presence.current_agent(), Some(Agent::Pi));
    }

    #[test]
    fn agent_only_clears_after_confirmation_misses() {
        let mut presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));

        for attempt in 1..AGENT_MISS_CONFIRMATION_ATTEMPTS {
            let changed = presence.observe_process_probe(None);
            assert!(
                !changed,
                "miss {attempt} should stay in the confirmation window"
            );
            assert_eq!(presence.current_agent(), Some(Agent::Pi));
        }

        let changed = presence.observe_process_probe(None);
        assert!(changed, "last confirmation miss should clear the agent");
        assert_eq!(presence.current_agent(), None);
    }

    #[test]
    fn detector_state_accepts_process_identity_without_a_runtime() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        let tick = tick(now, 25);
        let schedule = detector.schedule_process_probe(&tick);
        assert!(schedule.should_probe());
        detector.scheduler.probe_started(now);

        let probe = probe_result(25, false, Vec::new(), identified(Agent::Claude));
        let change = detector.observe_process_probe(&tick, &probe, schedule);

        assert_eq!(change.agent, Some(Agent::Claude));
        assert_eq!(change.process_detected, Some(Agent::Claude));
        assert!(!change.should_clear_osc_evidence);
        assert_eq!(detector.identified_process(), Some(agent_process()));
    }

    #[test]
    fn reset_does_not_rereport_an_exit_already_reported() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(None);
        detector.exit_phase = AgentExitPhase::ClearOwed { agent: Agent::Pi };

        detector.reset();

        assert_eq!(detector.current_agent(), Some(Agent::Pi));
        assert_eq!(
            detector.exit_phase,
            AgentExitPhase::ClearOwed { agent: Agent::Pi }
        );
        assert!(!detector.process_exited());
        // Presence is not rebuilt from the exited identity.
        assert_eq!(detector.agent_presence.current_agent(), None);
        let probe = probe_result(25, true, Vec::new(), ProcessProbeIdentity::Unidentified);
        detector.observe_process_probe(
            &tick(now + Duration::from_secs(1), 25),
            &probe,
            probe_decision(false),
        );
        assert!(!detector.process_exited());
    }

    #[test]
    fn confirmed_process_misses_publish_exit_before_clearing_identity() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));
        let probe = probe_result(25, false, Vec::new(), ProcessProbeIdentity::Unidentified);

        for attempt in 1..=AGENT_MISS_CONFIRMATION_ATTEMPTS {
            let change = detector.observe_process_probe(
                &tick(now + Duration::from_secs(u64::from(attempt)), 25),
                &probe,
                probe_decision(false),
            );
            if attempt < AGENT_MISS_CONFIRMATION_ATTEMPTS {
                assert!(!detector.process_exited());
            } else {
                assert!(detector.process_exited());
            }
            assert!(!change.agent_changed);
            assert_eq!(detector.current_agent(), Some(Agent::Pi));
        }
    }

    #[test]
    fn command_after_confirmed_misses_preserves_exit_then_withdraws_identity() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));
        let command = probe_result(25, false, Vec::new(), ProcessProbeIdentity::Unidentified);
        for _ in 0..AGENT_MISS_CONFIRMATION_ATTEMPTS {
            detector.observe_process_probe(&tick(now, 25), &command, probe_decision(false));
        }
        assert!(detector.process_exited());
        let command = probe_result(26, false, Vec::new(), ProcessProbeIdentity::Unidentified);
        let owed = detector.observe_process_probe(&tick(now, 26), &command, probe_decision(false));
        assert!(!owed.agent_changed);
        assert_eq!(owed.agent, Some(Agent::Pi));
        assert!(detector.process_exited());
        let mut owed_tick = tick(now, 26);
        owed_tick.agent_changed = owed.agent_changed;
        let update = detector
            .complete_screen(&owed_tick, &Default::default())
            .expect("owed exit must publish");
        assert_eq!(update.agent, Some(Agent::Pi));
        assert!(update.process_exited);
        assert_eq!(update.detection.state(), AgentState::Idle);
        let cleared =
            detector.observe_process_probe(&tick(now, 26), &command, probe_decision(false));
        assert!(cleared.agent_changed);
        assert_eq!(cleared.agent, None);
        assert!(!detector.process_exited());
    }

    #[test]
    fn same_agent_in_new_group_reports_exit_then_replacement() {
        for agent in [Agent::Claude, Agent::Pi, Agent::Codex] {
            let now = Instant::now();
            let mut detector = DetectorState::new(now, LaunchKind::Fresh);
            let probe = |group| probe_result(group, false, Vec::new(), identified(agent));
            detector.observe_process_probe(&tick(now, 25), &probe(25), probe_decision(true));
            let unchanged = detector.observe_process_probe(
                &tick(now + Duration::from_millis(1), 25),
                &probe(25),
                probe_decision(false),
            );
            assert!(!unchanged.agent_changed);
            let exited_at = now + Duration::from_millis(2);
            let exit = detector.observe_process_probe(
                &tick(exited_at, 26),
                &probe(26),
                probe_decision(true),
            );
            assert_eq!(exit.process_detected, None);
            assert!(detector.process_exited());
            let update = detector
                .complete_screen(&tick(exited_at, 26), &Default::default())
                .expect("replacement owes an exit before presence");
            assert_eq!(update.agent, Some(agent));
            assert!(update.process_exited);
            let replacement_at = exited_at + Duration::from_millis(1);
            let replacement = detector.observe_process_probe(
                &tick(replacement_at, 26),
                &probe(26),
                probe_decision(false),
            );
            assert_eq!(replacement.process_detected, Some(agent));
            assert!(replacement.agent_changed);
            assert!(replacement.should_clear_osc_evidence);
            assert!(!detector.process_exited());
        }
    }

    #[test]
    fn suspended_agent_is_not_reported_as_a_process_exit_or_replacement() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Claude));
        detector.agent_presence.identified_group = Some(pgid(27));
        detector.agent_presence.identified_process = Some(agent_process());
        let suspended_probe = probe_result(
            25,
            true,
            background(Agent::Claude, agent_process()),
            ProcessProbeIdentity::Unidentified,
        );

        let change =
            detector.observe_process_probe(&tick(now, 25), &suspended_probe, probe_decision(true));

        assert_eq!(detector.current_agent(), Some(Agent::Claude));
        assert!(!detector.process_exited());
        assert!(!change.agent_changed);
        assert_eq!(change.process_detected, None);

        let resumed_probe = probe_result(27, false, Vec::new(), identified(Agent::Claude));
        let resumed = detector.observe_process_probe(
            &tick(now + Duration::from_secs(1), 27),
            &resumed_probe,
            probe_decision(true),
        );

        assert_eq!(detector.current_agent(), Some(Agent::Claude));
        assert!(!resumed.agent_changed);
        assert_eq!(resumed.process_detected, None);
    }

    /// A detector that identified `Agent::Claude` from `agent_process()` in
    /// group 27 and has seen it leave the foreground.
    fn detector_with_identified_claude(now: Instant) -> DetectorState {
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        let in_front = probe_result(27, false, Vec::new(), identified(Agent::Claude));
        detector.observe_process_probe(&tick(now, 27), &in_front, probe_decision(true));
        assert_eq!(detector.current_agent(), Some(Agent::Claude));
        assert_eq!(detector.identified_process(), Some(agent_process()));
        detector
    }

    #[test]
    fn stopped_agent_behind_a_non_shell_foreground_keeps_its_identity() {
        let now = Instant::now();
        let mut detector = detector_with_identified_claude(now);
        // Ctrl-Z, then vim in front: the foreground is a job of another group
        // that identifies as no agent, and the stopped agent is still alive.
        let behind_vim = probe_result(
            31,
            false,
            background(Agent::Claude, agent_process()),
            ProcessProbeIdentity::Unidentified,
        );

        for attempt in 1..=(3 * u64::from(AGENT_MISS_CONFIRMATION_ATTEMPTS)) {
            let change = detector.observe_process_probe(
                &tick(now + Duration::from_secs(attempt), 31),
                &behind_vim,
                probe_decision(attempt == 1),
            );
            assert!(!change.agent_changed, "probe {attempt}");
            assert_eq!(detector.current_agent(), Some(Agent::Claude));
            assert!(!detector.process_exited(), "probe {attempt}");
        }

        // `fg`: the same process is back in its own group, with no exit owed.
        let resumed = probe_result(27, false, Vec::new(), identified(Agent::Claude));
        let change = detector.observe_process_probe(
            &tick(now + Duration::from_secs(100), 27),
            &resumed,
            probe_decision(true),
        );
        assert!(!change.agent_changed);
        assert_eq!(change.process_detected, None);
        assert!(!detector.process_exited());
    }

    #[test]
    fn running_background_agent_keeps_its_identity_behind_the_shell() {
        let now = Instant::now();
        let mut detector = detector_with_identified_claude(now);
        // `bg` or `claude &`: the shell holds the foreground, the agent runs.
        let at_prompt = probe_result(
            25,
            true,
            background(Agent::Claude, agent_process()),
            ProcessProbeIdentity::Unidentified,
        );

        for attempt in 1..=(3 * u64::from(AGENT_MISS_CONFIRMATION_ATTEMPTS)) {
            detector.observe_process_probe(
                &tick(now + Duration::from_secs(attempt), 25),
                &at_prompt,
                probe_decision(attempt == 1),
            );
            assert_eq!(detector.current_agent(), Some(Agent::Claude));
            assert!(!detector.process_exited(), "probe {attempt}");
        }
    }

    #[test]
    fn background_process_of_another_identity_does_not_hold_the_session() {
        let now = Instant::now();
        let others = [
            // A reused pid: same pid, another start time.
            instance(500, 8),
            // Another process of the same kind.
            instance(501, 7),
        ];
        for other in others {
            let mut detector = detector_with_identified_claude(now);
            let behind_vim = probe_result(
                31,
                false,
                background(Agent::Claude, other),
                ProcessProbeIdentity::Unidentified,
            );

            for attempt in 1..=u64::from(AGENT_MISS_CONFIRMATION_ATTEMPTS) {
                detector.observe_process_probe(
                    &tick(now + Duration::from_secs(attempt), 31),
                    &behind_vim,
                    probe_decision(attempt == 1),
                );
            }

            assert!(
                detector.process_exited(),
                "{other:?} must not hold the identified process's session"
            );
        }
    }

    #[test]
    fn background_process_of_another_kind_does_not_hold_the_session() {
        let now = Instant::now();
        let mut detector = detector_with_identified_claude(now);
        let behind_vim = probe_result(
            31,
            false,
            background(Agent::Codex, agent_process()),
            ProcessProbeIdentity::Unidentified,
        );

        for attempt in 1..=u64::from(AGENT_MISS_CONFIRMATION_ATTEMPTS) {
            detector.observe_process_probe(
                &tick(now + Duration::from_secs(attempt), 31),
                &behind_vim,
                probe_decision(attempt == 1),
            );
        }

        assert!(detector.process_exited());
    }

    #[test]
    fn no_recorded_identity_means_nothing_can_hold_the_session() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        // An identity from before any process was recognized in this detector,
        // such as a resumed agent whose process has not been seen yet.
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Claude));
        let behind_vim = probe_result(
            31,
            false,
            background(Agent::Claude, agent_process()),
            ProcessProbeIdentity::Unidentified,
        );

        for attempt in 1..=u64::from(AGENT_MISS_CONFIRMATION_ATTEMPTS) {
            detector.observe_process_probe(
                &tick(now + Duration::from_secs(attempt), 31),
                &behind_vim,
                probe_decision(attempt == 1),
            );
        }

        assert!(detector.process_exited());
    }

    #[test]
    fn same_kind_replacement_is_not_hidden_by_the_held_process() {
        let now = Instant::now();
        let mut detector = detector_with_identified_claude(now);
        // The old claude is stopped behind a new claude in the foreground:
        // the foreground agent is observed, not the held one.
        let new_in_front = probe_result(
            33,
            false,
            background(Agent::Claude, agent_process()),
            ProcessProbeIdentity::Agent {
                agent: Agent::Claude,
                process_name: "claude".to_string(),
                process: instance(600, 9),
            },
        );

        detector.observe_process_probe(
            &tick(now + Duration::from_secs(1), 33),
            &new_in_front,
            probe_decision(true),
        );

        assert!(detector.process_exited(), "the old session is owed an exit");
    }
}
