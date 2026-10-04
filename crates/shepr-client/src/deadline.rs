//! One monotonic deadline, with all arithmetic and remaining-time conversions in one place.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deadline(Instant);

impl Deadline {
    pub(crate) const fn at(instant: Instant) -> Self {
        Self(instant)
    }

    pub(crate) fn after(now: Instant, duration: Duration) -> Self {
        Self(now + duration)
    }

    pub(crate) const fn instant(self) -> Instant {
        self.0
    }

    pub(crate) fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    pub(crate) fn remaining(self, now: Instant) -> Option<Duration> {
        self.0.checked_duration_since(now)
    }

    pub(crate) fn is_expired(self, now: Instant) -> bool {
        now >= self.0
    }
}

#[cfg(test)]
mod tests {
    use super::Deadline;
    use std::time::{Duration, Instant};

    #[test]
    fn deadline_remaining_and_min_use_the_supplied_time() {
        let start = Instant::now();
        let deadline = Deadline::after(start, Duration::from_millis(10));
        assert_eq!(
            deadline.remaining(start + Duration::from_millis(3)),
            Some(Duration::from_millis(7))
        );

        let earlier = Deadline::at(start + Duration::from_millis(5));
        let combined = deadline.min(earlier);
        assert_eq!(combined.instant(), earlier.instant());
        assert_eq!(
            combined.remaining(start + Duration::from_millis(5)),
            Some(Duration::ZERO)
        );
        assert!(combined.is_expired(start + Duration::from_millis(5)));
    }
}
