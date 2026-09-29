//! Timing and capacity bounds for the Linux platform plumbing.

use std::time::Duration;

/// POSIX permission bits of a directory only its owner can enter or list: the
/// directories shepr creates for sockets and SSH state, and the mode it
/// requires of a directory before trusting it with either. A permission
/// format, not a tunable.
pub(super) const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

/// Attempts before random-name staging fails in socket and SSH config paths.
/// The retry cap makes collision handling finite while keeping exhaustion unlikely.
pub(super) const RANDOM_NAME_ATTEMPTS: u32 = 16;

/// Polling interval while waiting for Git and clipboard helper children.
/// The interval keeps exit detection responsive without a busy loop.
pub(super) const HELPER_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Poll interval used to bound cancellation latency on client streams.
/// The interval limits shutdown delay without continuously polling.
pub(super) const CLIENT_STREAM_POLL_INTERVAL_MS: i32 = 100;

/// Startup allowance for clipboard selection owners before detaching them.
/// The delay gives desktop helpers time to claim a selection.
pub(super) const CLIPBOARD_OWNER_STARTUP_WAIT: Duration = Duration::from_millis(100);

/// Interval between SSH agent liveness probes.
/// The interval detects stale agents promptly without repeated probes.
pub(super) const SSH_AGENT_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Initial delay after a shutdown signal stream is lost.
/// The delay retries promptly while avoiding a reconnect spin.
pub(super) const SHUTDOWN_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);

/// Maximum delay while reconnecting after a shutdown signal stream is lost.
/// The cap bounds recovery latency while keeping repeated failures inexpensive.
pub(super) const SHUTDOWN_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Multiplier for ordinary logind reconnect backoff between retries.
/// Doubling grows quickly after failure while the delay remains capped above.
pub(super) const SHUTDOWN_RECONNECT_BACKOFF_MULTIPLIER: u32 = 2;

/// Maximum time between checks by the idle SSH bridge watchdog.
/// The interval bounds idle-expiry detection without busy polling.
pub(super) const BRIDGE_WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Read chunk size for SSH bridge forwarding. It amortizes read overhead while
/// keeping each stack buffer small.
pub(super) const REMOTE_BRIDGE_COPY_BUFFER_BYTES: usize = 16 * 1024;

/// Read chunk size for bounded child output. It keeps reads efficient without
/// tying memory to the cap.
pub(super) const LIMITED_READ_BUFFER_BYTES: usize = 8 * 1024;

/// Read past a bounded child-output buffer to distinguish exact-cap output
/// from oversized output with minimal work.
pub(super) const LIMITED_READ_OVERFLOW_PROBE_BYTES: usize = 1;

/// Smallest positive timeout passed to poll for a deadline that has not elapsed.
/// A positive minimum prevents sub-millisecond deadlines from turning into
/// busy polls.
pub(super) const MIN_POLL_TIMEOUT_MILLISECONDS: u128 = 1;

/// Byte capacity for the fixed buffer used to read the Linux host name. It
/// leaves margin over Linux's host-name limit and its terminating NUL.
pub(super) const HOSTNAME_BUFFER_BYTES: usize = 256;

/// Total deadline shared by every clipboard helper tried for one read or write.
/// It bounds hung desktop helpers without holding a paste request
/// indefinitely.
pub(super) const CLIPBOARD_HELPER_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum bytes read from a host clipboard helper for a paste. The cap
/// accommodates large paste buffers while preventing unbounded host
/// input. Terminal-originated storage has its own limit.
pub(super) const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

/// How long one Git probe may run before it is killed. The timeout bounds hung
/// status probes so they cannot stall sidebar updates.
pub const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum size of one rotating process log file. The cap retains useful
/// diagnostics while bounding disk use per process.
pub(super) const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Number of rotated log generations kept beside the current log.
/// A prior generation preserves recent context without unbounded disk growth.
pub(super) const DEFAULT_RETAINED_LOG_FILES: usize = 1;
