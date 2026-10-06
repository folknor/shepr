//! Timeouts and capacity bounds of launching, probing and stopping a server,
//! and the heartbeat cadence of a connection to one.

use std::time::Duration;

// Keep budgets with the layer that owns the operation. API, SSH and server
// limits cannot all move here without reversing the crate dependency graph;
// exported worst-case budgets below let higher layers assert their ordering.

/// A connected client probes an endpoint that keeps a heartbeat after this
/// much silence, and the server answers. The interval leaves room for routine
/// SSH and server scheduling delays. It is the one timing fact both ends of a
/// connection share: anything that relays the connection and expires it when
/// no byte moves, as the remote host's SSH bridge does, must wait several of
/// these intervals before it gives up, and asserts that against this value.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// How often lifecycle waits check sockets, child processes and the launch lock.
/// The interval notices a daemon that died during boot and makes startup
/// visible promptly without a busy wait.
pub(crate) const LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Least time between two starts of the server daemon in one launch. A daemon
/// that found the data directory held while nothing listened (the holder was
/// stopping, or still booting) is started again once this has passed, so a
/// holder that is only leaving never fails the launch, and a holder that stays
/// is not asked every poll.
pub(crate) const DAEMON_RESTART_INTERVAL: Duration = Duration::from_millis(500);

/// Time allowed for one status request to a local server. This uses the API
/// client's shared ping budget, which bounds connect, write and response.
pub const STATUS_REQUEST_TIMEOUT: Duration = shepr_api::client::STATUS_REQUEST_TIMEOUT;

/// The most a launched server's boot log may hold. A launch that finds more
/// (a server printing without end while it boots) fails and kills the server,
/// and one that boots successfully empties the log. The cap keeps a runaway
/// server from filling the runtime directory, which is usually a small tmpfs.
pub(crate) const BOOT_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// How many panics a launched server whose own log could not be opened
/// reports to its boot log after readiness, its only record then. The first
/// reports are the ones that explain a failure; the cap keeps a panic that
/// repeats for the server's life from growing the file without limit.
pub const BOOT_LOG_PANIC_REPORTS: usize = 8;

/// Time allowed for the sibling `shepr-server --version` that `status client`
/// runs to report the installed pair. It prints one line and exits, so a longer
/// wait means a broken or hung binary; the deadline keeps a remote discovery
/// probe from hanging on it.
pub(crate) const SIBLING_VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// The most of the sibling's `--version` output that is read. The real output
/// is one short line; the cap bounds what a wrong binary can make us hold.
pub(crate) const SIBLING_VERSION_OUTPUT_BYTES: u64 = 512;

/// Maximum time for a newly spawned server to answer a status request with
/// this build's identity. The deadline allows normal startup while keeping a
/// failed launch finite.
pub const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Slack added to [`SERVER_READY_TIMEOUT`] for a client waiting on the launch
/// lock. The holder may spend its whole readiness window launching, so a
/// waiter that gave up sooner would fail a launch that is about to succeed.
pub(crate) const LAUNCH_LOCK_WAIT_GRACE: Duration = Duration::from_secs(5);

/// Maximum time a server stop waits for the named server to stop answering, or
/// for the socket to disappear when the stop was not conditional.
pub(crate) const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum time a server stop waits for a data-directory lease after the
/// stopped server no longer answers or its socket is gone. The server
/// releases its lease before removing its socket; a later holder may be a new
/// process using the same data directory.
pub(crate) const STOP_LEASE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-request deadline while polling the server's boot identity after a stop.
/// These repeated observations share the stop's overall deadline; keeping each
/// short lets socket disappearance or a replacement boot be noticed promptly.
/// They deliberately do not grant each poll the ordinary status request window.
pub(crate) const STOP_STATUS_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Poll interval while waiting for a server to stop answering or its socket
/// to disappear. It bounds shutdown detection latency without rapid repeated
/// probes.
pub(crate) const STOP_WAIT_POLL: Duration = Duration::from_millis(25);

/// How many times one startup restart offer asks about a server of another
/// build. A server replaced between the observation and the stop is a new
/// occupant and is offered once more; beyond that something keeps restarting
/// it.
pub const MAX_RESTART_OFFERS: usize = 2;

/// Status counts are optional; a stalled application loop must not hold up a probe.
pub const STATUS_SUMMARY_TIMEOUT: Duration = STATUS_REQUEST_TIMEOUT;

/// Maximum socket work performed by a status overview (ping and optional counts).
pub const STATUS_OVERVIEW_TIMEOUT: Duration =
    STATUS_REQUEST_TIMEOUT.saturating_add(STATUS_SUMMARY_TIMEOUT);

/// Stop's socket wait, subsequent lease wait and final identity probe.
pub const STOP_WORST_CASE: Duration = STOP_WAIT_TIMEOUT
    .saturating_add(STOP_LEASE_WAIT_TIMEOUT)
    .saturating_add(STOP_STATUS_PROBE_TIMEOUT);

/// A runtime-address launch can probe, wait for the lock, wait for an existing
/// socket to settle, and then give its own daemon a full readiness window.
/// Probes at phase boundaries and the last readiness poll also need time.
pub const START_WORST_CASE: Duration = SERVER_READY_TIMEOUT
    .saturating_mul(3)
    .saturating_add(LAUNCH_LOCK_WAIT_GRACE)
    .saturating_add(STATUS_REQUEST_TIMEOUT.saturating_mul(3));
