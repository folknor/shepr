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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// Why a pane ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneEndReason {
    /// The child exited with a status code.
    Exited,
    /// A signal ended the child.
    Signalled,
    /// Waiting for the child failed.
    WaitFailed,
    /// The pane's PTY reader panicked (or found the terminal core poisoned).
    /// The child may still be running; the pane is ended so its session is
    /// torn down.
    ReaderPanicked,
    /// The PTY actor hit a hard IO failure and can no longer read the pane.
    ReaderIoFailed,
    /// Every holder closed the pane's terminal, and the child watcher had not
    /// reported an exit a grace period later: usually the child closed its
    /// terminal and kept going. Nothing can reach it through the pane any
    /// more, so the pane ends.
    TerminalClosed,
}

impl From<shepr_platform::ChildExitKind> for PaneEndReason {
    fn from(kind: shepr_platform::ChildExitKind) -> Self {
        match kind {
            shepr_platform::ChildExitKind::Exited => Self::Exited,
            shepr_platform::ChildExitKind::Signalled => Self::Signalled,
        }
    }
}

/// How a pane ended: the one place that answers whether the exit is
/// checkpointed before the pane is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneEnding {
    reason: PaneEndReason,
}

impl PaneEnding {
    pub fn new(reason: PaneEndReason) -> Self {
        Self { reason }
    }

    pub fn reason(self) -> PaneEndReason {
        self.reason
    }

    /// Whether the exit needs a final session checkpoint before pane removal.
    /// A signal, failed child wait, reader failure or terminal close can retire
    /// a pane while its agent session remains resumable, so its identity must
    /// be kept. A normally exited child has finished and needs no resume
    /// checkpoint. A failed wait can leave the child alive, but the pane is
    /// still retired and needs its session identity saved first. A checkpoint
    /// reads layout, labels, the cwd and agent identity, none of
    /// them from the terminal core, so a core a panic broke does not exempt
    /// the pane. shepr-generated teardown signals follow pane removal, or
    /// happen during startup failure before any pane exit event, so they
    /// cannot skip a checkpoint for a pane that is still live.
    pub fn needs_checkpoint(self) -> bool {
        matches!(
            self.reason,
            PaneEndReason::Signalled
                | PaneEndReason::WaitFailed
                | PaneEndReason::ReaderPanicked
                | PaneEndReason::ReaderIoFailed
                | PaneEndReason::TerminalClosed
        )
    }
}

/// How the pane ended, as its first observer recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecordedEnding {
    /// The runtime was torn down on request: nothing is published.
    Silent,
    /// The pane ended as `ending` at `ended_at`, which is published as its
    /// death. `child_exit_confirmed` says the child was reaped, so its launch
    /// status channel is closed and settling the launch cannot wait on a live
    /// child. `ended_at` is when the observer saw the ending, not when the app
    /// handles it: a checkpoint judges how close it followed an agent's exit.
    Observed {
        ending: PaneEnding,
        child_exit_confirmed: bool,
        ended_at: Instant,
    },
}

#[derive(Default)]
pub(super) struct PaneExitArbiter {
    decided: Mutex<Option<RecordedEnding>>,
    changed: Condvar,
    /// Wakes the publisher. It is the only waiter, and a notification sent
    /// before it waits is kept as a permit, so no decision is missed.
    published: Notify,
    /// Set with the decision, so a task can poll for the ending without the
    /// lock. The pane's detection task is the only one that waits on
    /// `cancel_wake`, which keeps its notification as a permit.
    cancelled: AtomicBool,
    cancel_wake: Notify,
}

impl PaneExitArbiter {
    fn record(&self, decided: &mut Option<RecordedEnding>, ending: RecordedEnding) {
        *decided = Some(ending);
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_all();
        self.published.notify_one();
        self.cancel_wake.notify_one();
    }

    /// Records `ending` if nothing has been decided yet. Returns whether this
    /// call decided.
    pub(super) fn decide(&self, ending: RecordedEnding) -> bool {
        let mut decided = shepr_core::locks::lock_auxiliary(&self.decided);
        if decided.is_some() {
            return false;
        }
        self.record(&mut decided, ending);
        true
    }

    /// Waits up to `grace` for another observer to decide; if none has by
    /// then, records `ending`. Returns whether this call decided. The deadline
    /// is absolute, so a spurious wake neither decides early nor renews it.
    pub(super) fn decide_after(&self, grace: Duration, ending: RecordedEnding) -> bool {
        // clock-io-ok: bounds a real wait for the child watcher to report.
        let deadline = Instant::now() + grace;
        let mut decided = shepr_core::locks::lock_auxiliary(&self.decided);
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
    pub(super) fn ending(&self) -> Option<RecordedEnding> {
        *shepr_core::locks::lock_auxiliary(&self.decided)
    }

    /// Whether an ending is recorded, without taking the lock. Work that
    /// should stop with the pane (the detection task, between its steps)
    /// checks this instead of `ending`.
    pub(super) fn is_decided(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Completes once an ending is recorded. For the pane's detection task
    /// only: a second waiter could take the permit the first needs.
    pub(super) async fn cancelled(&self) {
        while !self.is_decided() {
            self.cancel_wake.notified().await;
        }
    }

    /// Waits until an ending is recorded. For the one publisher only.
    pub(super) async fn decided(&self) -> RecordedEnding {
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
    use std::sync::{Arc, LazyLock};

    static CLOSED_AT: LazyLock<Instant> = LazyLock::new(Instant::now);

    fn closed() -> RecordedEnding {
        RecordedEnding::Observed {
            ending: PaneEnding::new(PaneEndReason::TerminalClosed),
            child_exit_confirmed: false,
            ended_at: *CLOSED_AT,
        }
    }

    #[test]
    fn checkpoint_follows_the_reason() {
        use PaneEndReason::*;
        for (reason, expected) in [
            (Exited, false),
            (Signalled, true),
            (WaitFailed, true),
            (ReaderPanicked, true),
            (ReaderIoFailed, true),
            (TerminalClosed, true),
        ] {
            assert_eq!(
                PaneEnding::new(reason).needs_checkpoint(),
                expected,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn only_the_first_decision_wins() {
        let arbiter = PaneExitArbiter::default();
        assert!(arbiter.decide(RecordedEnding::Silent));
        assert!(!arbiter.decide(closed()));
        assert!(!arbiter.decide_after(Duration::ZERO, closed()));
        assert_eq!(arbiter.ending(), Some(RecordedEnding::Silent));
    }

    #[test]
    fn an_undecided_grace_decides_at_its_deadline() {
        let arbiter = PaneExitArbiter::default();
        assert!(arbiter.decide_after(Duration::from_millis(10), closed()));
        assert!(!arbiter.decide(RecordedEnding::Silent));
        assert_eq!(arbiter.ending(), Some(closed()));
    }

    #[test]
    fn a_decision_during_the_grace_ends_it_at_once() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        let waiter = {
            let arbiter = Arc::clone(&arbiter);
            std::thread::spawn(move || {
                let started = Instant::now();
                (
                    arbiter.decide_after(Duration::from_secs(30), closed()),
                    started.elapsed(),
                )
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        assert!(arbiter.decide(RecordedEnding::Silent));
        let (won, waited) = waiter.join().expect("waiter");
        assert!(!won);
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    }

    #[tokio::test]
    async fn cancellation_follows_the_first_decision_without_the_lock() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        assert!(!arbiter.is_decided());
        let waiter = {
            let arbiter = Arc::clone(&arbiter);
            tokio::spawn(async move { arbiter.cancelled().await })
        };
        tokio::task::yield_now().await;
        assert!(arbiter.decide(closed()));
        waiter.await.expect("cancellation waiter");
        assert!(arbiter.is_decided());
        // A decision made before anyone waits is not missed either.
        tokio::time::timeout(Duration::from_secs(5), arbiter.cancelled())
            .await
            .expect("decided arbiter is already cancelled");
    }

    #[tokio::test]
    async fn the_publisher_sees_a_decision_made_before_it_waits() {
        let arbiter = PaneExitArbiter::default();
        arbiter.decide(closed());
        assert_eq!(arbiter.decided().await, closed());
    }

    #[tokio::test]
    async fn the_publisher_wakes_for_a_later_decision() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        let publisher = {
            let arbiter = Arc::clone(&arbiter);
            tokio::spawn(async move { arbiter.decided().await })
        };
        tokio::task::yield_now().await;
        arbiter.decide(RecordedEnding::Silent);
        assert_eq!(publisher.await.expect("publisher"), RecordedEnding::Silent);
    }
}
