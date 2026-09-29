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

/// Maximum UTF-8 bytes in an SSH agent registration response line. The reply
/// is a small JSON status, so the cap leaves room for its envelope and bounds
/// memory used while waiting for the newline.
pub(crate) const SSH_AGENT_RESPONSE_MAX_BYTES: usize = 4096;

/// How often an attached SSH bridge checks its agent registration stream.
/// The interval bounds detection delay while keeping an idle worker quiet.
pub(crate) const SSH_AGENT_STREAM_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Initial delay before retrying SSH agent registration. The short pause
/// responds quickly when the local API starts after the bridge.
pub(crate) const SSH_AGENT_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Ceiling for exponential SSH agent registration retry pacing. The cap
/// keeps recovery responsive without waking continuously during an outage.
pub(crate) const SSH_AGENT_MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Multiplier for exponential SSH agent retry pacing. Backoff reaches the
/// ceiling in a few retries.
pub(crate) const SSH_AGENT_RETRY_BACKOFF_FACTOR: u32 = 2;

/// Time allowed for the local API status check and registration response.
/// The timeout bounds each local IPC wait while allowing a responsive server
/// to answer under ordinary load.
pub(crate) const SSH_AGENT_REGISTRATION_TIMEOUT: Duration = Duration::from_millis(500);

/// Delay between nonblocking SSH agent response reads. The interval gives
/// the API worker time to answer without a tight polling loop.
pub(crate) const SSH_AGENT_RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How often a launching client checks its spawned server and the launch lock.
/// The interval notices a daemon that died during boot and makes startup
/// visible promptly without a busy wait.
pub(crate) const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Time allowed for one status request to a local server, the response
/// deadline of every launch probe. The timeout bounds an unavailable or
/// overloaded local server check.
pub(crate) const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// The most a launched server's boot log may hold. A launch that finds more
/// (a server printing without end while it boots) fails and kills the server,
/// and one that boots successfully empties the log. The cap keeps a runaway
/// server from filling the runtime directory, which is usually a small tmpfs.
pub(crate) const BOOT_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Time allowed for the SSH command that stops a remote server. The remote
/// `server stop` itself waits up to its own stop deadline for the server to
/// close its sockets; this covers that plus the connection.
pub(crate) const REMOTE_STOP_SSH_TIMEOUT: Duration = Duration::from_secs(45);

/// How many times one machine is offered a restart. A server that was replaced
/// between the check and the stop is a new occupant and is offered again, once;
/// beyond that something keeps restarting it and the operator is told to run
/// shepr again.
pub(crate) const MAX_RESTART_OFFERS: usize = 2;

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

/// Poll interval while accepting a bridge socket or checking its SSH child.
/// The interval keeps bridge startup and teardown responsive without a
/// busy loop.
pub(crate) const BRIDGE_ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Delay after a bridge socket write would block. A short pause lets the
/// local reader catch up without spinning continuously.
pub(crate) const BRIDGE_IO_POLL: Duration = Duration::from_millis(1);

/// Time a local caller waits for the SSH worker to report a failure after EOF.
/// The timeout is enough for child reaping and diagnostic capture, and bounds
/// the caller's wait if the worker cannot report.
pub(crate) const BRIDGE_FAILURE_REPORT_TIMEOUT: Duration = Duration::from_secs(1);

/// Poll interval while waiting for an SSH worker failure report. It keeps the
/// error prompt without holding the receiver lock.
pub(crate) const BRIDGE_FAILURE_REPORT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Capacity for a pending bridge failure report. The slot preserves the
/// most recent request's error without allowing reports to queue unboundedly.
pub(crate) const BRIDGE_FAILURE_CHANNEL_CAPACITY: usize = 1;

/// Buffer used by each bridge transfer direction. The chunk amortizes IO
/// calls while keeping temporary memory for each worker bounded.
pub(crate) const BRIDGE_IO_BUFFER_BYTES: usize = 16 * 1024;

/// Maximum bytes passed to one local stream write. The cap limits each write
/// operation and leaves the bridge responsive to cancellation between chunks.
pub(crate) const BRIDGE_WRITE_CHUNK_BYTES: usize = 4 * 1024;

/// Initial capacity reserved for remote CLI arguments. This covers the common
/// command shape; `Vec` still grows if a command needs more.
pub(crate) const REMOTE_COMMAND_ARGS_INITIAL_CAPACITY: usize = 6;

/// SSH command budget, shared with the core SSH request budget so retries and
/// discovery use the same time limit.
pub(crate) const SSH_COMMAND_TIMEOUT: Duration = shepr_core::limits::SSH_ROUND_TRIP_TIMEOUT;

/// How long the startup check of every configured machine may take in all: one
/// cold SSH round trip plus slack for the remaining discovery commands. The
/// checks run concurrently, so this is a bound on the whole phase, not per
/// machine. It mirrors the client's per-attempt connection budget, which sits
/// in a higher crate.
pub(crate) const PREFLIGHT_CHECK_BUDGET: Duration =
    SSH_COMMAND_TIMEOUT.saturating_add(Duration::from_secs(10));

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

/// OpenSSH keepalive settings shared by command arguments and managed config.
#[derive(Clone, Copy)]
pub(crate) struct SshKeepalive {
    /// Seconds between probes of an idle SSH connection.
    pub(crate) interval_secs: u32,
    /// Unanswered probes allowed before the SSH connection is treated as lost.
    pub(crate) count_max: u32,
}

/// OpenSSH keepalive policy probes idle connections and declares a link lost
/// after repeated unanswered probes. This detects dropped connections while
/// tolerating transient packet loss without retaining stale connections long.
pub(crate) const SSH_KEEPALIVE: SshKeepalive = SshKeepalive {
    interval_secs: 15,
    count_max: 4,
};
