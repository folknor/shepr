use std::time::Duration;

use crate::limits::BACKOFF_MULTIPLIER;

/// Exponential retry spacing shared by session writes, checkpoints, empty
/// workspace creation, and logind reconnection.
#[derive(Clone, Copy)]
pub(crate) struct Backoff {
    min: Duration,
    max: Duration,
}

impl Backoff {
    pub(crate) const fn new(min: Duration, max: Duration) -> Self {
        Self { min, max }
    }

    /// Delay after `failures_before` earlier consecutive failures, starting
    /// at `min` and saturating at `max` without overflowing for a long-lived
    /// failure streak.
    pub(crate) fn delay_after(self, failures_before: u32) -> Duration {
        let factor = BACKOFF_MULTIPLIER
            .checked_pow(failures_before)
            .unwrap_or(u32::MAX);
        self.min.saturating_mul(factor).min(self.max)
    }
}
