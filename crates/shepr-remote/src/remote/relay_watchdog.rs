//! Idle tracking for the SSH bridge's stdio relay. The wrapped streams carry
//! every remote keystroke and paste: input content must stay out of logs and
//! error messages here (byte counts and error kinds only, never buffers).

use std::io::{self, Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;

// Every client bridge runs this watchdog. The client health-checks the endpoint
// on the far side of the bridge: it sends HealthPing after
// `shepr_launch::connection_health::HEARTBEAT_INTERVAL` without received data,
// and the server answers HealthPong. The timing relation is asserted beside
// `BRIDGE_IDLE_TIMEOUT` in this crate's limits. That heartbeat renews this
// byte-level watchdog, so a healthy idle bridge stays connected while a dead
// client's bridge exits.

type BootClock = Arc<dyn Fn() -> io::Result<u64> + Send + Sync>;

#[derive(Clone)]
pub(super) struct Activity {
    last: Arc<AtomicU64>,
    clock: BootClock,
    _stop: mpsc::Sender<()>,
}

impl Activity {
    /// Starts the watchdog thread. `expired` runs once, on the watchdog thread,
    /// when no positive IO was seen for `timeout` (or the clock cannot be
    /// read), with the measured idle duration when available. The watchdog
    /// never waits on the relay copies: they may be blocked in a read or write
    /// that nothing can interrupt, so it only reports.
    pub(super) fn start(
        timeout: Duration,
        expired: impl FnOnce(Option<Duration>) + Send + 'static,
    ) -> io::Result<Self> {
        // The boot clock includes suspend, so short maintenance wakes can reap
        // old bridges.
        Self::start_with_clock(timeout, Arc::new(shepr_platform::boot_time_nanos), expired)
    }

    fn start_with_clock(
        timeout: Duration,
        clock: BootClock,
        expired: impl FnOnce(Option<Duration>) + Send + 'static,
    ) -> io::Result<Self> {
        let last = Arc::new(AtomicU64::new(clock()?));
        let watched = Arc::clone(&last);
        let watched_clock = Arc::clone(&clock);
        let (stop, stopped) = mpsc::channel();
        std::thread::Builder::new()
            .name("ssh-bridge-liveness".into())
            .spawn(move || {
                loop {
                    match stopped
                        .recv_timeout(timeout.min(crate::limits::BRIDGE_WATCHDOG_POLL_INTERVAL))
                    {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let idle_for = match watched_clock() {
                        Ok(now) => {
                            let Some(idle_for) = expired_idle(&watched, now, timeout) else {
                                continue;
                            };
                            Some(idle_for)
                        }
                        Err(_) => None,
                    };
                    expired(idle_for);
                    return;
                }
            })?;
        Ok(Self {
            last,
            clock,
            _stop: stop,
        })
    }

    fn record(&self) {
        // Activity is advisory: a failed clock read leaves the timestamp stale
        // for the watchdog instead of turning transferred bytes into an I/O error.
        if let Ok(now) = (self.clock)() {
            self.last.fetch_max(now, Ordering::Relaxed);
        }
    }
}

/// How long the relay has been idle at `now`, if that is at least `timeout`.
fn expired_idle(last: &AtomicU64, now: u64, timeout: Duration) -> Option<Duration> {
    let idle_for = Duration::from_nanos(now.saturating_sub(last.load(Ordering::Relaxed)));
    (idle_for >= timeout).then_some(idle_for)
}

pub(super) struct TrackedIo<T> {
    inner: T,
    activity: Option<Activity>,
}

impl<T> TrackedIo<T> {
    pub(super) fn new(inner: T, activity: Option<Activity>) -> Self {
        Self { inner, activity }
    }

    fn progressed(&self, count: usize) {
        if count > 0
            && let Some(activity) = &self.activity
        {
            activity.record();
        }
    }
}

impl<T: Read> Read for TrackedIo<T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.progressed(count);
        Ok(count)
    }
}

impl<T: Write> Write for TrackedIo<T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let count = self.inner.write(buffer)?;
        self.progressed(count);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::BRIDGE_IDLE_TIMEOUT as IDLE_TIMEOUT;

    #[test]
    fn idle_deadline_counts_elapsed_sleep_and_only_positive_io_renews_it() {
        let (stop, _stopped) = mpsc::channel();
        let last = Arc::new(AtomicU64::new(0));
        let activity = Activity {
            last: Arc::clone(&last),
            clock: Arc::new(|| Ok(42)),
            _stop: stop,
        };
        assert_eq!(
            expired_idle(
                &last,
                u64::try_from(IDLE_TIMEOUT.as_nanos()).unwrap_or(u64::MAX),
                IDLE_TIMEOUT
            ),
            Some(IDLE_TIMEOUT)
        );
        let mut reader = TrackedIo::new(&b"output"[..], Some(activity.clone()));
        reader.read_exact(&mut [0; 6]).expect("test precondition");
        let after_read = last.load(Ordering::Relaxed);
        assert_eq!(after_read, 42);
        assert_eq!(expired_idle(&last, after_read, IDLE_TIMEOUT), None);
        assert_eq!(reader.read(&mut [0; 1]).expect("test precondition"), 0);
        assert_eq!(last.load(Ordering::Relaxed), after_read);
        let mut writer = TrackedIo::new(Vec::new(), Some(activity));
        writer.write_all(b"ping").expect("test precondition");
        assert!(last.load(Ordering::Relaxed) >= after_read);
        assert!(
            expired_idle(
                &last,
                last.load(Ordering::Relaxed) + 120_000_000_000,
                IDLE_TIMEOUT
            )
            .is_some()
        );
    }

    /// The watchdog judges idleness only by the injected boot clock: it
    /// expires on the first reading a full timeout past the start, not on the
    /// one just short of it, and a failed reading expires it unmeasured.
    #[test]
    fn watchdog_expires_when_the_injected_clock_reaches_the_idle_timeout() {
        use std::sync::atomic::AtomicUsize;

        // Short, so each watchdog poll only waits this long in real time.
        let timeout = Duration::from_millis(20);
        let timeout_nanos = u64::try_from(timeout.as_nanos()).expect("test precondition");
        // Reading 0 is the start; readings past the script fail the clock.
        for (script, expected, expected_reads) in [
            (vec![0, timeout_nanos - 1, timeout_nanos], Some(timeout), 3),
            (vec![0], None, 2),
        ] {
            let reads = Arc::new(AtomicUsize::new(0));
            let clock_reads = Arc::clone(&reads);
            let clock: BootClock = Arc::new(move || {
                let read = clock_reads.fetch_add(1, Ordering::Relaxed);
                script
                    .get(read)
                    .copied()
                    .ok_or_else(|| io::Error::other("scripted clock failure"))
            });
            let (expired_tx, expired_rx) = mpsc::channel();
            let _activity = Activity::start_with_clock(timeout, clock, move |idle_for| {
                expired_tx
                    .send(idle_for)
                    .expect("the test is still waiting for the expiry");
            })
            .expect("test precondition");

            let idle_for = expired_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the watchdog expires");
            assert_eq!(idle_for, expected);
            assert_eq!(reads.load(Ordering::Relaxed), expected_reads);
        }
    }
}
