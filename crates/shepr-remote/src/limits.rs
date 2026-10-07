//! Timeouts, capacity bounds, and SSH policy values for remote connections.

use std::time::Duration;

/// Full discovery produces at most one PATH candidate and the two known
/// install locations (`discovery::ordered_candidates` holds it to this). The
/// Connect and Restart deadlines include a status probe for each.
pub(crate) const MAX_REMOTE_EXECUTABLE_CANDIDATES: u32 = 3;

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
/// does not exist (yet, or since it was removed) and so cannot be watched. A
/// local check on that host, not an SSH round trip.
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

/// Bytes retained from the tail of the remote server-wait command's stdout.
/// The wait itself writes nothing there; the only evidence read from it is the
/// wrapper's output marker, which follows any shell startup noise, so a tail
/// as small as the stderr cap keeps it without the discovery-output allowance.
pub(crate) const SSH_WAIT_STDOUT_CAPTURE_LIMIT: usize = 16 * 1024;

/// Time a pipe reader may continue after the SSH child exits. OpenSSH points
/// a detached ControlPersist master's standard streams at `/dev/null`, but a
/// background descendant of a configured `LocalCommand` can inherit the pipe
/// and hold it open. The grace captures prompt output and then lets the caller
/// continue instead of waiting for that descendant.
pub(crate) const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Read chunk size for draining an SSH child pipe. The chunk limits each
/// temporary stack buffer while keeping pipe reads efficient.
pub(crate) const SSH_PIPE_READ_BUFFER_BYTES: usize = 8 * 1024;

/// Grace after bridge stream IO stops before the SSH child is terminated. It
/// lets EOF and buffered output settle while bounding teardown.
pub(crate) const BRIDGE_CONNECTION_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// Poll interval while checking the bridge's SSH child.
/// The interval keeps bridge startup and teardown responsive without a
/// busy loop.
pub(crate) const BRIDGE_CHILD_POLL: Duration = Duration::from_millis(50);

/// Buffer used by each bridge transfer direction. The chunk amortizes IO
/// calls while keeping temporary memory for each worker bounded.
pub(crate) const BRIDGE_IO_BUFFER_BYTES: usize = 16 * 1024;

/// Maximum bytes passed to one local stream write. The cap limits each write
/// operation and leaves the bridge responsive to cancellation between chunks.
pub(crate) const BRIDGE_WRITE_CHUNK_BYTES: usize = 4 * 1024;

/// One cold SSH round trip carrying the slowest remote status command: the
/// connection window, then `status --json` (the sibling `--version` probe and
/// the server overview, `STATUS_COMMAND_WORST_CASE`), then startup grace. It is
/// the budget of every bounded SSH command, retries and discovery alike, so
/// no remote status command can use it all before it would have answered.
pub(crate) const SSH_COMMAND_TIMEOUT: Duration = SSH_CONNECT_TIMEOUT
    .saturating_add(shepr_launch::limits::STATUS_COMMAND_WORST_CASE)
    .saturating_add(SSH_STATUS_COMMAND_GRACE);

/// OpenSSH's connection window, formatted directly into the command option.
/// The remote status needs its own window after a cold
/// connection has used this one.
pub(crate) const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Scheduling and command startup beyond connection and socket request time.
const SSH_STATUS_COMMAND_GRACE: Duration = Duration::from_secs(1);

/// What [`SSH_CONNECTION_ATTEMPT_BUDGET`] allows beyond one cold SSH round trip,
/// for the remaining discovery commands, the bridge and the handshake. The
/// client's retry bound caps the sum (its `ATTEMPT_BUDGET` must stay below its
/// `MAX_RETRY_DELAY`), so this shrinks when the round trip grows.
pub(crate) const SSH_ATTEMPT_SLACK: Duration = Duration::from_secs(8);

/// The longest one connection attempt to a configured machine may run: one cold
/// SSH round trip plus [`SSH_ATTEMPT_SLACK`]. The client's per-attempt deadline
/// and the startup check of every machine both take it from here, so the two
/// cannot drift apart.
pub const SSH_CONNECTION_ATTEMPT_BUDGET: Duration =
    SSH_COMMAND_TIMEOUT.saturating_add(SSH_ATTEMPT_SLACK);

// Both operator modes re-verify a cached executable. If it no longer matches,
// resolution can run the two discovery commands and probe each candidate.
const SSH_OPERATOR_MAX_RESOLUTION_COMMANDS: u32 = 1 + 2 + MAX_REMOTE_EXECUTABLE_CANDIDATES;

/// The bridge phase of an operator's Connect or Restart: one connection
/// attempt with the remote launch inside it, since a starting bridge launches
/// the host's server before it relays the handshake. It also bounds the
/// client's wait for a machine's Welcome, which starts after resolution.
pub const SSH_START_BRIDGE_BUDGET: Duration =
    SSH_CONNECTION_ATTEMPT_BUDGET.saturating_add(shepr_launch::limits::START_WORST_CASE);

/// An operator's Connect allows executable verification (including full
/// discovery when a cached path is rejected), then the remote launch to finish
/// before the bridge relays the handshake. Automatic attaches never start a daemon.
pub const SSH_START_ATTEMPT_BUDGET: Duration = SSH_COMMAND_TIMEOUT
    .saturating_mul(SSH_OPERATOR_MAX_RESOLUTION_COMMANDS)
    .saturating_add(SSH_START_BRIDGE_BUDGET);

/// The longest an operator's Restart of a configured machine may run: cached
/// executable verification with a full-discovery fallback, the server-status
/// read and conditional stop, then a bridge that starts this build's server.
pub const SSH_RESTART_ATTEMPT_BUDGET: Duration = SSH_START_ATTEMPT_BUDGET
    .saturating_add(SSH_COMMAND_TIMEOUT)
    .saturating_add(REMOTE_STOP_SSH_TIMEOUT);

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
///
/// It is as long as [`SSH_KEEPALIVE`]'s loss window (interval times count),
/// but the two govern separate checks: this one is the remote bridge's
/// watchdog on client traffic, the keepalive is OpenSSH's probe of the
/// transport. Neither is ordered against the other, so either may fire first
/// on a dead link.
pub(crate) const BRIDGE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum number of client heartbeat intervals that a quiet bridge survives.
/// Several cycles allow delayed probes before the bridge is considered
/// idle.
const BRIDGE_IDLE_MIN_HEARTBEAT_CYCLES: u32 = 3;

// The client's heartbeat is what keeps an idle bridge's watchdog renewed, so
// bridge expiry must comfortably exceed the cadence the client probes at.
const _: () = assert!(
    BRIDGE_IDLE_TIMEOUT.as_millis()
        >= shepr_launch::limits::HEARTBEAT_INTERVAL
            .saturating_mul(BRIDGE_IDLE_MIN_HEARTBEAT_CYCLES)
            .as_millis()
);

/// Maximum time between checks by the idle SSH bridge watchdog.
/// The interval bounds idle-expiry detection without busy polling.
pub(crate) const BRIDGE_WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Read chunk size for the remote host's bridge stdio relay. It amortizes
/// read overhead while keeping each stack buffer small.
pub(crate) const REMOTE_BRIDGE_COPY_BUFFER_BYTES: usize = 16 * 1024;

/// OpenSSH's `ConnectionAttempts`: one, disabling internal retries. The
/// surrounding shepr retry owns pacing, so SSH does not hide a failed attempt.
pub(crate) const SSH_CONNECTION_ATTEMPTS: u32 = 1;

/// OpenSSH's `ControlPersist`: how long an idle control master is kept. This
/// amortizes repeated SSH setup while bounding how long it remains available.
/// It bounds an idle master only; a remote wait for a server keeps its channel
/// open and so the master in use, so it has no ordering with `SERVER_WAIT_MAX`.
pub(crate) const SSH_CONTROL_PERSIST: Duration = Duration::from_secs(600);

/// OpenSSH's `NumberOfPasswordPrompts` for background SSH commands: none.
/// They cannot be answered and would otherwise stall the bounded attempt.
pub(crate) const SSH_NO_PASSWORD_PROMPTS: u32 = 0;

/// OpenSSH's `NumberOfPasswordPrompts` for the foreground login command,
/// enough for ordinary interactive authentication flows.
pub(crate) const SSH_AUTHENTICATION_PASSWORD_PROMPTS: u32 = 3;

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

// A cold connection and the slowest remote status command must complete before
// SSH's command timeout can be mistaken for an authentication wait, with room
// to spare. The stop and start budgets are derived from launch's worst cases
// above, so they need no check of their own.
const _: () = assert!(
    SSH_COMMAND_TIMEOUT.as_millis()
        > SSH_CONNECT_TIMEOUT
            .saturating_add(shepr_launch::limits::STATUS_COMMAND_WORST_CASE)
            .as_millis()
);

// OpenSSH's connect window must end inside the command budget: a dead host
// then fails as a connection error (Offline) rather than as a command that used
// its whole budget, which reads as a possible authentication wait.
const _: () = assert!(SSH_CONNECT_TIMEOUT.as_millis() < SSH_COMMAND_TIMEOUT.as_millis());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_budgets_cover_verification_discovery_status_stop_and_start() {
        assert_eq!(MAX_REMOTE_EXECUTABLE_CANDIDATES, 3);
        assert_eq!(SSH_OPERATOR_MAX_RESOLUTION_COMMANDS, 6);
        assert_eq!(
            SSH_START_ATTEMPT_BUDGET,
            SSH_COMMAND_TIMEOUT
                .saturating_mul(SSH_OPERATOR_MAX_RESOLUTION_COMMANDS)
                .saturating_add(SSH_CONNECTION_ATTEMPT_BUDGET)
                .saturating_add(shepr_launch::limits::START_WORST_CASE)
        );
        assert_eq!(
            SSH_START_BRIDGE_BUDGET,
            SSH_CONNECTION_ATTEMPT_BUDGET.saturating_add(shepr_launch::limits::START_WORST_CASE)
        );
        assert_eq!(
            SSH_RESTART_ATTEMPT_BUDGET,
            SSH_START_ATTEMPT_BUDGET
                .saturating_add(SSH_COMMAND_TIMEOUT)
                .saturating_add(REMOTE_STOP_SSH_TIMEOUT)
        );
    }
}
