//! When deferred agent resumes may be attempted, as one pure value.
//!
//! `App` owns a [`ResumeSchedule`] and keeps the candidate walk and runtime
//! creation. The schedule decides *when*. Nothing here is stored twice: the
//! loop wakeup is derived by [`ResumeSchedule::wakeup`] from the two instants
//! the schedule does keep, so no pass that merely runs (a geometry change with
//! resume starting disabled, a loop iteration) can clear or restart anything.
//!
//! State is either "no pending plans" or "pending" with
//!
//! - `not_before`: a barrier before which nothing is attempted (the spacing
//!   after a launch);
//! - `theme_wait_until`: the deadline to use the restored host theme as a
//!   fallback, set the first time candidates become eligible and never
//!   restarted while plans stay pending. A live host color report bypasses it.
//!
//! A live host color report bypasses the theme wait but never a barrier.

use std::time::{Duration, Instant};

/// How one resume attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    /// Its shell launch was dispatched. Spaces the next launch out, from the
    /// dispatch: the launch itself settles later, possibly out of order.
    Launched,
    /// The plan was consumed without starting an agent (PTY could not be
    /// opened, empty argv, missing launch env). Nothing to space out.
    Abandoned,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    not_before: Option<Instant>,
    theme_wait_until: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResumeSchedule {
    theme_wait: Duration,
    spacing: Duration,
    pending: Option<Pending>,
}

impl ResumeSchedule {
    /// `theme_wait`: how long the first eligible candidates wait for live host
    /// colors before using the restored theme as fallback. `spacing`: the gap
    /// after a launch (zero launches every eligible candidate in one pass).
    pub(crate) fn new(theme_wait: Duration, spacing: Duration) -> Self {
        Self {
            theme_wait,
            spacing,
            pending: None,
        }
    }

    /// Records what the app reports at the start of a pass: whether any plan is
    /// pending at all and whether any candidate is eligible (workspace laid
    /// out, pane in layout, no runtime, unconsumed plan). No pending plans
    /// resets the schedule, which is how work that disappeared without a
    /// launch (a pane or workspace removed) stops holding a barrier. The theme
    /// wait starts the first time candidates are eligible and is kept after.
    pub(crate) fn observe(&mut self, now: Instant, has_pending_plans: bool, eligible: bool) {
        if !has_pending_plans {
            self.pending = None;
            return;
        }
        let theme_wait = self.theme_wait;
        let pending = self.pending.get_or_insert(Pending {
            not_before: None,
            theme_wait_until: None,
        });
        if eligible && pending.theme_wait_until.is_none() {
            pending.theme_wait_until = Some(now + theme_wait);
        }
    }

    /// When the loop should wake for a resume: `None` while nothing is
    /// eligible (the barrier is kept for when something is) or nothing holds
    /// an eligible candidate back, otherwise the later future deadline. An
    /// expired deadline is omitted so the loop does not spin on it.
    pub(crate) fn wakeup(
        &self,
        now: Instant,
        eligible: bool,
        live_theme_reported: bool,
    ) -> Option<Instant> {
        if !eligible {
            return None;
        }
        let pending = self.pending?;
        let barrier = pending.not_before.filter(|deadline| *deadline > now);
        let theme_wait = if live_theme_reported {
            None
        } else {
            pending.theme_wait_until.filter(|deadline| *deadline > now)
        };
        barrier.max(theme_wait)
    }

    /// Whether an attempt may run now: candidates are eligible and neither the
    /// barrier nor the theme wait still holds them back.
    pub(crate) fn is_due(&self, now: Instant, eligible: bool, live_theme_reported: bool) -> bool {
        eligible
            && self.pending.is_some()
            && self
                .wakeup(now, eligible, live_theme_reported)
                .is_none_or(|wakeup| now >= wakeup)
    }

    /// Starts a pass over the candidates; feed each attempt's outcome to
    /// [`ResumePass::record`] and hand the pass back to [`Self::finish`].
    pub(crate) fn begin_pass(&self, now: Instant) -> ResumePass {
        ResumePass {
            now,
            spacing: self.spacing,
            barrier: None,
            changed: false,
        }
    }

    /// Applies the barrier a pass produced. A pass runs only once the previous
    /// barrier has passed, so the new one replaces it.
    pub(crate) fn finish(&mut self, pass: &ResumePass) {
        if let (Some(pending), Some(barrier)) = (self.pending.as_mut(), pass.barrier) {
            pending.not_before = Some(barrier);
        }
    }
}

/// The barrier a candidate walk accumulates: the max of what its attempts
/// produced.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResumePass {
    now: Instant,
    spacing: Duration,
    barrier: Option<Instant>,
    changed: bool,
}

impl ResumePass {
    /// Records one attempt and says whether the walk goes on. It continues
    /// past abandonments, and stops after a launch when spacing is nonzero.
    pub(crate) fn record(&mut self, outcome: AttemptOutcome) -> bool {
        match outcome {
            AttemptOutcome::Launched => {
                self.changed = true;
                if self.spacing.is_zero() {
                    true
                } else {
                    self.raise_barrier(self.now + self.spacing);
                    false
                }
            }
            AttemptOutcome::Abandoned => {
                self.changed = true;
                true
            }
        }
    }

    /// Whether some attempt consumed its plan (launched or abandoned).
    pub(crate) fn changed(&self) -> bool {
        self.changed
    }

    fn raise_barrier(&mut self, barrier: Instant) {
        self.barrier = Some(self.barrier.map_or(barrier, |current| current.max(barrier)));
    }
}

#[cfg(test)]
impl ResumeSchedule {
    pub(crate) fn not_before(&self) -> Option<Instant> {
        self.pending.and_then(|pending| pending.not_before)
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THEME_WAIT: Duration = Duration::from_millis(750);

    fn schedule(spacing_ms: u64) -> ResumeSchedule {
        ResumeSchedule::new(THEME_WAIT, Duration::from_millis(spacing_ms))
    }

    /// Runs one pass over candidates whose outcomes are `outcomes` (the first
    /// attempted first), the way `App` walks them. Returns how many were
    /// attempted.
    fn run(schedule: &mut ResumeSchedule, now: Instant, outcomes: &[AttemptOutcome]) -> usize {
        let mut pass = schedule.begin_pass(now);
        let mut attempted = 0;
        for outcome in outcomes {
            attempted += 1;
            if !pass.record(*outcome) {
                break;
            }
        }
        schedule.finish(&pass);
        attempted
    }

    #[test]
    fn a_walk_continues_past_abandonments_to_a_launch() {
        let mut schedule = schedule(100);
        let now = Instant::now();
        schedule.observe(now, true, true);
        let attempted = run(
            &mut schedule,
            now + THEME_WAIT,
            &[
                AttemptOutcome::Abandoned,
                AttemptOutcome::Launched,
                AttemptOutcome::Launched,
            ],
        );
        assert_eq!(attempted, 2);
        assert_eq!(
            schedule.not_before(),
            Some(now + THEME_WAIT + Duration::from_millis(100))
        );
    }

    #[test]
    fn a_launch_spaces_the_next_attempt_out() {
        let mut schedule = schedule(250);
        let start = Instant::now();
        schedule.observe(start, true, true);
        let now = start + THEME_WAIT;
        let attempted = run(&mut schedule, now, &[AttemptOutcome::Launched; 3]);
        assert_eq!(attempted, 1, "the walk stops after a launch");
        let barrier = now + Duration::from_millis(250);
        assert_eq!(schedule.not_before(), Some(barrier));
        schedule.observe(now, true, true);
        assert_eq!(schedule.wakeup(start, true, true), Some(barrier));
        assert!(!schedule.is_due(barrier - Duration::from_millis(1), true, true));
        assert!(schedule.is_due(barrier, true, true));
    }

    #[test]
    fn zero_spacing_launches_every_candidate_in_one_pass() {
        let mut schedule = schedule(0);
        let start = Instant::now();
        schedule.observe(start, true, true);
        let attempted = run(&mut schedule, start, &[AttemptOutcome::Launched; 3]);
        assert_eq!(attempted, 3);
        assert_eq!(schedule.not_before(), None);
    }

    #[test]
    fn an_abandonment_sets_no_spacing() {
        let mut schedule = schedule(250);
        let start = Instant::now();
        schedule.observe(start, true, true);
        let now = start + THEME_WAIT;
        let attempted = run(&mut schedule, now, &[AttemptOutcome::Abandoned; 3]);
        assert_eq!(attempted, 3);
        assert_eq!(schedule.not_before(), None);
        assert!(schedule.is_due(now, true, false));
        // The pass that abandoned something reports it.
        let mut pass = schedule.begin_pass(now);
        assert!(!pass.changed());
        assert!(pass.record(AttemptOutcome::Abandoned));
        assert!(pass.changed());
    }

    #[test]
    fn repeated_passes_do_not_restart_the_theme_wait() {
        let mut schedule = schedule(100);
        let start = Instant::now();
        schedule.observe(start, true, true);
        let deadline = start + THEME_WAIT;
        assert_eq!(schedule.wakeup(start, true, false), Some(deadline));
        // A loop that keeps running (any pane printing) observes on every
        // iteration; none of them moves the wait.
        for step in 1..7 {
            let now = start + Duration::from_millis(step * 100);
            schedule.observe(now, true, true);
            assert_eq!(schedule.wakeup(now, true, false), Some(deadline));
            assert_eq!(schedule.is_due(now, true, false), now >= deadline);
        }
    }

    #[test]
    fn a_live_theme_report_bypasses_the_wait_but_not_a_barrier() {
        let mut schedule = schedule(250);
        let start = Instant::now();
        schedule.observe(start, true, true);
        assert!(!schedule.is_due(start, true, false));
        assert!(
            schedule.is_due(start, true, true),
            "a live theme report ends the wait"
        );
        run(&mut schedule, start, &[AttemptOutcome::Launched]);
        let barrier = start + Duration::from_millis(250);
        assert!(!schedule.is_due(start, true, true));
        assert_eq!(schedule.wakeup(start, true, true), Some(barrier));
    }

    #[test]
    fn no_wakeup_while_nothing_is_eligible_and_the_barrier_is_kept() {
        let mut schedule = schedule(250);
        let start = Instant::now();
        schedule.observe(start, true, true);
        run(&mut schedule, start, &[AttemptOutcome::Launched]);
        let barrier = start + Duration::from_millis(250);

        // Plans are pending but none is eligible (workspace not laid out).
        schedule.observe(start, true, false);
        assert_eq!(schedule.wakeup(start, false, true), None);
        assert_eq!(schedule.wakeup(start, false, false), None);
        assert!(!schedule.is_due(barrier, false, true));

        // Something becomes eligible again: the barrier still holds.
        schedule.observe(start, true, true);
        assert_eq!(schedule.wakeup(start, true, true), Some(barrier));
    }

    #[test]
    fn the_theme_wait_starts_when_candidates_first_become_eligible() {
        let mut schedule = schedule(0);
        let start = Instant::now();
        schedule.observe(start, true, false);
        assert_eq!(schedule.wakeup(start, true, false), None);
        let later = start + Duration::from_secs(5);
        schedule.observe(later, true, true);
        assert_eq!(
            schedule.wakeup(later, true, false),
            Some(later + THEME_WAIT)
        );
        // Never restarted, even across a stretch with nothing eligible.
        schedule.observe(later + Duration::from_secs(1), true, false);
        schedule.observe(later + Duration::from_secs(2), true, true);
        assert_eq!(
            schedule.wakeup(later + Duration::from_secs(2), true, false),
            None
        );
    }

    #[test]
    fn no_pending_plans_resets_the_schedule() {
        let mut schedule = schedule(250);
        let start = Instant::now();
        schedule.observe(start, true, true);
        run(&mut schedule, start, &[AttemptOutcome::Launched]);
        assert!(schedule.not_before().is_some());

        // The pane went away without a launch.
        schedule.observe(start, false, false);
        assert!(!schedule.is_pending());
        assert_eq!(schedule.wakeup(start, true, false), None);

        // New plans start from scratch: fresh theme wait, no old barrier.
        let later = start + Duration::from_secs(1);
        schedule.observe(later, true, true);
        assert_eq!(schedule.not_before(), None);
        assert_eq!(
            schedule.wakeup(later, true, false),
            Some(later + THEME_WAIT)
        );
    }
}
