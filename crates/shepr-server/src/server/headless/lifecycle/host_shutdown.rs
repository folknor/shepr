//! Host shutdown notification from logind.
//!
//! logind announces a shutdown with `PrepareForShutdown(true)`, then waits
//! for every delay inhibitor to be released (or for `InhibitDelayMaxSec`)
//! before it starts killing processes. It may also call the shutdown off
//! again with `PrepareForShutdown(false)`.
//!
//! The monitor holds a delay inhibitor while no shutdown is pending. On a
//! warning it sets the server's `requested` flag and wakes the server, which
//! writes a checkpoint and then calls [`HostShutdownMonitor::release_delay_lock`];
//! only then is the inhibitor dropped, so the shutdown is held up exactly as
//! long as the checkpoint takes. A connection made after shutdown preparation
//! began cannot acquire a delay inhibitor; it still reports the warning and
//! asks the server to checkpoint, but cannot delay that shutdown. The monitor
//! keeps watching: on a cancellation it clears the flag, wakes the server
//! again and takes a fresh inhibitor for the next warning.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures_util::StreamExt;
use tokio::sync::watch;

use super::WarningGeneration;
use crate::backoff::Backoff;
use crate::limits::{SHUTDOWN_RECONNECT_INITIAL_DELAY, SHUTDOWN_RECONNECT_MAX_DELAY};

/// Watches logind for host shutdown warnings and cancellations. Dropping it
/// stops the watch and releases any delay inhibitor it holds.
pub(in crate::server::headless) struct HostShutdownMonitor {
    task: tokio::task::JoinHandle<()>,
    shared: Arc<Shared>,
    /// The warning generation the server has checkpointed for.
    checkpointed: watch::Sender<Option<WarningGeneration>>,
}

impl HostShutdownMonitor {
    /// Start watching. `requested` is set while a host shutdown is pending
    /// and cleared when it is cancelled; `wake` runs after every change.
    /// Must be called inside a tokio runtime.
    pub(in crate::server::headless) fn start(
        requested: Arc<AtomicBool>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            requested,
            generation: AtomicU64::new(0),
            wake: Box::new(wake),
        });
        let (checkpointed, checkpoints) = watch::channel(None);
        let task = tokio::spawn(monitor(Arc::clone(&shared), checkpoints));
        Self {
            task,
            shared,
            checkpointed,
        }
    }

    /// Generation associated with the shutdown warning currently pending.
    pub(in crate::server::headless) fn warning_generation(&self) -> Option<WarningGeneration> {
        self.shared.warning_generation()
    }

    /// Tell the monitor that the session checkpoint answering the current
    /// shutdown warning is on disk, so it can release its delay inhibitor
    /// and let the shutdown proceed. A call with no warning pending, or one
    /// that races a cancellation, is ignored: each warning is numbered, and
    /// only a release for the warning still pending counts.
    pub(in crate::server::headless) fn release_delay_lock(&self, generation: WarningGeneration) {
        if self.warning_generation() == Some(generation) {
            self.checkpointed.send_replace(Some(generation));
        }
    }
}

impl Drop for HostShutdownMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Shared {
    requested: Arc<AtomicBool>,
    /// Counts warnings; bumped before `requested` is set, so a server that
    /// sees the flag and then reads this gets the warning it answered (or a
    /// newer one).
    generation: AtomicU64,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    /// Report a pending shutdown, once per warning.
    fn announce(&self) {
        if self.requested.load(Ordering::Acquire) {
            return;
        }
        let generation = self.start_warning();
        tracing::info!(
            event = "host.shutdown.request",
            subsystem = "shutdown",
            outcome = "pending",
            generation = generation.as_u64(),
            "host shutdown requested; preserving session before pane termination"
        );
    }

    /// Refresh a warning after reconnecting if the flag could have hidden a
    /// cancellation and a second warning while logind was unavailable.
    fn refresh_warning(&self) {
        let generation = self.start_warning();
        tracing::info!(
            event = "host.shutdown.refresh",
            subsystem = "shutdown",
            outcome = "pending",
            generation = generation.as_u64(),
            "host shutdown remains pending after reconnect; refreshing session checkpoint"
        );
    }

    fn start_warning(&self) -> WarningGeneration {
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.requested.store(true, Ordering::Release);
        (self.wake)();
        WarningGeneration(generation)
    }

    /// Report that the pending shutdown was called off.
    fn cancel(&self) {
        if self.requested.swap(false, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    /// The identity of the shutdown warning currently pending.
    fn warning_generation(&self) -> Option<WarningGeneration> {
        self.requested
            .load(Ordering::Acquire)
            .then(|| WarningGeneration(self.generation.load(Ordering::Acquire)))
    }

    /// Whether the server checkpointed the warning now pending.
    fn checkpointed(&self, released_generation: Option<WarningGeneration>) -> bool {
        self.requested.load(Ordering::Acquire) && released_generation == self.warning_generation()
    }
}

async fn monitor(shared: Arc<Shared>, mut checkpoints: watch::Receiver<Option<WarningGeneration>>) {
    let reconnect_backoff = Backoff::new(
        SHUTDOWN_RECONNECT_INITIAL_DELAY,
        SHUTDOWN_RECONNECT_MAX_DELAY,
    );
    let mut failures = 0_u32;
    let mut refresh_pending_warning = false;
    // The sender lives in the handle, whose drop also aborts this task; a
    // closed channel only means the abort has not landed yet.
    while checkpoints.has_changed().is_ok() {
        match watch_shutdown(&shared, &mut checkpoints, refresh_pending_warning).await {
            Ok(()) => {
                failures = 0;
                refresh_pending_warning = shared.requested.load(Ordering::Acquire);
            }
            Err(err) => {
                let shutdown_pending = shared.requested.load(Ordering::Acquire);
                refresh_pending_warning |= shutdown_pending;
                // A pending shutdown keeps retries at the initial delay and
                // restarts the count, so once the warning clears ordinary
                // absence backs off again from the initial delay.
                let failures_before = if shutdown_pending { 0 } else { failures };
                let retry_delay = reconnect_backoff.delay_after(failures_before);
                failures = if shutdown_pending {
                    0
                } else {
                    failures.saturating_add(1)
                };
                // Losing logind while a shutdown is pending may cost the session
                // checkpoint, so that case warns. Without one pending it is
                // routine on hosts without a system bus and would repeat every
                // retry for the life of the server, so it stays diagnostic.
                if shutdown_pending {
                    tracing::warn!(
                        event = "host.shutdown.notification",
                        subsystem = "shutdown",
                        outcome = "unavailable",
                        shutdown_pending,
                        generation = shared.generation.load(Ordering::Acquire),
                        error = %err,
                        retry_seconds = retry_delay.as_secs(),
                        "host shutdown notification unavailable"
                    );
                } else {
                    tracing::debug!(
                        event = "host.shutdown.notification",
                        subsystem = "shutdown",
                        outcome = "unavailable",
                        shutdown_pending,
                        generation = shared.generation.load(Ordering::Acquire),
                        error = %err,
                        retry_seconds = retry_delay.as_secs(),
                        "host shutdown notification unavailable"
                    );
                }
                // A lost signal stream leaves cancellation unobservable until
                // reconnecting, so retry promptly while a warning is pending.
                tokio::time::sleep(retry_delay).await;
            }
        }
    }
}

async fn watch_shutdown(
    shared: &Shared,
    checkpoints: &mut watch::Receiver<Option<WarningGeneration>>,
    refresh_pending_warning: bool,
) -> zbus::Result<()> {
    let connection = zbus::Connection::system().await?;
    watch_connection(connection, shared, checkpoints, refresh_pending_warning).await
}

async fn take_inhibitor(manager: &zbus::Proxy<'_>) -> zbus::Result<zbus::zvariant::OwnedFd> {
    manager
        .call(
            "Inhibit",
            &(
                "shutdown",
                "Shepr",
                "Save terminal workspace layout",
                "delay",
            ),
        )
        .await
}

/// Acquire a delay inhibitor unless logind is already preparing for shutdown.
/// The state property is checked first; if shutdown begins between that read
/// and `Inhibit`, logind reports `OperationInProgress` and the warning still
/// needs to reach the server even though the operation can no longer be held.
async fn take_inhibitor_if_idle(
    manager: &zbus::Proxy<'_>,
    preparing: &mut bool,
) -> zbus::Result<Option<zbus::zvariant::OwnedFd>> {
    if *preparing {
        return Ok(None);
    }
    match take_inhibitor(manager).await {
        Ok(inhibitor) => Ok(Some(inhibitor)),
        Err(error) if shutdown_already_in_progress(&error) => {
            *preparing = true;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn shutdown_already_in_progress(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.login1.OperationInProgress"
    )
}

/// Follow one logind connection until it drops. Returns `Ok` when logind
/// goes away (the caller reconnects) and `Err` when a call fails.
async fn watch_connection(
    connection: zbus::Connection,
    shared: &Shared,
    checkpoints: &mut watch::Receiver<Option<WarningGeneration>>,
    refresh_pending_warning: bool,
) -> zbus::Result<()> {
    let manager = zbus::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let mut owners = manager.receive_owner_changed().await?;
    let mut signals = manager.receive_signal("PrepareForShutdown").await?;
    let mut preparing: bool = manager.get_property("PreparingForShutdown").await?;
    let mut inhibitor = take_inhibitor_if_idle(&manager, &mut preparing).await?;
    tracing::debug!("host shutdown notification ready");

    if preparing {
        if refresh_pending_warning && shared.requested.load(Ordering::Acquire) {
            shared.refresh_warning();
        } else {
            shared.announce();
        }
    }
    loop {
        if preparing {
            shared.announce();
            if inhibitor.is_some() && shared.checkpointed(*checkpoints.borrow_and_update()) {
                tracing::debug!(
                    event = "host.shutdown.delay_release",
                    subsystem = "shutdown",
                    generation = shared.generation.load(Ordering::Acquire),
                    "session checkpoint finished; releasing the shutdown delay lock"
                );
                inhibitor = None;
            }
        } else {
            shared.cancel();
            if inhibitor.is_none() {
                inhibitor = take_inhibitor_if_idle(&manager, &mut preparing).await?;
                if preparing {
                    shared.announce();
                }
            }
        }
        tokio::select! {
            biased;
            _ = owners.next() => break,
            signal = signals.next() => {
                let Some(signal) = signal else { break };
                preparing = signal.body().deserialize()?;
            }
            changed = checkpoints.changed() => {
                if changed.is_err() {
                    // The handle is gone; see `monitor`.
                    break;
                }
            }
        }
    }
    drop(inhibitor);
    Ok(())
}

#[cfg(test)]
impl HostShutdownMonitor {
    /// A warning source without a bus, retaining the real generation and
    /// checkpoint acknowledgement machinery. The lifecycle regression needs
    /// only this boundary; injecting bus connections and reconnect delays
    /// would couple that test to transport behavior it does not exercise.
    pub(super) fn test_warning(
        requested: Arc<AtomicBool>,
    ) -> (Self, watch::Receiver<Option<WarningGeneration>>) {
        let shared = Arc::new(Shared {
            requested,
            generation: AtomicU64::new(0),
            wake: Box::new(|| {}),
        });
        let (checkpointed, checkpoints) = watch::channel(None);
        shared.announce();
        let task = tokio::spawn(std::future::pending());
        (
            Self {
                task,
                shared,
                checkpointed,
            },
            checkpoints,
        )
    }

    pub(super) fn test_refresh_warning(&self) {
        self.shared.refresh_warning();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read};
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Stdio};
    use std::sync::Mutex;
    use std::time::Duration;

    #[tokio::test]
    async fn stale_checkpoint_cannot_release_a_new_warning() {
        let wakes = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(shared(&wakes));
        let (checkpointed, receiver) = watch::channel(None);
        let monitor = HostShutdownMonitor {
            task: tokio::spawn(std::future::pending()),
            shared,
            checkpointed,
        };
        monitor.shared.announce();
        let first = monitor
            .warning_generation()
            .expect("the first warning is pending");
        monitor.shared.cancel();
        monitor.shared.announce();
        let second = monitor
            .warning_generation()
            .expect("the second warning is pending");
        assert_ne!(first, second);
        monitor.release_delay_lock(first);
        assert_eq!(*receiver.borrow(), None);
        monitor.release_delay_lock(second);
        assert_eq!(*receiver.borrow(), Some(second));
    }

    fn shared(wakes: &Arc<AtomicU64>) -> Shared {
        let wakes = Arc::clone(wakes);
        Shared {
            requested: Arc::new(AtomicBool::new(false)),
            generation: AtomicU64::new(0),
            wake: Box::new(move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }),
        }
    }

    #[test]
    fn warnings_and_cancellations_are_reported_once_each() {
        let wakes = Arc::new(AtomicU64::new(0));
        let shared = shared(&wakes);
        shared.cancel();
        assert_eq!(wakes.load(Ordering::Relaxed), 0, "nothing to cancel");
        shared.announce();
        shared.announce();
        assert!(shared.requested.load(Ordering::Acquire));
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        shared.cancel();
        shared.cancel();
        assert!(!shared.requested.load(Ordering::Acquire));
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_release_only_counts_for_the_warning_still_pending() {
        let wakes = Arc::new(AtomicU64::new(0));
        let shared = shared(&wakes);
        assert!(!shared.checkpointed(None), "no warning, nothing to release");
        shared.announce();
        let first = shared.warning_generation().expect("warning is pending");
        assert!(!shared.checkpointed(None), "no checkpoint yet");
        assert!(shared.checkpointed(Some(first)));
        // The server's release for the first warning arrives after it was
        // cancelled and a second one began: it must not free the second.
        shared.cancel();
        shared.announce();
        assert!(!shared.checkpointed(Some(first)));
        assert!(shared.checkpointed(shared.warning_generation()));
    }

    struct PrivateBus(Child);

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let killed = self.0.kill();
            let reaped = self.0.wait();
            // Panicking again while a failed test unwinds would abort the
            // whole test binary and hide the original failure.
            if !std::thread::panicking() {
                killed.expect("kill the private bus daemon");
                reaped.expect("reap the private bus daemon");
            }
        }
    }

    struct LoginManager {
        preparing: Arc<AtomicBool>,
        start_preparing_on_first_inhibit: AtomicBool,
        /// The peer end of every inhibitor handed out, newest last.
        peers: Arc<Mutex<Vec<UnixStream>>>,
    }

    #[derive(Debug, zbus::DBusError)]
    #[zbus(prefix = "org.freedesktop.login1")]
    enum LoginManagerError {
        #[zbus(name = "OperationInProgress")]
        OperationInProgress,
        Failed(String),
    }

    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl LoginManager {
        fn inhibit(
            &self,
            what: &str,
            _who: &str,
            _why: &str,
            mode: &str,
        ) -> Result<zbus::zvariant::OwnedFd, LoginManagerError> {
            assert_eq!((what, mode), ("shutdown", "delay"));
            if self.preparing.load(Ordering::Acquire)
                || self
                    .start_preparing_on_first_inhibit
                    .swap(false, Ordering::AcqRel)
            {
                self.preparing.store(true, Ordering::Release);
                return Err(LoginManagerError::OperationInProgress);
            }
            let (lock, peer) =
                UnixStream::pair().map_err(|err| LoginManagerError::Failed(err.to_string()))?;
            peer.set_nonblocking(true)
                .map_err(|err| LoginManagerError::Failed(err.to_string()))?;
            self.peers
                .lock()
                .map_err(|err| LoginManagerError::Failed(err.to_string()))?
                .push(peer);
            Ok(std::os::fd::OwnedFd::from(lock).into())
        }

        #[zbus(property)]
        fn preparing_for_shutdown(&self) -> bool {
            self.preparing.load(Ordering::Acquire)
        }
    }

    /// Whether the inhibitor behind `peer` is still held.
    fn held(peer: &mut UnixStream) -> bool {
        match peer.read(&mut [0]) {
            Ok(0) => false,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => true,
            other => panic!("unexpected inhibitor peer read: {other:?}"),
        }
    }

    async fn emit(service: &zbus::Connection, preparing: bool) {
        service
            .emit_signal(
                None::<&str>,
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "PrepareForShutdown",
                &preparing,
            )
            .await
            .expect("test precondition");
    }

    // These waits observe a private D-Bus daemon and its real Unix-fd peers.
    // Their Tokio sleeps yield while external I/O changes; the timeouts guard
    // the integration harness rather than the retry-delay policy.
    async fn inhibitor_count(peers: &Mutex<Vec<UnixStream>>, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while peers.lock().expect("test precondition").len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("inhibitor taken");
    }

    #[tokio::test]
    // zbus 5.19 accepts UnixStream, but its server/peer setup is behind `p2p`,
    // disabled by this workspace. A peer pair also lacks the bus-generated
    // NameOwnerChanged signals this watch consumes; this test's `false`
    // argument also leaves the reconnect-only warning refresh untested.
    #[ignore = "requires dbus-daemon; uses a private bus, never requests host shutdown"]
    async fn delay_lock_is_held_until_checkpoint_and_retaken_after_cancellation() {
        for (already_preparing, shutdown_races_inhibit) in
            [(false, false), (true, false), (false, true)]
        {
            let mut bus = PrivateBus(
                shepr_test_support::command_in_scratch("dbus-daemon", "private-dbus")
                    .args(["--session", "--nofork", "--print-address=1"])
                    .stdout(Stdio::piped())
                    .spawn()
                    .expect("test precondition"),
            );
            let mut address = String::new();
            std::io::BufReader::new(bus.0.stdout.take().expect("test precondition"))
                .read_line(&mut address)
                .expect("test precondition");
            let peers = Arc::new(Mutex::new(Vec::new()));
            let preparing = Arc::new(AtomicBool::new(already_preparing));
            let service = zbus::connection::Builder::address(address.trim())
                .expect("test precondition")
                .name("org.freedesktop.login1")
                .expect("test precondition")
                .serve_at(
                    "/org/freedesktop/login1",
                    LoginManager {
                        preparing: Arc::clone(&preparing),
                        start_preparing_on_first_inhibit: AtomicBool::new(shutdown_races_inhibit),
                        peers: Arc::clone(&peers),
                    },
                )
                .expect("test precondition")
                .build()
                .await
                .expect("test precondition");
            let client = zbus::connection::Builder::address(address.trim())
                .expect("test precondition")
                .build()
                .await
                .expect("test precondition");
            let requested = Arc::new(AtomicBool::new(false));
            let wake = Arc::new(tokio::sync::Notify::new());
            let shared = Arc::new(Shared {
                requested: Arc::clone(&requested),
                generation: AtomicU64::new(0),
                wake: Box::new({
                    let wake = Arc::clone(&wake);
                    move || wake.notify_one()
                }),
            });
            let (checkpointed, mut checkpoints) = watch::channel(None);
            let task = tokio::spawn({
                let shared = Arc::clone(&shared);
                async move {
                    watch_connection(client, &shared, &mut checkpoints, false)
                        .await
                        .expect("test precondition");
                }
            });

            let starts_without_lock = already_preparing || shutdown_races_inhibit;
            if !starts_without_lock {
                inhibitor_count(&peers, 1).await;
                assert!(!requested.load(Ordering::Acquire));
                emit(&service, true).await;
            }
            tokio::time::timeout(Duration::from_secs(5), wake.notified())
                .await
                .expect("warning reported");
            assert!(requested.load(Ordering::Acquire));
            if starts_without_lock {
                assert!(peers.lock().expect("test precondition").is_empty());
                if shutdown_races_inhibit {
                    emit(&service, true).await;
                }
            } else {
                assert!(
                    held(&mut peers.lock().expect("test precondition")[0]),
                    "shutdown must remain inhibited while the server saves"
                );
            }

            // The server has checkpointed: the lock goes, the watch stays.
            checkpointed.send_replace(shared.warning_generation());
            if !starts_without_lock {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while held(&mut peers.lock().expect("test precondition")[0]) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("delay lock released after the checkpoint");
            }
            assert!(!task.is_finished());

            // The shutdown is called off: reported, and a new lock is taken.
            preparing.store(false, Ordering::Release);
            emit(&service, false).await;
            tokio::time::timeout(Duration::from_secs(5), wake.notified())
                .await
                .expect("cancellation reported");
            assert!(!requested.load(Ordering::Acquire));
            let expected_inhibitors = if starts_without_lock { 1 } else { 2 };
            inhibitor_count(&peers, expected_inhibitors).await;
            assert!(held(
                &mut peers.lock().expect("test precondition")[expected_inhibitors - 1]
            ));

            task.abort();
            assert!(task.await.expect_err("test precondition").is_cancelled());
            assert!(!held(
                &mut peers.lock().expect("test precondition")[expected_inhibitors - 1]
            ));
        }
    }
}
