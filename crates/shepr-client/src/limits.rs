//! Client timing limits, named once so the event loop, shell input and the
//! endpoint writer agree on them.

use std::time::{Duration, Instant};

/// One monotonic deadline, with all arithmetic and remaining-time conversions in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Deadline(Instant);

impl Deadline {
    pub(super) const fn at(instant: Instant) -> Self {
        Self(instant)
    }

    pub(super) fn after(now: Instant, duration: Duration) -> Self {
        Self(now + duration)
    }

    pub(super) const fn instant(self) -> Instant {
        self.0
    }

    pub(super) fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    pub(super) fn remaining(self, now: Instant) -> Option<Duration> {
        self.0.checked_duration_since(now)
    }

    pub(super) fn remaining_millis_i32(self, now: Instant) -> Option<i32> {
        self.remaining(now).map(|remaining| {
            i32::try_from(remaining.as_millis())
                .unwrap_or(i32::MAX)
                .max(1)
        })
    }

    pub(super) fn is_expired(self, now: Instant) -> bool {
        now >= self.0
    }
}

/// Two clicks on the same spot within this window are a double click.
pub(super) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(350);
/// Minimum spacing of the requests a scrollbar or split drag sends.
pub(super) const MOUSE_DRAG_SEND_INTERVAL: Duration = Duration::from_millis(33);
pub(super) const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(30);
/// Minimum spacing of the frames a selection drag rebuilds.
pub(super) const SELECTION_REPAINT_INTERVAL: Duration = Duration::from_millis(16);
/// The longest the client loop sleeps when no shell timer is due sooner.
pub(super) const MAX_CLIENT_TIMER_DELAY: Duration = Duration::from_millis(100);
pub(super) const CLIENT_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
pub(super) const SSH_RESOURCE_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
pub(super) const ENDPOINT_ERROR_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an endpoint notice card stays up before it hides itself. A click on the card hides
/// it sooner; the timeout is what dismisses it when `ui.mouse_capture` is off.
pub(super) const ENDPOINT_NOTICE_TIMEOUT: Duration = Duration::from_secs(10);
/// Keep only the latest direct-attach transport notices while startup or forwarding is failing.
pub(super) const MAX_NOTICES: usize = 64;
/// How long one endpoint frame write, or an input flush, may block.
pub(super) const ENDPOINT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll spacing while an endpoint writer waits for socket progress.
pub(super) const ENDPOINT_IO_POLL_INTERVAL: Duration = Duration::from_millis(2);

const _: () = assert!(ENDPOINT_IO_POLL_INTERVAL.as_millis() < ENDPOINT_WRITE_TIMEOUT.as_millis());

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
        assert_eq!(
            deadline.remaining_millis_i32(start + Duration::from_millis(3)),
            Some(7)
        );

        let earlier = Deadline::at(start + Duration::from_millis(5));
        let combined = deadline.min(earlier);
        assert_eq!(combined.instant(), earlier.instant());
        assert_eq!(
            combined.remaining(start + Duration::from_millis(5)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            combined.remaining_millis_i32(start + Duration::from_millis(5)),
            Some(1)
        );
        assert!(combined.is_expired(start + Duration::from_millis(5)));
    }
}
