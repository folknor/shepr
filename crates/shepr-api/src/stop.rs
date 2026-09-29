use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// The latch a server stop sets, and the wakeup that makes the server loop
/// look at it.
///
/// The latch alone is not enough: an idle server loop waits with no deadline,
/// so a stop that only set it would go unobserved until unrelated work woke
/// the loop, and `server.stop` would time out waiting for sockets to close.
/// [`Notify`] keeps a permit when nobody is waiting yet, so a request made
/// between two waits still wakes the next one.
#[derive(Debug, Default)]
pub struct ServerStopSignal {
    requested: AtomicBool,
    wake: Notify,
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
}
