use std::time::Instant;

use super::SavePolicyConfig;

/// How a finished host-shutdown checkpoint ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostCheckpointOutcome {
    Saved,
    /// Every attempt failed, or the persister stopped accepting saves.
    Unsaved,
}

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
    /// `CHECKPOINT_MAX_FAILURES` attempts failed, or the persister stopped
    /// accepting saves for this boot. Held until the lifecycle takes the
    /// result.
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
    /// `config` is the saver's retry and failure-limit policy.
    pub(super) fn failed_with_config(&mut self, now: Instant, config: SavePolicyConfig) -> bool {
        let Self::Requested { failures, retry_at } = self else {
            return false;
        };
        let delay = config.checkpoint_retry.delay_after(u32::from(*failures));
        *failures = failures.saturating_add(1);
        if *failures >= config.checkpoint_max_failures {
            *self = Self::Unsaved;
            true
        } else {
            *retry_at = Some(now + delay);
            false
        }
    }

    /// Finishes a requested checkpoint when no later save can run.
    pub(super) fn fail_permanently(&mut self) {
        if self.is_requested() {
            *self = Self::Unsaved;
        }
    }

    /// Takes a finished checkpoint's result, once.
    pub(super) fn take_result(&mut self) -> Option<HostCheckpointOutcome> {
        let result = match self {
            Self::Saved => HostCheckpointOutcome::Saved,
            Self::Unsaved => HostCheckpointOutcome::Unsaved,
            Self::Idle | Self::Requested { .. } => return None,
        };
        *self = Self::Idle;
        Some(result)
    }

    /// Drops a request or an unclaimed result.
    pub(super) fn cancel(&mut self) {
        *self = Self::Idle;
    }

    /// [`Self::failed_with_config`] under the default save policy.
    #[cfg(test)]
    pub(super) fn failed(&mut self, now: Instant) -> bool {
        self.failed_with_config(now, SavePolicyConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::super::checkpoint_retry_delay;
    use super::*;
    use crate::limits::CHECKPOINT_MAX_FAILURES;
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
    fn the_failure_limit_finishes_unsaved_after_retries() {
        let mut host = HostShutdownCheckpoint::new();
        let now = Instant::now();
        host.request();
        for failures_before in 0..CHECKPOINT_MAX_FAILURES.saturating_sub(1) {
            assert!(!host.failed(now));
            assert_eq!(
                host.retry_at(),
                Some(now + checkpoint_retry_delay(failures_before))
            );
        }
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
        assert_eq!(host.take_result(), Some(HostCheckpointOutcome::Saved));
        assert_eq!(host.take_result(), None);
        host.request();
        for _ in 0..CHECKPOINT_MAX_FAILURES {
            host.failed(Instant::now());
        }
        assert_eq!(host.take_result(), Some(HostCheckpointOutcome::Unsaved));
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
