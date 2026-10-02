//! The one decision of how a pane ended. The child watcher, the PTY reader
//! and the runtime's own teardown can each observe an ending, in any order;
//! the first to decide records it, and a later observation can neither
//! replace it nor skip the checkpoint it asked for.
//!
//! Observers only record. The pane's launch coordinator (`launch_status`) is
//! the one publisher: it tells the app how the launch ended, then publishes
//! the recorded ending, so the app always hears the settlement first whoever
//! decided. Recording takes a short lock and never waits on the app, so the
//! runtime's teardown cannot block on a full event channel.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use shepr_platform::ChildExitReason;
use tokio::sync::Notify;

/// How the pane ended, as its first observer recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneEnding {
    /// The runtime was torn down on request: nothing is published.
    Silent,
    /// The pane ended for `reason`, which is published as its death.
    /// `child_exit_confirmed` says the child was reaped, so its launch status
    /// channel is closed and settling the launch cannot wait on a live child.
    Observed {
        reason: ChildExitReason,
        child_exit_confirmed: bool,
    },
}

#[derive(Default)]
pub(super) struct PaneExitArbiter {
    decided: Mutex<Option<PaneEnding>>,
    changed: Condvar,
    /// Wakes the publisher. It is the only waiter, and a notification sent
    /// before it waits is kept as a permit, so no decision is missed.
    published: Notify,
}

impl PaneExitArbiter {
    fn record(&self, decided: &mut Option<PaneEnding>, ending: PaneEnding) {
        *decided = Some(ending);
        self.changed.notify_all();
        self.published.notify_one();
    }

    /// Records `ending` if nothing has been decided yet. Returns whether this
    /// call decided.
    pub(super) fn decide(&self, ending: PaneEnding) -> bool {
        let mut decided = shepr_vt::lock_auxiliary(&self.decided);
        if decided.is_some() {
            return false;
        }
        self.record(&mut decided, ending);
        true
    }

    /// Waits up to `grace` for another observer to decide; if none has by
    /// then, records `ending`. Returns whether this call decided. The deadline
    /// is absolute, so a spurious wake neither decides early nor renews it.
    pub(super) fn decide_after(&self, grace: Duration, ending: PaneEnding) -> bool {
        // clock-io-ok: bounds a real wait for the child watcher to report.
        let deadline = Instant::now() + grace;
        let mut decided = shepr_vt::lock_auxiliary(&self.decided);
        loop {
            if decided.is_some() {
                return false;
            }
            // clock-io-ok: the same real wait.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.record(&mut decided, ending);
                return true;
            }
            decided = match self.changed.wait_timeout(decided, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// The recorded ending, if any.
    pub(super) fn ending(&self) -> Option<PaneEnding> {
        *shepr_vt::lock_auxiliary(&self.decided)
    }

    /// Waits until an ending is recorded. For the one publisher only.
    pub(super) async fn decided(&self) -> PaneEnding {
        loop {
            if let Some(ending) = self.ending() {
                return ending;
            }
            self.published.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const CLOSED: PaneEnding = PaneEnding::Observed {
        reason: ChildExitReason::TerminalClosed,
        child_exit_confirmed: false,
    };

    #[test]
    fn only_the_first_decision_wins() {
        let arbiter = PaneExitArbiter::default();
        assert!(arbiter.decide(PaneEnding::Silent));
        assert!(!arbiter.decide(CLOSED));
        assert!(!arbiter.decide_after(Duration::ZERO, CLOSED));
        assert_eq!(arbiter.ending(), Some(PaneEnding::Silent));
    }

    #[test]
    fn an_undecided_grace_decides_at_its_deadline() {
        let arbiter = PaneExitArbiter::default();
        assert!(arbiter.decide_after(Duration::from_millis(10), CLOSED));
        assert!(!arbiter.decide(PaneEnding::Silent));
        assert_eq!(arbiter.ending(), Some(CLOSED));
    }

    #[test]
    fn a_decision_during_the_grace_ends_it_at_once() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        let waiter = {
            let arbiter = Arc::clone(&arbiter);
            std::thread::spawn(move || {
                let started = Instant::now();
                (
                    arbiter.decide_after(Duration::from_secs(30), CLOSED),
                    started.elapsed(),
                )
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        assert!(arbiter.decide(PaneEnding::Silent));
        let (won, waited) = waiter.join().expect("waiter");
        assert!(!won);
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    }

    #[tokio::test]
    async fn the_publisher_sees_a_decision_made_before_it_waits() {
        let arbiter = PaneExitArbiter::default();
        arbiter.decide(CLOSED);
        assert_eq!(arbiter.decided().await, CLOSED);
    }

    #[tokio::test]
    async fn the_publisher_wakes_for_a_later_decision() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        let publisher = {
            let arbiter = Arc::clone(&arbiter);
            tokio::spawn(async move { arbiter.decided().await })
        };
        tokio::task::yield_now().await;
        arbiter.decide(PaneEnding::Silent);
        assert_eq!(publisher.await.expect("publisher"), PaneEnding::Silent);
    }
}
