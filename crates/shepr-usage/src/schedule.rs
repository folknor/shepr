//! When an endpoint of an account may be asked again. Pure: every decision
//! takes the monotonic time it is made at, so it is tested without a clock.

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use crate::transport::RetryAfter;

/// The timing a worker runs with. Production uses [`Timing::default`]; tests
/// shorten it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub(crate) poll_cadence: Duration,
    pub(crate) jitter_percent: u32,
    pub(crate) host_spacing: Duration,
    pub(crate) profile_refresh: Duration,
    pub(crate) throttle_ladder: [Duration; 5],
    pub(crate) failure_start: Duration,
    pub(crate) failure_max: Duration,
    pub(crate) reset_follow_up: Duration,
    pub(crate) reread: Duration,
    pub(crate) read_deadline: Duration,
    pub(crate) unreadable_grace: Duration,
    pub(crate) probe_retry: Duration,
    pub(crate) idle_wake: Duration,
    /// How long a host stays untried after its request slot was found held.
    pub(crate) host_blocked_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        use crate::limits::*;
        Self {
            poll_cadence: USAGE_POLL_CADENCE,
            jitter_percent: USAGE_POLL_JITTER_PERCENT,
            host_spacing: HOST_REQUEST_SPACING,
            profile_refresh: PROFILE_REFRESH_INTERVAL,
            throttle_ladder: THROTTLE_LADDER,
            failure_start: FAILURE_BACKOFF_START,
            failure_max: FAILURE_BACKOFF_MAX,
            reset_follow_up: RESET_FOLLOW_UP,
            reread: CREDENTIAL_REREAD_INTERVAL,
            read_deadline: CREDENTIAL_READ_DEADLINE,
            unreadable_grace: UNREADABLE_GRACE,
            probe_retry: CURL_PROBE_RETRY,
            idle_wake: WORKER_IDLE_WAKE,
            host_blocked_retry: HOST_REQUEST_SPACING.saturating_mul(6),
        }
    }
}

/// The gate of one account endpoint (or one generation's identity
/// bootstrap): when it may be asked next, and how deep in backoff it is.
/// Rotation, dedupe and pause never reset it; only its own outcomes do.
#[derive(Debug, Clone, Default)]
pub(crate) struct Gate {
    /// `None`: due now.
    next: Option<Instant>,
    throttle_streak: u32,
    failure_streak: u32,
}

/// What the last outcome did to a gate, for health reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Backoff {
    pub(crate) retry_at: Instant,
    pub(crate) streak: u32,
}

impl Gate {
    pub(crate) fn due_at(&self) -> Option<Instant> {
        self.next
    }

    pub(crate) fn in_backoff(&self) -> bool {
        self.throttle_streak > 0 || self.failure_streak > 0
    }

    /// After a success: the cadence plus jitter that only ever delays.
    pub(crate) fn succeeded(&mut self, now: Instant, timing: &Timing, jitter_key: &impl Hash) {
        self.throttle_streak = 0;
        self.failure_streak = 0;
        self.next = Some(add(now, timing.poll_cadence + jitter(timing, jitter_key)));
    }

    /// After a success of an endpoint polled on another interval.
    pub(crate) fn succeeded_after(&mut self, now: Instant, interval: Duration) {
        self.throttle_streak = 0;
        self.failure_streak = 0;
        self.next = Some(add(now, interval));
    }

    /// After a 429: the ladder step, lengthened by a positive Retry-After.
    pub(crate) fn throttled(&mut self, now: Instant, timing: &Timing, hint: RetryAfter) -> Backoff {
        let step = usize::try_from(self.throttle_streak).unwrap_or(usize::MAX);
        let ladder = timing.throttle_ladder;
        let mut delay = ladder[step.min(ladder.len() - 1)];
        if let RetryAfter::Delay(hint) = hint {
            delay = delay.max(hint);
        }
        self.throttle_streak = self.throttle_streak.saturating_add(1);
        let retry_at = add(now, delay);
        self.next = Some(retry_at);
        Backoff {
            retry_at,
            streak: self.throttle_streak,
        }
    }

    /// After a network, transport, server or response failure: doubling
    /// from the start delay up to the maximum, lengthened by a Retry-After.
    pub(crate) fn failed(&mut self, now: Instant, timing: &Timing, hint: RetryAfter) -> Backoff {
        let doublings = self.failure_streak.min(16);
        let mut delay = timing
            .failure_start
            .saturating_mul(1 << doublings)
            .min(timing.failure_max);
        if let RetryAfter::Delay(hint) = hint {
            delay = delay.max(hint);
        }
        self.failure_streak = self.failure_streak.saturating_add(1);
        let retry_at = add(now, delay);
        self.next = Some(retry_at);
        Backoff {
            retry_at,
            streak: self.failure_streak,
        }
    }

    /// Pushes the next poll to at least `at`, keeping any later bound and the
    /// backoff streaks: what another job's success does to this endpoint,
    /// which may delay it but never shorten an outstanding backoff.
    pub(crate) fn extend_to(&mut self, at: Instant) {
        self.next = Some(self.next.map_or(at, |next| next.max(at)));
    }

    /// Brings the next poll forward to just after a window resets, when the
    /// gate is not backing off and the poll was due later anyway.
    pub(crate) fn follow_reset(&mut self, reset: Instant, timing: &Timing) {
        if self.in_backoff() {
            return;
        }
        let at = add(reset, timing.reset_follow_up);
        if self.next.is_some_and(|next| at < next) {
            self.next = Some(at);
        }
    }
}

fn add(now: Instant, delay: Duration) -> Instant {
    now.checked_add(delay).unwrap_or(now)
}

/// A stable share of the cadence, up to its jitter percentage, chosen by the
/// key so accounts spread out but each keeps its own offset.
fn jitter(timing: &Timing, key: &impl Hash) -> Duration {
    // SipHash with fixed keys: stable across runs, which is what spreading
    // needs; nothing here is security sensitive.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let fraction = u32::try_from(hasher.finish() % 1_000).unwrap_or(0);
    timing.poll_cadence * timing.jitter_percent / 100 * fraction / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> Timing {
        Timing::default()
    }

    #[test]
    fn success_waits_the_cadence_and_jitter_never_brings_it_forward() {
        let now = Instant::now();
        let mut gate = Gate::default();
        assert_eq!(gate.due_at(), None, "a new gate is due at once");
        gate.succeeded(now, &timing(), &"account");
        let next = gate.due_at().expect("scheduled");
        assert!(next >= now + timing().poll_cadence);
        assert!(next <= now + timing().poll_cadence * 6 / 5);
    }

    #[test]
    fn throttling_climbs_the_ladder_and_a_hint_only_lengthens() {
        let now = Instant::now();
        let mut gate = Gate::default();
        let first = gate.throttled(now, &timing(), RetryAfter::None);
        assert_eq!(first.retry_at, now + Duration::from_secs(5 * 60));
        let second = gate.throttled(now, &timing(), RetryAfter::Delay(Duration::from_secs(1)));
        assert_eq!(second.retry_at, now + Duration::from_secs(10 * 60));
        let third = gate.throttled(now, &timing(), RetryAfter::Delay(Duration::from_secs(3600)));
        assert_eq!(third.retry_at, now + Duration::from_secs(3600));
        for _ in 0..10 {
            gate.throttled(now, &timing(), RetryAfter::None);
        }
        let capped = gate.throttled(now, &timing(), RetryAfter::None);
        assert_eq!(capped.retry_at, now + Duration::from_secs(60 * 60));
        gate.succeeded(now, &timing(), &1);
        assert!(!gate.in_backoff());
    }

    #[test]
    fn failures_double_up_to_the_maximum() {
        let now = Instant::now();
        let mut gate = Gate::default();
        let delays: Vec<Duration> = (0..8)
            .map(|_| gate.failed(now, &timing(), RetryAfter::None).retry_at - now)
            .collect();
        assert_eq!(delays[0], Duration::from_secs(60));
        assert_eq!(delays[1], Duration::from_secs(120));
        assert_eq!(delays[7], Duration::from_secs(30 * 60));
    }

    #[test]
    fn a_reset_follow_up_respects_backoff() {
        let now = Instant::now();
        let mut gate = Gate::default();
        gate.succeeded(now, &timing(), &"a");
        gate.follow_reset(now + Duration::from_secs(60), &timing());
        assert_eq!(gate.due_at(), Some(now + Duration::from_secs(75)));

        let mut throttled = Gate::default();
        let backoff = throttled.throttled(now, &timing(), RetryAfter::None);
        throttled.follow_reset(now + Duration::from_secs(60), &timing());
        assert_eq!(throttled.due_at(), Some(backoff.retry_at));
    }
}
