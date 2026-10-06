use std::time::Duration;

use crate::limits::BACKOFF_MULTIPLIER;

/// Saturating exponential retry spacing. Callers own failure counts, reset
/// conditions and attempt limits; this type only computes the delay.
#[derive(Clone, Copy)]
pub struct Backoff {
    min: Duration,
    max: Duration,
}

impl Backoff {
    pub const fn new(min: Duration, max: Duration) -> Self {
        Self { min, max }
    }

    /// Delay after `failures_before` earlier consecutive failures, starting
    /// at `min` and saturating at `max` without overflowing for a long-lived
    /// failure streak.
    pub fn delay_after(self, failures_before: u32) -> Duration {
        let mut delay = self.min.min(self.max);
        if delay.is_zero() {
            return delay;
        }
        // Stop at the duration cap, rather than capping an intermediate u32
        // power: very small minima may need more than 32 doublings to reach it.
        for _ in 0..failures_before {
            if delay >= self.max {
                break;
            }
            delay = delay.saturating_mul(BACKOFF_MULTIPLIER).min(self.max);
        }
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_grow_and_saturate_even_with_a_tiny_minimum() {
        let retry = Backoff::new(Duration::from_nanos(1), Duration::from_secs(60));
        assert_eq!(retry.delay_after(0), Duration::from_nanos(1));
        assert_eq!(retry.delay_after(32), Duration::from_nanos(1_u64 << 32));
        assert_eq!(retry.delay_after(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn zero_and_a_minimum_above_the_cap_are_bounded() {
        assert_eq!(
            Backoff::new(Duration::ZERO, Duration::MAX).delay_after(u32::MAX),
            Duration::ZERO
        );
        assert_eq!(
            Backoff::new(Duration::MAX, Duration::from_secs(1)).delay_after(0),
            Duration::from_secs(1)
        );
    }
}
