//! When process probes run, and how foreground and content activity open an
//! acquisition window.

use std::time::Instant;

use super::state::TickContext;
use crate::limits::{
    PROCESS_ACQUISITION_FAST_RECHECK, PROCESS_ACQUISITION_FAST_WINDOW,
    PROCESS_ACQUISITION_IDLE_RESET, PROCESS_ACQUISITION_SLOW_RECHECK, PROCESS_ACQUISITION_WINDOW,
    PROCESS_RECHECK_IDENTIFIED, PROCESS_RECHECK_MISSING_FOREGROUND_GROUP,
};
use shepr_agent::Agent;
use shepr_platform::Pgid;

pub(super) fn foreground_group_changed(
    foreground_pgid: Option<Pgid>,
    last_foreground_pgid: Option<Pgid>,
) -> bool {
    foreground_pgid != last_foreground_pgid
        && (foreground_pgid.is_some() || last_foreground_pgid.is_some())
}

// Only kernel-observed foreground groups drive change detection. Remembering an
// inferred group would look like a change on every tick while the kernel stays silent.
fn process_group_for_change_tracking(
    observed_foreground_pgid: Option<Pgid>,
    probed_process_group_id: Option<Pgid>,
) -> Option<Pgid> {
    observed_foreground_pgid?;
    probed_process_group_id.or(observed_foreground_pgid)
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

    fn had_previous_probe(self) -> bool {
        matches!(
            self,
            Self::Probe {
                had_previous_probe: true,
                ..
            }
        )
    }
}

/// What the probe found, for the scheduler to learn from.
#[derive(Debug, Clone, Copy)]
pub(super) struct ProbeFinding {
    pub(super) probed_process_group: Option<Pgid>,
    pub(super) identified_agent: Option<Agent>,
    /// The detector's agent after it absorbed the probe.
    pub(super) current_agent: Option<Agent>,
}

/// Owns when process probes run and how foreground/content activity opens an
/// acquisition window. Callers provide only the observations for this tick.
#[derive(Debug)]
pub(super) struct ProcessProbeScheduler {
    last_check: Instant,
    last_foreground_group: Option<Pgid>,
    has_probe: bool,
    acquisition_started_at: Option<Instant>,
    last_content_change_at: Option<Instant>,
}

impl ProcessProbeScheduler {
    pub(super) fn new(now: Instant) -> Self {
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

    fn foreground_group_changed(&self, observed: Option<Pgid>) -> bool {
        foreground_group_changed(observed, self.last_foreground_group)
    }

    /// Decides whether this tick probes. `agent` is the identity the detector
    /// currently holds and `shell_clear_pending` says an exit still has to be
    /// reported and withdrawn.
    pub(super) fn schedule(
        &self,
        tick: &TickContext,
        agent: Option<Agent>,
        shell_clear_pending: bool,
    ) -> ProbeScheduleDecision {
        let group_changed = self.foreground_group_changed(tick.foreground_group);
        let elapsed_since_check = tick.now.duration_since(self.last_check);
        let acquisition_age = self
            .acquisition_started_at
            .map(|started| tick.now.duration_since(started));

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
            && !shell_clear_pending
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
        let should_probe = if shell_clear_pending || acquisition_due {
            true
        } else if agent.is_none() {
            !self.has_probe
                || group_changed
                || (tick.lifecycle_authority_active
                    && elapsed_since_check >= PROCESS_RECHECK_IDENTIFIED)
                || (tick.foreground_group.is_none()
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

    pub(super) fn probe_started(&mut self, now: Instant) {
        self.last_check = now;
        self.has_probe = true;
    }

    pub(super) fn probe_completed(
        &mut self,
        tick: &TickContext,
        schedule: ProbeScheduleDecision,
        finding: ProbeFinding,
    ) {
        self.last_foreground_group =
            process_group_for_change_tracking(tick.foreground_group, finding.probed_process_group);
        if finding.identified_agent.is_some() {
            self.acquisition_started_at = None;
            self.last_content_change_at = None;
        } else if finding.current_agent.is_none()
            && schedule.had_previous_probe()
            && schedule.foreground_group_changed()
        {
            self.acquisition_started_at = Some(tick.now);
        }
    }

    pub(super) fn content_changed(
        &mut self,
        now: Instant,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn pgid(value: u32) -> Pgid {
        Pgid::new(value).expect("test process group")
    }

    fn decide(
        scheduler: &ProcessProbeScheduler,
        now: Instant,
        agent: Option<Agent>,
        foreground_group: Option<Pgid>,
        lifecycle_authority_active: bool,
        shell_clear_pending: bool,
    ) -> ProbeScheduleDecision {
        let tick = TickContext::new(now, foreground_group, 0, lifecycle_authority_active, false);
        scheduler.schedule(&tick, agent, shell_clear_pending)
    }

    #[test]
    fn scheduler_probes_initially_then_waits_for_activity_or_safety_interval() {
        let now = Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        assert!(matches!(
            decide(&scheduler, now, None, None, false, false),
            ProbeScheduleDecision::Probe {
                had_previous_probe: false,
                ..
            }
        ));

        scheduler.probe_started(now);
        assert!(
            !decide(
                &scheduler,
                now + PROCESS_RECHECK_MISSING_FOREGROUND_GROUP - Duration::from_millis(1),
                None,
                None,
                false,
                false,
            )
            .should_probe()
        );
        assert!(
            decide(
                &scheduler,
                now + PROCESS_RECHECK_MISSING_FOREGROUND_GROUP,
                None,
                None,
                false,
                false,
            )
            .should_probe()
        );
    }

    #[test]
    fn scheduler_uses_foreground_changes_and_lifecycle_authority_together() {
        let now = Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.probe_started(now);
        scheduler.last_foreground_group = Some(pgid(42));
        let later = now + Duration::from_millis(300);

        assert!(matches!(
            decide(
                &scheduler,
                later,
                Some(Agent::Pi),
                Some(pgid(42)),
                true,
                false
            ),
            ProbeScheduleDecision::Skip {
                foreground_group_changed: false
            }
        ));
        assert!(
            decide(
                &scheduler,
                later,
                Some(Agent::Pi),
                Some(pgid(43)),
                true,
                false
            )
            .should_probe()
        );
        assert!(
            decide(
                &scheduler,
                later,
                Some(Agent::Pi),
                Some(pgid(42)),
                true,
                true
            )
            .should_probe()
        );
    }

    #[test]
    fn scheduler_keeps_identified_safety_probes_without_a_foreground_group() {
        let now = Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.probe_started(now);

        assert!(
            !decide(
                &scheduler,
                now + PROCESS_RECHECK_IDENTIFIED - Duration::from_millis(1),
                Some(Agent::Pi),
                None,
                true,
                false,
            )
            .should_probe()
        );
        assert!(
            decide(
                &scheduler,
                now + PROCESS_RECHECK_IDENTIFIED,
                Some(Agent::Pi),
                None,
                true,
                false,
            )
            .should_probe()
        );
    }

    #[test]
    fn lifecycle_authority_rechecks_identified_and_unidentified_processes_on_a_timer() {
        let now = Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.probe_started(now);
        scheduler.last_foreground_group = Some(pgid(42));

        assert!(
            !decide(
                &scheduler,
                now + PROCESS_RECHECK_IDENTIFIED - Duration::from_millis(1),
                Some(Agent::Pi),
                Some(pgid(42)),
                true,
                false,
            )
            .should_probe()
        );
        assert!(
            decide(
                &scheduler,
                now + PROCESS_RECHECK_IDENTIFIED,
                Some(Agent::Pi),
                Some(pgid(42)),
                true,
                false,
            )
            .should_probe()
        );

        let reacquisition_started = now + PROCESS_RECHECK_IDENTIFIED;
        scheduler.probe_started(reacquisition_started);
        assert!(
            !decide(
                &scheduler,
                reacquisition_started + PROCESS_RECHECK_IDENTIFIED - Duration::from_millis(1),
                None,
                Some(pgid(42)),
                true,
                false,
            )
            .should_probe()
        );
        assert!(
            decide(
                &scheduler,
                reacquisition_started + PROCESS_RECHECK_IDENTIFIED,
                None,
                Some(pgid(42)),
                true,
                false,
            )
            .should_probe()
        );
    }

    #[test]
    fn scheduler_rechecks_acquisition_quickly_and_resets_after_quiet() {
        let now = Instant::now();
        let mut scheduler = ProcessProbeScheduler::new(now);
        scheduler.content_changed(now, None, false, true);

        assert!(
            !decide(
                &scheduler,
                now + PROCESS_ACQUISITION_FAST_RECHECK - Duration::from_millis(1),
                None,
                None,
                false,
                false,
            )
            .should_probe()
        );
        assert!(
            decide(
                &scheduler,
                now + PROCESS_ACQUISITION_FAST_RECHECK,
                None,
                None,
                false,
                false,
            )
            .should_probe()
        );

        assert!(
            !decide(
                &scheduler,
                now + PROCESS_ACQUISITION_SLOW_RECHECK - Duration::from_millis(1),
                None,
                None,
                false,
                false,
            )
            .should_probe()
        );
        assert!(
            decide(
                &scheduler,
                now + PROCESS_ACQUISITION_SLOW_RECHECK,
                None,
                None,
                false,
                false,
            )
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
}
