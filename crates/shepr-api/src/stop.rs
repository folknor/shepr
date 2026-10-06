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

impl ServerStopSignal {
    /// Latches the stop and wakes the server loop.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.wake.notify_one();
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

    /// Waits for the final save result after [`Self::request`] has been called.
    /// The caller then owes an answer: it reports it with
    /// [`Self::stop_answered`] once the answer is written or abandoned.
    pub(crate) fn wait_for_final_save(&self) -> Option<String> {
        let mut completion = match self.final_save.lock() {
            Ok(completion) => completion,
            Err(poisoned) => poisoned.into_inner(),
        };
        completion.unanswered += 1;
        while !completion.completed {
            completion = match self.final_save_ready.wait(completion) {
                Ok(completion) => completion,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
        completion.error.clone()
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
    use super::ServerStopSignal;

    #[tokio::test]
    async fn a_request_before_the_wait_still_wakes_it() {
        let signal = ServerStopSignal::default();
        assert!(!signal.is_requested());

        signal.request();

        assert!(signal.is_requested());
        tokio::time::timeout(std::time::Duration::from_secs(5), signal.notified())
            .await
            .expect("the stored permit wakes the wait");
    }

    #[test]
    fn the_exit_waits_for_an_answer_owed_with_the_final_save_result() {
        let signal = ServerStopSignal::default();
        assert!(signal.wait_for_stop_answers(std::time::Duration::ZERO));

        signal.request();
        signal.complete_final_save(Some("disk full".to_owned()));
        assert_eq!(signal.wait_for_final_save().as_deref(), Some("disk full"));
        assert!(
            !signal.wait_for_stop_answers(std::time::Duration::ZERO),
            "an answer holding the result is still owed"
        );

        signal.stop_answered();
        assert!(signal.wait_for_stop_answers(std::time::Duration::ZERO));
    }
}
