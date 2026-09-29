//! Timeouts, capacity bounds, and SSH policy values for remote connections.

use std::time::Duration;

/// Maximum UTF-8 bytes in an absolute remote executable path. A 4 KiB bound
/// admits normal paths while rejecting unexpectedly large host output before
/// it is reused in a shell command.
pub(crate) const MAX_REMOTE_EXECUTABLE_BYTES: usize = 4096;

/// Maximum UTF-8 bytes in a saved SSH target. One KiB covers user, host,
/// port, and SSH URI forms while bounding catalog data and command arguments.
pub(crate) const MAX_SSH_TARGET_BYTES: usize = 1024;

/// Maximum bytes read from one cached remote metadata file. Discovery hints
/// contain only three paths or names, so 16 KiB leaves ample room without
/// letting a corrupt cache consume unbounded memory.
pub(crate) const MAX_METADATA_BYTES: u64 = 16 * 1024;

/// Maximum bytes in the saved endpoint catalog. Sixty-four KiB covers a useful
/// catalog of profiles with typical labels, targets, sessions, and JSON
/// overhead while bounding file reads.
pub(crate) const MAX_CATALOG_BYTES: u64 = 64 * 1024;

/// Maximum saved SSH profiles. This keeps catalog size and each client refresh
/// bounded while allowing a useful set of remote machines.
pub(crate) const MAX_PROFILES: usize = 64;

/// Maximum UTF-8 bytes in a saved endpoint label. This allows a readable name
/// while keeping catalog rows and user-facing labels compact.
pub(crate) const MAX_LABEL_BYTES: usize = 128;

/// How often an open client checks the saved machine catalog for changes. An
/// interval of one second keeps selection updates responsive without polling the
/// filesystem on every client event.
pub(crate) const CATALOG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum UTF-8 bytes in an SSH agent registration response line. The reply
/// is a small JSON status, so 4 KiB leaves room for its envelope and bounds
/// memory used while waiting for the newline.
pub(crate) const SSH_AGENT_RESPONSE_MAX_BYTES: usize = 4096;

/// How often an attached SSH bridge checks its agent registration stream.
/// One second bounds detection delay while keeping an idle worker quiet.
pub(crate) const SSH_AGENT_STREAM_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Initial delay before retrying SSH agent registration. One tenth of a second
/// responds quickly when the local API starts after the bridge.
pub(crate) const SSH_AGENT_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Ceiling for exponential SSH agent registration retry pacing. Five seconds
/// keeps recovery responsive without waking continuously during an outage.
pub(crate) const SSH_AGENT_MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Multiplier for exponential SSH agent retry pacing. Doubling the 100 ms
/// initial delay reaches the five-second ceiling in a few retries.
pub(crate) const SSH_AGENT_RETRY_BACKOFF_FACTOR: u32 = 2;

/// Time allowed for the local API status check and registration response.
/// Half a second bounds each local IPC wait while allowing a responsive server
/// to answer under ordinary load.
pub(crate) const SSH_AGENT_REGISTRATION_TIMEOUT: Duration = Duration::from_millis(500);

/// Delay between nonblocking SSH agent response reads. Ten milliseconds gives
/// the API worker time to answer without a tight polling loop.
pub(crate) const SSH_AGENT_RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How often a client checks for the newly spawned server socket. Fifty
/// milliseconds makes startup visible promptly without a busy wait.
pub(crate) const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Time allowed for the stable status API check before attaching. Two seconds
/// bounds an unavailable or overloaded local server check.
pub(crate) const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum time for a newly spawned server to expose its client socket. Fifteen
/// seconds allows normal startup while keeping a failed launch finite.
pub const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Time allowed for a stopped remote server to disappear. Five seconds covers
/// ordinary process shutdown without making an SSH attach wait indefinitely.
pub(crate) const REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval while confirming remote server shutdown. One tenth of a
/// second keeps confirmation responsive without repeatedly invoking SSH.
pub(crate) const REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Delay between checks that an SSH child process has exited. Fifty
/// milliseconds bounds completion latency without spinning on `try_wait`.
pub(crate) const SSH_CHILD_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bytes retained from SSH stdout. Discovery and status output are small; one
/// MiB allows banners and structured results while bounding remote output.
pub(crate) const SSH_STDOUT_CAPTURE_LIMIT: usize = 1024 * 1024;

/// Bytes retained from SSH stderr. Sixteen KiB is enough for useful SSH
/// diagnostics while bounding untrusted remote error output.
pub(crate) const SSH_STDERR_CAPTURE_LIMIT: usize = 16 * 1024;

/// Time a pipe reader may continue after the SSH child exits. OpenSSH's
/// ControlPersist master can inherit the pipe, so the 500 ms grace captures
/// prompt output and then lets the caller continue instead of waiting minutes.
pub(crate) const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Read chunk size for draining an SSH child pipe. Eight KiB limits each
/// temporary stack buffer while keeping pipe reads efficient.
pub(crate) const SSH_PIPE_READ_BUFFER_BYTES: usize = 8 * 1024;

/// Capacity for the single completion result sent by an SSH pipe reader. One
/// slot lets the reader report completion without waiting for its consumer.
pub(crate) const SSH_PIPE_DONE_CHANNEL_CAPACITY: usize = 1;

/// Grace after bridge stream IO stops before the SSH child is terminated. A
/// quarter second lets EOF and buffered output settle while bounding teardown.
pub(crate) const BRIDGE_CONNECTION_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// Poll interval while accepting a bridge socket or checking its SSH child.
/// Fifty milliseconds keeps bridge startup and teardown responsive without a
/// busy loop.
pub(crate) const BRIDGE_ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Delay after a bridge socket write would block. One millisecond lets the
/// local reader catch up without spinning continuously.
pub(crate) const BRIDGE_IO_POLL: Duration = Duration::from_millis(1);

/// Time a local caller waits for the SSH worker to report a failure after EOF.
/// One second is enough for child reaping and diagnostic capture, and bounds
/// the caller's wait if the worker cannot report.
pub(crate) const BRIDGE_FAILURE_REPORT_TIMEOUT: Duration = Duration::from_secs(1);

/// Poll interval while waiting for an SSH worker failure report. Ten
/// milliseconds keeps the error prompt without holding the receiver lock.
pub(crate) const BRIDGE_FAILURE_REPORT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Capacity for one pending bridge failure report. A single slot preserves the
/// most recent request's error without allowing reports to queue unboundedly.
pub(crate) const BRIDGE_FAILURE_CHANNEL_CAPACITY: usize = 1;

/// Buffer used by each bridge transfer direction. Sixteen KiB amortizes IO
/// calls while keeping temporary memory for each worker bounded.
pub(crate) const BRIDGE_IO_BUFFER_BYTES: usize = 16 * 1024;

/// Maximum bytes passed to one local stream write. Four KiB limits each write
/// operation and leaves the bridge responsive to cancellation between chunks.
pub(crate) const BRIDGE_WRITE_CHUNK_BYTES: usize = 4 * 1024;

/// Number of target characters kept in the readable part of a shortened
/// bridge socket name. Eight characters aid diagnosis while leaving room for
/// the hash and session data under the platform socket path limit.
pub(crate) const BRIDGE_TARGET_PREFIX_CHARS: usize = 8;

/// Maximum characters in a sanitized component in socket names. Thirty-two
/// preserves a useful readable prefix while keeping the complete socket name
/// within Unix socket path limits.
pub(crate) const BRIDGE_PATH_COMPONENT_MAX_CHARS: usize = 32;

/// Initial capacity reserved for remote CLI arguments. Six covers the common
/// command shape; `Vec` still grows for attach commands with more fields.
pub(crate) const REMOTE_COMMAND_ARGS_INITIAL_CAPACITY: usize = 6;

/// Buffer size for filtering remote SSH stderr before displaying it locally.
/// Four KiB keeps each read bounded while passing prompts through promptly.
pub(crate) const REMOTE_STDERR_FILTER_BUFFER_BYTES: usize = 4 * 1024;

/// Noninteractive SSH command budget, shared with the core SSH request budget
/// so retries and discovery use the same time limit.
pub(crate) const NONINTERACTIVE_SSH_COMMAND_TIMEOUT: Duration =
    shepr_core::limits::SSH_ROUND_TRIP_TIMEOUT;

/// OpenSSH option limiting connection establishment to ten seconds. This
/// leaves room for ordinary network setup while bounding unreachable hosts.
pub(crate) const SSH_CONNECT_TIMEOUT_OPTION: &str = "ConnectTimeout=10";

/// OpenSSH option allowing one connection attempt. The surrounding shepr
/// retry owns pacing, so SSH's internal retries do not hide a failed attempt.
pub(crate) const SSH_CONNECTION_ATTEMPTS_OPTION: &str = "ConnectionAttempts=1";

/// OpenSSH option retaining an idle control master for ten minutes. This
/// amortizes repeated SSH setup while bounding how long it remains available.
pub(crate) const SSH_CONTROL_PERSIST_OPTION: &str = "ControlPersist=600";

/// OpenSSH option disabling password prompts in background SSH commands.
/// They cannot be answered and would otherwise stall the bounded attempt.
pub(crate) const SSH_NONINTERACTIVE_PASSWORD_PROMPTS_OPTION: &str = "NumberOfPasswordPrompts=0";

/// OpenSSH option permitting up to three authentication prompts for the
/// foreground authentication command, enough for ordinary auth flows with
/// several prompts.
pub(crate) const SSH_AUTHENTICATION_PASSWORD_PROMPTS_OPTION: &str = "NumberOfPasswordPrompts=3";

/// OpenSSH keepalive settings shared by command arguments and managed config.
#[derive(Clone, Copy)]
pub(crate) struct SshKeepalive {
    /// Seconds between probes of an idle SSH connection.
    pub(crate) interval_secs: u32,
    /// Unanswered probes allowed before the SSH connection is treated as lost.
    pub(crate) count_max: u32,
}

/// OpenSSH keepalive policy: probe every 15 seconds and declare the link lost
/// after four unanswered probes. This detects dropped idle connections while
/// tolerating transient packet loss without retaining stale connections long.
pub(crate) const SSH_KEEPALIVE: SshKeepalive = SshKeepalive {
    interval_secs: 15,
    count_max: 4,
};
