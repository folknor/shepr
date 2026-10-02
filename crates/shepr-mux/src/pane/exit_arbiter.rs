//! The one decision of how a pane ended. The child watcher, the PTY reader
//! and the runtime's own teardown can each observe an ending, in any order;
//! only the first to decide publishes a `PaneDied`, so a later observation
//! can neither publish a second, different exit nor skip the checkpoint the
//! first one asked for.
//!
//! Deciding and publishing are separate: a caller decides under the arbiter's
//! lock and sends its event after releasing it, so a send that waits for
//! channel capacity never holds the lock the runtime's teardown takes.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct PaneExitArbiter {
    decided: Mutex<bool>,
    changed: Condvar,
}

impl PaneExitArbiter {
    /// Decides the pane's ending if nothing has yet. Returns whether this
    /// call decided it, and so owns publishing it.
    pub(super) fn decide(&self) -> bool {
        let mut decided = shepr_vt::lock_auxiliary(&self.decided);
        if *decided {
            return false;
        }
        *decided = true;
        self.changed.notify_all();
        true
    }

    /// Waits up to `grace` for another observer to decide; if none has by
    /// then, decides itself. Returns whether this call decided. The deadline
    /// is absolute, so a spurious wake neither decides early nor renews it.
    pub(super) fn decide_after(&self, grace: Duration) -> bool {
        // clock-io-ok: bounds a real wait for the child watcher to report.
        let deadline = Instant::now() + grace;
        let mut decided = shepr_vt::lock_auxiliary(&self.decided);
        loop {
            if *decided {
                return false;
            }
            // clock-io-ok: the same real wait.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                *decided = true;
                self.changed.notify_all();
                return true;
            }
            decided = match self.changed.wait_timeout(decided, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn only_the_first_decision_wins() {
        let arbiter = PaneExitArbiter::default();
        assert!(arbiter.decide());
        assert!(!arbiter.decide());
        assert!(!arbiter.decide_after(Duration::ZERO));
    }

    #[test]
    fn an_undecided_grace_decides_at_its_deadline() {
        let arbiter = PaneExitArbiter::default();
        assert!(arbiter.decide_after(Duration::from_millis(10)));
        assert!(!arbiter.decide());
    }

    #[test]
    fn a_decision_during_the_grace_ends_it_at_once() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        let waiter = {
            let arbiter = Arc::clone(&arbiter);
            std::thread::spawn(move || {
                let started = Instant::now();
                (
                    arbiter.decide_after(Duration::from_secs(30)),
                    started.elapsed(),
                )
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        assert!(arbiter.decide());
        let (won, waited) = waiter.join().expect("waiter");
        assert!(!won);
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    }
}
