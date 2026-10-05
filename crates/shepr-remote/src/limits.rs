//! Timeouts, capacity bounds, and SSH policy values for remote connections.

use std::time::Duration;

/// Maximum UTF-8 bytes in an absolute remote executable path. The bound
/// admits normal paths while rejecting unexpectedly large host output before
/// it is reused in a shell command.
pub(crate) const MAX_REMOTE_EXECUTABLE_BYTES: usize = 4096;

/// Maximum bytes read from one cached remote metadata file. Discovery hints
/// contain only a few paths or names, so the cap leaves ample room without
/// letting a corrupt cache consume unbounded memory.
pub(crate) const MAX_METADATA_BYTES: u64 = 16 * 1024;

/// Time allowed for the SSH command that stops a remote server. The remote
/// `shepr stop` itself waits up to its own stop deadline for the server to
/// close its socket; this covers that plus the connection.
pub(crate) const REMOTE_STOP_SSH_TIMEOUT: Duration =
    shepr_launch::limits::STOP_WORST_CASE.saturating_add(SSH_COMMAND_TIMEOUT);

/// How often a remote wait for a server checks the server while its runtime
/// directory is watched and nothing is there. The directory watch ends the
/// wait as soon as a socket appears, so this only covers an event the watch
/// could miss.
pub(crate) const SERVER_WAIT_RECHECK: Duration = Duration::from_secs(30);

/// How often a remote wait for a server checks a server that is there but not
/// ready yet (still restoring, stopping, or slow to answer). Its socket does
/// not change again when it becomes ready, so only this check sees it.
pub(crate) const SERVER_WAIT_SETTLING_RECHECK: Duration = Duration::from_millis(500);

/// How often a remote wait for a server checks while the runtime directory
/// does not exist yet and so cannot be watched. A local check on that host,
/// not an SSH round trip.
pub(crate) const SERVER_WAIT_UNWATCHED_RECHECK: Duration = Duration::from_secs(2);

/// The longest a remote wait for a server runs before it exits and the client
/// starts another. It bounds how long a wait whose client vanished without
/// closing its stdin can linger on the host.
pub(crate) const SERVER_WAIT_MAX: Duration = Duration::from_secs(60 * 60);

/// Bytes read at a time from a waiting client's stdin, which carries nothing
/// but its close.
pub(crate) const SERVER_WAIT_INPUT_DISCARD_BYTES: usize = 64;

/// How often the client checks whether a machine's server watch has ended or
/// been cancelled. A local check of the watch's ssh child, not an SSH round
/// trip.
pub(crate) const SERVER_WATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Delay between checks that an SSH child process has exited. It bounds
/// completion latency without spinning on `try_wait`.
pub(crate) const SSH_CHILD_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bytes retained from SSH stdout. Discovery and status output are small; the
/// cap allows banners and structured results while bounding remote output.
pub(crate) const SSH_STDOUT_CAPTURE_LIMIT: usize = 1024 * 1024;

/// Bytes retained from SSH stderr. The cap is enough for useful SSH
/// diagnostics while bounding untrusted remote error output.
pub(crate) const SSH_STDERR_CAPTURE_LIMIT: usize = 16 * 1024;

/// Time a pipe reader may continue after the SSH child exits. OpenSSH's
/// ControlPersist master can inherit the pipe, so the grace captures prompt
/// output and then lets the caller continue instead of waiting indefinitely.
pub(crate) const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Read chunk size for draining an SSH child pipe. The chunk limits each
/// temporary stack buffer while keeping pipe reads efficient.
pub(crate) const SSH_PIPE_READ_BUFFER_BYTES: usize = 8 * 1024;

/// Capacity for the completion result sent by an SSH pipe reader. This lets
/// the reader report completion without waiting for its consumer.
pub(crate) const SSH_PIPE_DONE_CHANNEL_CAPACITY: usize = 1;

/// Grace after bridge stream IO stops before the SSH child is terminated. It
/// lets EOF and buffered output settle while bounding teardown.
pub(crate) const BRIDGE_CONNECTION_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// Poll interval while checking the bridge's SSH child.
/// The interval keeps bridge startup and teardown responsive without a
/// busy loop.
pub(crate) const BRIDGE_CHILD_POLL: Duration = Duration::from_millis(50);

/// Delay after a bridge socket write would block. A short pause lets the
/// local reader catch up without spinning continuously.
pub(crate) const BRIDGE_IO_POLL: Duration = Duration::from_millis(1);

/// Buffer used by each bridge transfer direction. The chunk amortizes IO
/// calls while keeping temporary memory for each worker bounded.
pub(crate) const BRIDGE_IO_BUFFER_BYTES: usize = 16 * 1024;

/// Maximum bytes passed to one local stream write. The cap limits each write
/// operation and leaves the bridge responsive to cancellation between chunks.
pub(crate) const BRIDGE_WRITE_CHUNK_BYTES: usize = 4 * 1024;

/// Initial capacity reserved for remote CLI arguments. This covers the common
/// command shape; `Vec` still grows if a command needs more.
pub(crate) const REMOTE_COMMAND_ARGS_INITIAL_CAPACITY: usize = 6;

/// One cold SSH round trip, including a remote command or status probe: the
/// budget of every bounded SSH command, retries and discovery alike. This
/// bounds a slow startup without letting a hung host block the caller.
pub(crate) const SSH_COMMAND_TIMEOUT: Duration = SSH_CONNECT_TIMEOUT
    .saturating_add(shepr_launch::limits::STATUS_OVERVIEW_TIMEOUT)
    .saturating_add(SSH_STATUS_COMMAND_GRACE);

/// OpenSSH's connection window, `SSH_CONNECT_TIMEOUT_OPTION` (a test holds the
/// two together); the remote status needs its own window after a cold
/// connection has used this one.
const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Scheduling and command startup beyond connection and socket request time.
const SSH_STATUS_COMMAND_GRACE: Duration = Duration::from_secs(1);

/// What [`SSH_CONNECTION_ATTEMPT_BUDGET`] allows beyond one cold SSH round trip,
/// for the remaining discovery commands, the bridge and the handshake.
pub(crate) const SSH_ATTEMPT_SLACK: Duration = Duration::from_secs(10);

/// The longest one connection attempt to a configured machine may run: one cold
/// SSH round trip plus [`SSH_ATTEMPT_SLACK`]. The client's per-attempt deadline
/// and the startup check of every machine both take it from here, so the two
/// cannot drift apart.
pub const SSH_CONNECTION_ATTEMPT_BUDGET: Duration =
    SSH_COMMAND_TIMEOUT.saturating_add(SSH_ATTEMPT_SLACK);

/// An operator's Connect also allows the remote launch to finish before the
/// bridge can relay the handshake. Automatic attaches never start a daemon.
pub const SSH_START_ATTEMPT_BUDGET: Duration =
    SSH_CONNECTION_ATTEMPT_BUDGET.saturating_add(shepr_launch::limits::START_WORST_CASE);

/// The longest an operator's Restart of a configured machine may run: the
/// conditional stop of the server of another build, with its own timeout, and
/// then an ordinary connection attempt that starts this build's server.
pub const SSH_RESTART_ATTEMPT_BUDGET: Duration =
    SSH_START_ATTEMPT_BUDGET.saturating_add(REMOTE_STOP_SSH_TIMEOUT);

/// How long the startup check of every configured machine may take in all. The
/// checks run concurrently, so this is a bound on the whole phase, not per
/// machine. It is the client's per-attempt connection budget.
pub(crate) const PREFLIGHT_CHECK_BUDGET: Duration = SSH_CONNECTION_ATTEMPT_BUDGET;

/// How long `shepr status --all` or `shepr stop --all` may spend finding a
/// configured machine's `shepr` and reading its status: the same budget as one
/// connection attempt. Machines are handled concurrently, so this bounds the
/// whole status. A stop then gives its stop command [`REMOTE_STOP_SSH_TIMEOUT`].
pub(crate) const FLEET_STATUS_BUDGET: Duration = SSH_CONNECTION_ATTEMPT_BUDGET;

/// An SSH bridge must outlive several client heartbeat cycles while idle.
/// This gives a healthy bridge multiple chances to answer endpoint probes;
/// its minimum cycle ratio is checked below.
pub(crate) const BRIDGE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum number of client heartbeat intervals that a quiet bridge survives.
/// Several cycles allow delayed probes before the bridge is considered
/// idle.
const BRIDGE_IDLE_MIN_HEARTBEAT_CYCLES: u32 = 3;

// The client's heartbeat is what keeps an idle bridge's watchdog renewed, so
// bridge expiry must comfortably exceed the cadence the client probes at.
const _: () = assert!(
    BRIDGE_IDLE_TIMEOUT.as_millis()
        >= shepr_launch::connection_health::HEARTBEAT_INTERVAL
            .saturating_mul(BRIDGE_IDLE_MIN_HEARTBEAT_CYCLES)
            .as_millis()
);

/// Maximum time between checks by the idle SSH bridge watchdog.
/// The interval bounds idle-expiry detection without busy polling.
pub(crate) const BRIDGE_WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Read chunk size for the remote host's bridge stdio relay. It amortizes
/// read overhead while keeping each stack buffer small.
pub(crate) const REMOTE_BRIDGE_COPY_BUFFER_BYTES: usize = 16 * 1024;

/// OpenSSH option limiting connection establishment. This
/// leaves room for ordinary network setup while bounding unreachable hosts.
pub(crate) const SSH_CONNECT_TIMEOUT_OPTION: &str = "ConnectTimeout=10";

/// OpenSSH option disabling internal retries. The surrounding shepr retry owns
/// pacing, so SSH does not hide a failed attempt.
pub(crate) const SSH_CONNECTION_ATTEMPTS_OPTION: &str = "ConnectionAttempts=1";

/// OpenSSH option retaining an idle control master. This
/// amortizes repeated SSH setup while bounding how long it remains available.
pub(crate) const SSH_CONTROL_PERSIST_OPTION: &str = "ControlPersist=600";

/// OpenSSH option disabling password prompts in background SSH commands.
/// They cannot be answered and would otherwise stall the bounded attempt.
pub(crate) const SSH_NO_PASSWORD_PROMPTS_OPTION: &str = "NumberOfPasswordPrompts=0";

/// OpenSSH option permitting authentication prompts for the foreground
/// command, enough for ordinary interactive authentication flows.
pub(crate) const SSH_AUTHENTICATION_PASSWORD_PROMPTS_OPTION: &str = "NumberOfPasswordPrompts=3";

/// OpenSSH keepalive settings written to the managed SSH config.
#[derive(Clone, Copy)]
pub(crate) struct SshKeepalive {
    /// Seconds between probes of an idle SSH connection.
    interval_secs: u32,
    /// Unanswered probes allowed before the SSH connection is treated as lost.
    count_max: u32,
}

impl SshKeepalive {
    /// The managed config's `Host *` keepalive lines.
    pub(crate) fn config_lines(&self) -> String {
        format!(
            "  ServerAliveInterval {}\n  ServerAliveCountMax {}\n",
            self.interval_secs, self.count_max
        )
    }
}

/// OpenSSH keepalive policy probes idle connections and declares a link lost
/// after repeated unanswered probes. This detects dropped connections while
/// tolerating transient packet loss without retaining stale connections long.
pub(crate) const SSH_KEEPALIVE: SshKeepalive = SshKeepalive {
    interval_secs: 15,
    count_max: 4,
};

// A cold connection and the full remote status overview must complete before
// SSH's command timeout can be mistaken for an authentication wait, with room
// to spare. The stop and start budgets are derived from launch's worst cases
// above, so they need no check of their own.
const _: () = assert!(
    SSH_COMMAND_TIMEOUT.as_millis()
        > SSH_CONNECT_TIMEOUT
            .saturating_add(shepr_launch::limits::STATUS_OVERVIEW_TIMEOUT)
            .as_millis()
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The status budget counts the connection window that OpenSSH is told.
    #[test]
    fn the_connect_timeout_option_spells_the_budgeted_window() {
        assert_eq!(
            SSH_CONNECT_TIMEOUT_OPTION,
            format!("ConnectTimeout={}", SSH_CONNECT_TIMEOUT.as_secs())
        );
    }
}
