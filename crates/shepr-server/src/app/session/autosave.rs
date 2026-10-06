use std::time::{Duration, Instant};

use crate::backoff::Backoff;

/// The debounced save of the live layout, and the backoff shared by every
/// kind of save.
pub(super) struct Autosave {
    debounce: Duration,
    retry: Backoff,
    deadline: Option<Instant>,
    /// Consecutive failed saves of any kind, for the backoff.
    failures: u32,
}

impl Autosave {
    /// An autosave on the saver's policy: `debounce` after a mutation, and
    /// `retry` after each consecutive failed save.
    pub(super) fn with_config(debounce: Duration, retry: Backoff) -> Self {
        Self {
            debounce,
            retry,
            deadline: None,
            failures: 0,
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// The autosave's single deadline comparison, shared by scheduling and
    /// its boundary tests; checkpoint readiness belongs to SessionSaver.
    pub(super) fn is_due(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|d| now >= d)
    }

    /// A session mutation was observed: the save is due
    /// `SESSION_SAVE_DEBOUNCE` from now.
    pub(super) fn schedule(&mut self, now: Instant) {
        self.deadline = Some(now + self.debounce);
    }

    pub(super) fn clear(&mut self) {
        self.deadline = None;
    }

    /// Counts the failure and arms the retry: the earlier of an existing
    /// future deadline and a delay doubling from `SESSION_SAVE_RETRY_MIN` per
    /// consecutive failure, capped at `SESSION_SAVE_RETRY_MAX`, so a
    /// persistent failure (a full disk, an unwritable data directory) does
    /// not re-capture and rewrite the whole session at the minimum delay over
    /// and over.
    /// Returns the failure count and the delay.
    pub(super) fn record_failure(&mut self, now: Instant) -> (u32, Duration) {
        let failures_before = self.failures;
        self.failures = self.failures.saturating_add(1);
        let delay = self.retry.delay_after(failures_before);
        let retry = now + delay;
        self.deadline = Some(
            self.deadline
                .filter(|d| *d > now)
                .map_or(retry, |d| d.min(retry)),
        );
        (self.failures, delay)
    }

    /// Resets the backoff; returns the failures it recovered from, if any.
    pub(super) fn record_success(&mut self) -> Option<u32> {
        let failures = std::mem::take(&mut self.failures);
        (failures > 0).then_some(failures)
    }
}

#[cfg(test)]
impl Autosave {
    pub(super) fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{SESSION_SAVE_DEBOUNCE, SESSION_SAVE_RETRY_MAX, SESSION_SAVE_RETRY_MIN};

    const RETRY_BACKOFF: Backoff = Backoff::new(SESSION_SAVE_RETRY_MIN, SESSION_SAVE_RETRY_MAX);

    impl Autosave {
        /// An autosave on the production policy constants.
        fn new() -> Self {
            Self::with_config(SESSION_SAVE_DEBOUNCE, RETRY_BACKOFF)
        }
    }

    #[test]
    fn injected_policy_controls_debounce_and_retry() {
        let now = Instant::now();
        let debounce = Duration::from_millis(17);
        let retry = Backoff::new(Duration::from_millis(3), Duration::from_millis(9));
        let mut save = Autosave::with_config(debounce, retry);
        save.schedule(now);
        assert_eq!(save.deadline(), Some(now + debounce));
        save.clear();
        for failures_before in 0..5 {
            let (_, delay) = save.record_failure(now);
            assert_eq!(delay, retry.delay_after(failures_before));
            save.clear();
        }
    }

    #[test]
    fn schedule_sets_the_debounce_deadline() {
        let now = Instant::now();
        let mut save = Autosave::new();
        save.schedule(now);
        assert_eq!(save.deadline(), Some(now + SESSION_SAVE_DEBOUNCE));
        assert!(!save.is_due(now));
        assert!(save.is_due(now + SESSION_SAVE_DEBOUNCE));
    }

    #[test]
    fn failed_saves_back_off_cap_and_recover() {
        let now = Instant::now();
        let mut save = Autosave::new();
        let mut previous = Duration::ZERO;
        for count in 1..=12 {
            save.clear();
            let (failures, delay) = save.record_failure(now);
            assert_eq!(failures, count);
            assert!(delay >= previous);
            assert!(delay <= SESSION_SAVE_RETRY_MAX);
            assert_eq!(save.deadline(), Some(now + delay));
            previous = delay;
        }
        assert_eq!(previous, SESSION_SAVE_RETRY_MAX);
        assert_eq!(save.record_success(), Some(12));
        assert_eq!(save.record_success(), None);
        save.clear();
        assert_eq!(save.record_failure(now), (1, SESSION_SAVE_RETRY_MIN));
    }

    #[test]
    fn a_retry_never_postpones_an_earlier_pending_deadline() {
        let now = Instant::now();
        let mut save = Autosave::new();
        save.deadline = Some(now + Duration::from_millis(1));
        save.record_failure(now);
        assert_eq!(save.deadline(), Some(now + Duration::from_millis(1)));
        save.deadline = Some(now);
        save.record_failure(now);
        assert_eq!(save.deadline(), Some(now + RETRY_BACKOFF.delay_after(1)));
    }
}
