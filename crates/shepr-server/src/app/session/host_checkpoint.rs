use std::time::Instant;

use super::checkpoint_retry_delay;
use crate::limits::CHECKPOINT_MAX_FAILURES;

/// The checkpoint a logind shutdown warning asks for: one save, retried after
/// failures, whose result the lifecycle takes before it freezes saves. A
/// cancelled request's late completion is ignored because the saver voids
/// the host half of the ticket in flight, not by a counter here.
pub(super) enum HostShutdownCheckpoint {
    Idle,
    Requested {
        /// Consecutive failed attempts of this request.
        failures: u8,
        /// A failed attempt's retry; `None` means start at once.
        retry_at: Option<Instant>,
    },
    /// The checkpoint saved. Held until the lifecycle takes the result.
    Saved,
    /// `CHECKPOINT_MAX_FAILURES` attempts failed. Held until the lifecycle
    /// takes the result.
    Unsaved,
}

impl HostShutdownCheckpoint {
    pub(super) fn new() -> Self {
        Self::Idle
    }

    /// Requests the checkpoint; returns false, changing nothing, while one is
    /// requested or its result is unclaimed.
    pub(super) fn request(&mut self) -> bool {
        if !matches!(self, Self::Idle) {
            return false;
        }
        *self = Self::Requested {
            failures: 0,
            retry_at: None,
        };
        true
    }

    pub(super) fn is_requested(&self) -> bool {
        matches!(self, Self::Requested { .. })
    }

    pub(super) fn retry_at(&self) -> Option<Instant> {
        if let Self::Requested { retry_at, .. } = self {
            *retry_at
        } else {
            None
        }
    }

    pub(super) fn is_finished(&self) -> bool {
        matches!(self, Self::Saved | Self::Unsaved)
    }

    pub(super) fn finished_unsaved(&self) -> bool {
        matches!(self, Self::Unsaved)
    }

    pub(super) fn saved(&mut self) {
        if self.is_requested() {
            *self = Self::Saved;
        }
    }

    /// An attempt failed. Returns whether this was the last one allowed,
    /// finishing the checkpoint unsaved; otherwise the retry is armed.
    pub(super) fn failed(&mut self, now: Instant) -> bool {
        let Self::Requested { failures, retry_at } = self else {
            return false;
        };
        let delay = checkpoint_retry_delay(*failures);
        *failures = failures.saturating_add(1);
        if *failures >= CHECKPOINT_MAX_FAILURES {
            *self = Self::Unsaved;
            true
        } else {
            *retry_at = Some(now + delay);
            false
        }
    }

    /// Takes a finished checkpoint's result (whether it saved), once.
    pub(super) fn take_result(&mut self) -> Option<bool> {
        let result = match self {
            Self::Saved => true,
            Self::Unsaved => false,
            Self::Idle | Self::Requested { .. } => return None,
        };
        *self = Self::Idle;
        Some(result)
    }

    /// Drops a request or an unclaimed result.
    pub(super) fn cancel(&mut self) {
        *self = Self::Idle;
    }

    #[cfg(test)]
    pub(super) fn expedite_retry(&mut self) {
        if let Self::Requested { retry_at, .. } = self {
            *retry_at = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::SESSION_SAVE_RETRY_MIN;
    #[test]
    fn a_request_is_ignored_while_requested_or_unclaimed() {
        let mut host = HostShutdownCheckpoint::new();
        assert!(host.request());
        assert!(!host.request());
        host.saved();
        assert!(!host.request());
        assert!(host.is_finished());
        assert!(!host.finished_unsaved());
    }
    #[test]
    fn two_failures_retry_and_the_third_finishes_unsaved() {
        let mut host = HostShutdownCheckpoint::new();
        let now = Instant::now();
        host.request();
        assert!(!host.failed(now));
        assert_eq!(host.retry_at(), Some(now + SESSION_SAVE_RETRY_MIN));
        assert!(!host.failed(now));
        assert_eq!(host.retry_at(), Some(now + SESSION_SAVE_RETRY_MIN * 2));
        assert!(host.failed(now));
        assert!(host.finished_unsaved());
        assert_eq!(host.retry_at(), None);
        assert!(!host.request());
    }
    #[test]
    fn a_result_is_taken_once() {
        let mut host = HostShutdownCheckpoint::new();
        assert_eq!(host.take_result(), None);
        host.request();
        host.saved();
        assert_eq!(host.take_result(), Some(true));
        assert_eq!(host.take_result(), None);
        host.request();
        for _ in 0..CHECKPOINT_MAX_FAILURES {
            host.failed(Instant::now());
        }
        assert_eq!(host.take_result(), Some(false));
        assert_eq!(host.take_result(), None);
    }
    #[test]
    fn cancel_discards_a_request_and_an_unclaimed_result() {
        let mut host = HostShutdownCheckpoint::new();
        host.request();
        host.cancel();
        host.saved();
        assert_eq!(host.take_result(), None);
        assert!(host.request());
        host.saved();
        host.cancel();
        assert_eq!(host.take_result(), None);
        assert!(host.request());
    }
}
