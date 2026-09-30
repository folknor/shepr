//! When deferred agent resumes may be attempted, as one pure value.
//!
//! `App` owns a [`ResumeSchedule`] and keeps the candidate walk and runtime
//! creation. The schedule decides *when* and holds cwd check results until
//! their matching candidate is attempted. Nothing here is stored twice: the
//! loop wakeup is derived by [`ResumeSchedule::wakeup`] from the two instants
//! the schedule does keep, so no pass that merely runs (a geometry change with
//! resume starting disabled, a loop iteration) can clear or restart anything.
//!
//! State is either "no pending plans" or "pending" with
//!
//! - `not_before`: a barrier before which nothing is attempted (the spacing
//!   after a launch, the backoff after a retryable failure);
//! - `theme_wait_until`: how long a host theme is waited for, set the first
//!   time candidates become eligible and never restarted while plans stay
//!   pending.
//!
//! The theme arriving bypasses the theme wait but never a barrier.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use shepr_protocol::TerminalId;

/// How one resume attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    /// An agent was started. Spaces the next launch out.
    Launched,
    /// The plan was consumed without starting an agent (missing cwd, shell
    /// spawn failure, empty argv, missing launch env). Nothing to space out.
    Abandoned,
    /// The plan is kept and the attempt is repeated later (the resume command
    /// could not be queued to the shell). Backs the schedule off.
    Retryable,
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
    backoff: Duration,
    pending: Option<Pending>,
    directory_checks: HashMap<(TerminalId, PathBuf), bool>,
}

impl ResumeSchedule {
    /// `theme_wait`: how long the first eligible candidates wait for a host
    /// theme. `spacing`: the gap after a launch (zero launches every eligible
    /// candidate in one pass). `backoff`: the gap after a retryable failure.
    pub(crate) fn new(theme_wait: Duration, spacing: Duration, backoff: Duration) -> Self {
        Self {
            theme_wait,
            spacing,
            backoff,
            pending: None,
            directory_checks: HashMap::new(),
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
            self.directory_checks.clear();
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

    /// Saves the result of a directory check performed away from the headless
    /// loop. It is consumed by the matching resume attempt.
    pub(crate) fn record_directory_check(
        &mut self,
        terminal_id: TerminalId,
        cwd: PathBuf,
        available: bool,
    ) {
        self.directory_checks.insert((terminal_id, cwd), available);
    }

    /// Returns whether a worker has already checked this candidate's cwd.
    pub(crate) fn has_directory_check(&self, terminal_id: &TerminalId, cwd: &Path) -> bool {
        self.directory_checks
            .contains_key(&(terminal_id.clone(), cwd.to_path_buf()))
    }

    /// Takes a worker result when the matching candidate is attempted.
    pub(crate) fn take_directory_check(
        &mut self,
        terminal_id: &TerminalId,
        cwd: &Path,
    ) -> Option<bool> {
        self.directory_checks
            .remove(&(terminal_id.clone(), cwd.to_path_buf()))
    }

    /// When the loop should wake for a resume: `None` while nothing is
    /// eligible (the barrier is kept for when something is) or while nothing
    /// holds an eligible candidate back, otherwise the later of the barrier and
    /// the theme wait (which does not apply once the theme is available).
    pub(crate) fn wakeup(&self, eligible: bool, theme_available: bool) -> Option<Instant> {
        if !eligible {
            return None;
        }
        let pending = self.pending?;
        let theme_wait = if theme_available {
            None
        } else {
            pending.theme_wait_until
        };
        pending.not_before.max(theme_wait)
    }

    /// Whether an attempt may run now: candidates are eligible and neither the
    /// barrier nor the theme wait still holds them back.
    pub(crate) fn is_due(&self, now: Instant, eligible: bool, theme_available: bool) -> bool {
        eligible
            && self.pending.is_some()
            && self
                .wakeup(eligible, theme_available)
                .is_none_or(|wakeup| now >= wakeup)
    }

    /// Starts a pass over the candidates; feed each attempt's outcome to
    /// [`ResumePass::record`] and hand the pass back to [`Self::finish`].
    pub(crate) fn begin_pass(&self, now: Instant) -> ResumePass {
        ResumePass {
            now,
            spacing: self.spacing,
            backoff: self.backoff,
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
    backoff: Duration,
    barrier: Option<Instant>,
    changed: bool,
}

impl ResumePass {
    /// Records one attempt and says whether the walk goes on. It continues
    /// past retryable failures and abandonments, and stops after a launch when
    /// spacing is nonzero.
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
            AttemptOutcome::Retryable => {
                self.raise_barrier(self.now + self.backoff);
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
    const BACKOFF: Duration = Duration::from_secs(1);

    fn schedule(spacing_ms: u64) -> ResumeSchedule {
        ResumeSchedule::new(THEME_WAIT, Duration::from_millis(spacing_ms), BACKOFF)
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
    fn a_retryable_first_candidate_does_not_block_a_later_launch() {
        let mut schedule = schedule(100);
        let start = Instant::now();
        schedule.observe(start, true, true);
        let mut now = start + THEME_WAIT;
        for _ in 0..3 {
            assert!(schedule.is_due(now, true, false));
            // The first candidate fails retryably every pass; the second
            // still launches, and only then does the walk stop.
            let attempted = run(
                &mut schedule,
                now,
                &[AttemptOutcome::Retryable, AttemptOutcome::Launched],
            );
            assert_eq!(attempted, 2);
            // The barrier is the later of the backoff and the spacing.
            assert_eq!(schedule.not_before(), Some(now + BACKOFF));
            assert!(!schedule.is_due(now + BACKOFF - Duration::from_millis(1), true, false));
            now += BACKOFF;
            schedule.observe(now, true, true);
        }
    }

    #[test]
    fn a_walk_continues_past_retryable_failures_and_abandonments() {
        let mut schedule = schedule(100);
        let now = Instant::now();
        schedule.observe(now, true, true);
        let attempted = run(
            &mut schedule,
            now + THEME_WAIT,
            &[
                AttemptOutcome::Retryable,
                AttemptOutcome::Abandoned,
                AttemptOutcome::Retryable,
            ],
        );
        assert_eq!(attempted, 3);
        assert_eq!(schedule.not_before(), Some(now + THEME_WAIT + BACKOFF));
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
        assert_eq!(schedule.wakeup(true, true), Some(barrier));
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
        assert_eq!(schedule.wakeup(true, false), Some(deadline));
        // A loop that keeps running (any pane printing) observes on every
        // iteration; none of them moves the wait.
        for step in 1..7 {
            let now = start + Duration::from_millis(step * 100);
            schedule.observe(now, true, true);
            assert_eq!(schedule.wakeup(true, false), Some(deadline));
            assert_eq!(schedule.is_due(now, true, false), now >= deadline);
        }
    }

    #[test]
    fn the_theme_bypasses_the_wait_but_not_a_barrier() {
        let mut schedule = schedule(250);
        let start = Instant::now();
        schedule.observe(start, true, true);
        assert!(!schedule.is_due(start, true, false));
        assert!(
            schedule.is_due(start, true, true),
            "the theme ends the wait"
        );
        run(&mut schedule, start, &[AttemptOutcome::Launched]);
        let barrier = start + Duration::from_millis(250);
        assert!(!schedule.is_due(start, true, true));
        assert_eq!(schedule.wakeup(true, true), Some(barrier));
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
        assert_eq!(schedule.wakeup(false, true), None);
        assert_eq!(schedule.wakeup(false, false), None);
        assert!(!schedule.is_due(barrier, false, true));

        // Something becomes eligible again: the barrier still holds.
        schedule.observe(start, true, true);
        assert_eq!(schedule.wakeup(true, true), Some(barrier));
    }

    #[test]
    fn the_theme_wait_starts_when_candidates_first_become_eligible() {
        let mut schedule = schedule(0);
        let start = Instant::now();
        schedule.observe(start, true, false);
        assert_eq!(schedule.wakeup(true, false), None);
        let later = start + Duration::from_secs(5);
        schedule.observe(later, true, true);
        assert_eq!(schedule.wakeup(true, false), Some(later + THEME_WAIT));
        // Never restarted, even across a stretch with nothing eligible.
        schedule.observe(later + Duration::from_secs(1), true, false);
        schedule.observe(later + Duration::from_secs(2), true, true);
        assert_eq!(schedule.wakeup(true, false), Some(later + THEME_WAIT));
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
        assert_eq!(schedule.wakeup(true, false), None);

        // New plans start from scratch: fresh theme wait, no old barrier.
        let later = start + Duration::from_secs(1);
        schedule.observe(later, true, true);
        assert_eq!(schedule.not_before(), None);
        assert_eq!(schedule.wakeup(true, false), Some(later + THEME_WAIT));
    }
}
