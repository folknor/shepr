//! A panic anywhere in the client is a bug, and the client never carries on
//! after one: it holds no state the server does not, so ending cleanly and
//! letting the operator relaunch costs nothing. One owner finalizes: the code
//! after the client loop in `run_launched_client`. The panic hook only records
//! the panic and wakes the loop, so a helper thread's panic is never followed
//! by the loop drawing to a terminal the hook has already restored.
//!
//! The limits are stated, not fixed: a loop blocked in a synchronous call (a
//! terminal write, a preference store) sees the latch only once that call
//! returns, and a panic after the exit status is chosen cannot change it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::Notify;

#[derive(Default)]
pub(crate) struct FatalPanic {
    latched: AtomicBool,
    wake: Notify,
    /// The first panic's message, location, thread and backtrace.
    diagnostic: OnceLock<String>,
}

impl FatalPanic {
    /// Installs the process panic hook that records into a new latch. The
    /// hook does no logging and no terminal IO: a panic inside the logger's
    /// own critical section would deadlock on it, and stderr is the alternate
    /// screen until the terminal is restored. The owner logs and prints the
    /// diagnostic after finalization.
    pub(crate) fn install() -> Arc<Self> {
        let fatal = Arc::new(Self::default());
        let hook_fatal = Arc::clone(&fatal);
        std::panic::set_hook(Box::new(move |info| hook_fatal.record(info)));
        fatal
    }

    fn record(&self, info: &std::panic::PanicHookInfo<'_>) {
        // Only string payloads are formatted; any other type could run
        // arbitrary code inside the hook.
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("a panic with a non-string payload");
        let location = info
            .location()
            .map_or_else(|| "an unknown location".to_owned(), ToString::to_string);
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("an unnamed thread");
        let backtrace = std::backtrace::Backtrace::force_capture();
        self.diagnostic
            .set(format!(
                "internal error: panicked at {location} on {thread}: {payload}\n{backtrace}"
            ))
            .ok();
        self.latch();
    }

    /// Latches the fatal condition and wakes the loop. Also used for a panic
    /// caught by the owner itself, which does not depend on the hook still
    /// being installed.
    pub(crate) fn latch(&self) {
        self.latched.store(true, Ordering::Release);
        // `notify_one` keeps a permit if the loop is not waiting yet.
        self.wake.notify_one();
    }

    pub(crate) fn is_latched(&self) -> bool {
        self.latched.load(Ordering::Acquire)
    }

    /// Resolves once a panic is latched.
    pub(crate) async fn latched(&self) {
        while !self.is_latched() {
            self.wake.notified().await;
        }
    }

    pub(crate) fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.get().map(String::as_str)
    }

    /// Runs `stage` and, if it panics, latches and forgets the payload instead
    /// of dropping it: a payload's destructor can itself panic, which would
    /// skip every later stage. The hook already recorded the panic.
    pub(crate) fn guard<R>(&self, stage: impl FnOnce() -> R) -> Option<R> {
        #[expect(
            clippy::disallowed_methods,
            reason = "finalization must run every stage even after one panics; the panic is \
                      recorded and the client exits unsuccessfully"
        )]
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(stage));
        match result {
            Ok(value) => Some(value),
            Err(payload) => {
                std::mem::forget(payload);
                self.latch();
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caught_stage_panic_latches_and_later_stages_still_run() {
        let fatal = FatalPanic::default();
        assert_eq!(fatal.guard(|| 1), Some(1));
        assert!(!fatal.is_latched());
        let caught: Option<()> = fatal.guard(|| panic!("stage failed"));
        assert_eq!(caught, None);
        assert!(fatal.is_latched());
        assert_eq!(fatal.guard(|| 2), Some(2));
    }

    #[tokio::test]
    async fn a_latch_before_the_wait_still_wakes_it() {
        let fatal = FatalPanic::default();
        fatal.latch();
        tokio::time::timeout(std::time::Duration::from_secs(1), fatal.latched())
            .await
            .expect("latched");
    }
}
