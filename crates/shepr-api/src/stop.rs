use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use tokio::sync::Notify;

/// The latch a server stop sets, and the wakeup that makes the server loop
/// look at it.
///
/// The latch alone is not enough: an idle server loop waits with no deadline,
/// so a stop that only set it would go unobserved until unrelated work woke
/// the loop, and `server.stop` would time out waiting for the socket to close.
/// [`Notify`] keeps a permit when nobody is waiting yet, so a request made
/// between two waits still wakes the next one.
#[derive(Debug, Default)]
pub struct ServerStopSignal {
    requested: AtomicBool,
    reason: Mutex<Option<StopReason>>,
    wake: Notify,
    final_save: Mutex<FinalSaveCompletion>,
    final_save_ready: Condvar,
}

#[derive(Debug, Default)]
struct FinalSaveCompletion {
    completed: bool,
    error: Option<String>,
    /// Stop requests that waited for the final save and have not yet written
    /// their answer.
    unanswered: usize,
}

/// Why the server is stopping, recorded by the first request and logged once
/// when shutdown begins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// SIGINT or SIGTERM, by name.
    Signal(&'static str),
    /// A `server.stop` or `server.stop_if_boot` request; `caller` describes
    /// the connecting process (pid and name) when it could be resolved.
    ApiStop { caller: Option<String> },
    /// The host announced a shutdown.
    HostShutdown,
    /// The socket listener failed or ended while the server ran.
    ListenerFailed,
    /// The server loop lost every API sender.
    ApiChannelClosed,
    /// The loop began shutting down with no request recorded.
    EventLoopExit,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal(name) => f.write_str(name),
            Self::ApiStop {
                caller: Some(caller),
            } => write!(f, "server stop request from {caller}"),
            Self::ApiStop { caller: None } => f.write_str("server stop request"),
            Self::HostShutdown => f.write_str("host shutdown"),
            Self::ListenerFailed => f.write_str("socket listener failed"),
            Self::ApiChannelClosed => f.write_str("API request channel closed"),
            Self::EventLoopExit => f.write_str("event loop exit"),
        }
    }
}

impl ServerStopSignal {
    /// Latches the stop and wakes the server loop. The first `reason` is kept;
    /// later requests do not replace it. The reason is recorded before the
    /// latch is set, so anything that sees the latch also sees a reason.
    pub fn request(&self, reason: StopReason) {
        match self.reason.lock() {
            Ok(mut recorded) => {
                recorded.get_or_insert(reason);
            }
            Err(poisoned) => {
                poisoned.into_inner().get_or_insert(reason);
            }
        }
        self.requested.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    /// The reason of the first stop request, if one was made.
    pub fn reason(&self) -> Option<StopReason> {
        match self.reason.lock() {
            Ok(recorded) => recorded.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Resolves once [`Self::request`] has been called since the last wakeup.
    pub async fn notified(&self) {
        self.wake.notified().await;
    }

    /// Publishes the final save result to stop requests waiting on this boot.
    pub fn complete_final_save(&self, error: Option<String>) {
        let mut completion = match self.final_save.lock() {
            Ok(completion) => completion,
            Err(poisoned) => poisoned.into_inner(),
        };
        completion.error = error;
        completion.completed = true;
        self.final_save_ready.notify_all();
    }

    /// Publishes `error` as the final save result unless a result was already
    /// published, and says whether it did. The server calls this on every exit
    /// path: an exit that never reached its final save would otherwise leave
    /// a waiting stop request to read a closed connection as an accepted stop.
    pub fn complete_unfinished_final_save(&self, error: &str) -> bool {
        let mut completion = match self.final_save.lock() {
            Ok(completion) => completion,
            Err(poisoned) => poisoned.into_inner(),
        };
        if completion.completed {
            return false;
        }
        completion.error = Some(error.to_owned());
        completion.completed = true;
        self.final_save_ready.notify_all();
        true
    }

    /// Waits up to `timeout` for the final save result after [`Self::request`]
    /// has been called; `None` when none was published in time. With a result
    /// the caller owes an answer: it reports it with [`Self::stop_answered`]
    /// once the answer is written or abandoned. Without one nothing is owed.
    pub(crate) fn wait_for_final_save(
        &self,
        timeout: std::time::Duration,
    ) -> Option<Option<String>> {
        let mut completion = match self.final_save.lock() {
            Ok(completion) => completion,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Counted before the wait, so the result's publisher cannot see no
        // answer owed between waking this waiter and its reading the result.
        completion.unanswered += 1;
        let (mut completion, _) =
            match self
                .final_save_ready
                .wait_timeout_while(completion, timeout, |completion| !completion.completed)
            {
                Ok(waited) => waited,
                Err(poisoned) => poisoned.into_inner(),
            };
        if !completion.completed {
            completion.unanswered = completion.unanswered.saturating_sub(1);
            self.final_save_ready.notify_all();
            return None;
        }
        Some(completion.error.clone())
    }

    /// Settles one answer owed since [`Self::wait_for_final_save`].
    pub(crate) fn stop_answered(&self) {
        let mut completion = match self.final_save.lock() {
            Ok(completion) => completion,
            Err(poisoned) => poisoned.into_inner(),
        };
        completion.unanswered = completion.unanswered.saturating_sub(1);
        self.final_save_ready.notify_all();
    }

    /// Waits up to `timeout` for every stop request holding the final save
    /// result to finish writing its answer, and says whether they all did. The
    /// server calls this after [`Self::complete_final_save`] and before it
    /// exits: an exit under an answer still being written would close the
    /// connection unanswered, and the stopping client would read that as a
    /// stop with no save failure to report.
    ///
    /// Accepted edge: a `server.stop` that arrives after this wait has run
    /// (the socket stays bound until the server releases it, and the runtime
    /// then gets a short shutdown timeout) reads the published result at once
    /// and may be cut off mid-write by the exit. If the final save failed,
    /// that second stopper sees a closed connection and counts the stop as
    /// accepted without the failure. It needs a second operator, retry or
    /// `stop --all` racing a local stop inside a window of milliseconds, and
    /// the first stopper still gets the full answer, so it is not closed.
    pub fn wait_for_stop_answers(&self, timeout: std::time::Duration) -> bool {
        let completion = match self.final_save.lock() {
            Ok(completion) => completion,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (completion, _) =
            match self
                .final_save_ready
                .wait_timeout_while(completion, timeout, |completion| completion.unanswered > 0)
            {
                Ok(waited) => waited,
                Err(poisoned) => poisoned.into_inner(),
            };
        completion.unanswered == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{ServerStopSignal, StopReason};

    #[test]
    fn the_first_stop_reason_wins() {
        let signal = ServerStopSignal::default();
        assert_eq!(signal.reason(), None);

        signal.request(StopReason::Signal("SIGTERM"));
        signal.request(StopReason::ApiStop { caller: None });

        assert_eq!(signal.reason(), Some(StopReason::Signal("SIGTERM")));
        assert_eq!(
            StopReason::ApiStop {
                caller: Some("pid 42 (shepr)".to_owned())
            }
            .to_string(),
            "server stop request from pid 42 (shepr)"
        );
    }

    #[tokio::test]
    async fn a_request_before_the_wait_still_wakes_it() {
        let signal = ServerStopSignal::default();
        assert!(!signal.is_requested());

        signal.request(StopReason::EventLoopExit);

        assert!(signal.is_requested());
        tokio::time::timeout(std::time::Duration::from_secs(5), signal.notified())
            .await
            .expect("the stored permit wakes the wait");
    }

    #[test]
    fn the_exit_waits_for_an_answer_owed_with_the_final_save_result() {
        let signal = ServerStopSignal::default();
        assert!(signal.wait_for_stop_answers(std::time::Duration::ZERO));

        signal.request(StopReason::EventLoopExit);
        signal.complete_final_save(Some("disk full".to_owned()));
        assert_eq!(
            signal.wait_for_final_save(std::time::Duration::ZERO),
            Some(Some("disk full".to_owned()))
        );
        assert!(
            !signal.wait_for_stop_answers(std::time::Duration::ZERO),
            "an answer holding the result is still owed"
        );

        signal.stop_answered();
        assert!(signal.wait_for_stop_answers(std::time::Duration::ZERO));
    }

    #[test]
    fn a_wait_with_no_result_in_time_gives_up_and_owes_no_answer() {
        let signal = ServerStopSignal::default();
        signal.request(StopReason::EventLoopExit);
        assert_eq!(signal.wait_for_final_save(std::time::Duration::ZERO), None);
        assert!(signal.wait_for_stop_answers(std::time::Duration::ZERO));
    }

    #[test]
    fn an_unfinished_completion_never_overwrites_a_published_result() {
        let signal = ServerStopSignal::default();
        assert!(signal.complete_unfinished_final_save("exited early"));
        assert_eq!(
            signal.wait_for_final_save(std::time::Duration::ZERO),
            Some(Some("exited early".to_owned()))
        );
        signal.stop_answered();

        let signal = ServerStopSignal::default();
        signal.complete_final_save(None);
        assert!(!signal.complete_unfinished_final_save("exited early"));
        assert_eq!(
            signal.wait_for_final_save(std::time::Duration::ZERO),
            Some(None)
        );
    }
}
