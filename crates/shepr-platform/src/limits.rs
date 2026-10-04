//! Timing and capacity bounds for the Linux platform plumbing.

use std::time::Duration;

/// POSIX permission bits of a directory only its owner can enter or list: the
/// directories shepr creates for sockets and SSH state, and the mode it
/// requires of a directory before trusting it with either. A permission
/// format, not a tunable.
pub(super) const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

/// Permission mode for files shepr creates with owner-only access.
pub(super) const PRIVATE_FILE_MODE: u32 = 0o600;

/// How long a connect to a local server socket waits for a listener whose
/// backlog is full before it gives up. A healthy server accepts at once, so
/// this only bounds a wedged one.
pub(super) const LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause between connect attempts while a listener's backlog is full.
pub(super) const LOCAL_CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Attempts before random-name staging fails in socket and SSH config paths.
/// The retry cap makes collision handling finite while keeping exhaustion unlikely.
pub(super) const RANDOM_NAME_ATTEMPTS: u32 = 16;

/// Bytes of a server boot log quoted in a failed-launch message. The tail is
/// where a boot failure ends up, and the cap keeps a runaway log out of an
/// error line.
pub(super) const BOOT_LOG_TAIL_BYTES: u64 = 4096;

/// Largest owner marker the runtime artifact sweep reads.
/// An owner identity tag is well under this, so anything larger is not a
/// marker and is left alone unread.
pub(super) const RUNTIME_OWNER_MAX_BYTES: u64 = 128;

/// Polling interval while waiting for clipboard helper children.
/// The interval keeps exit detection responsive without a busy loop.
pub(super) const HELPER_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Poll interval used to bound cancellation latency on client streams.
/// The interval limits shutdown delay without continuously polling.
pub(super) const CLIENT_STREAM_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Startup allowance for clipboard selection owners before detaching them.
/// The delay gives desktop helpers time to claim a selection.
pub(super) const CLIPBOARD_OWNER_STARTUP_WAIT: Duration = Duration::from_millis(100);

/// Read chunk size for bounded child output. It keeps reads efficient without
/// tying memory to the cap.
pub(super) const LIMITED_READ_BUFFER_BYTES: usize = 8 * 1024;

/// Bytes read past a bounded child-output buffer's cap. The smallest possible
/// probe distinguishes exact-cap output from oversized output with minimal work.
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

/// Maximum size of one rotating process log file. The cap retains useful
/// diagnostics while bounding disk use per process.
pub(super) const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Number of rotated log generations kept beside the current log.
/// A prior generation preserves recent context without unbounded disk growth.
pub(super) const DEFAULT_RETAINED_LOG_FILES: usize = 1;

/// Private marker file mode.
pub(super) const RUNTIME_MARKER_MODE: u32 = PRIVATE_FILE_MODE;

/// Permission bits including special bits.
pub(super) const PERMISSION_BITS: u32 = 0o7777;

/// Hex digits in a random runtime entry token.
pub(super) const RUNTIME_TOKEN_HEX_BYTES: usize = 16;

/// Minimum spacing between warnings about rejected socket peers. A connect
/// flood from a foreign uid would otherwise write one log line per attempt.
pub(super) const PEER_REJECTION_WARNING_INTERVAL: Duration = Duration::from_secs(30);

/// Writes between re-stats of the log path. The recheck notices a deleted or
/// replaced log file without a stat on every write.
pub(super) const PATH_RECHECK_AFTER_WRITES: u8 = 16;

/// Upper bound on the number of processes visited while resolving a pane's
/// foreground process-group tree. Foreground-job detection reads `/proc/<pid>/stat`
/// and task/children files for every visited process on a repeated cadence,
/// so an unbounded walk lets accumulated descendants or unreaped zombies
/// under the pane shell grow the server's read-syscall rate and CPU without limit
/// at a constant pane count; the walk runs per pane per tick, so its cost
/// multiplies by the number of panes. The foreground-group leader's subtree and
/// the pane shell's descendants advance round-robin under a shared candidate
/// ceiling, with independent per-root work budgets, so a pathologically large
/// accumulation on either side cannot starve the other. Discovery is best effort
/// once a budget is exhausted.
pub(super) const FOREGROUND_TREE_SCAN_LIMIT: usize = 512;

/// Number of `/proc/<pid>/task` entries a root's subtree may consume, bounding how
/// far one process's thread count can multiply the walk's work.
pub(super) const FOREGROUND_TASK_ENTRY_LIMIT: usize = 2_048;

/// Number of `/proc/<pid>/task/<tid>/children` bytes a root's subtree may read,
/// stopping a parent that accumulates unreaped children from growing read work
/// without limit.
pub(super) const FOREGROUND_CHILD_BYTE_LIMIT: usize = 128 * 1024;

/// Aggregate number of child pids a root's subtree may parse and enqueue, bounding
/// the walk's pending queues and allocations.
pub(super) const FOREGROUND_CHILD_PID_LIMIT: usize = 2_048;

/// Bytes read from one `/proc/.../children` file per syscall. A fixed chunk
/// amortizes reads without allocating in proportion to the entire child list.
pub(super) const PROC_CHILDREN_READ_BUFFER_BYTES: usize = 4096;

/// Bytes of one `/proc/<pid>/cmdline` the foreground probe reads. A longer argv
/// is not returned at all, so one process with a huge command line cannot turn
/// every detection probe into an unbounded procfs read and allocation.
pub(super) const PROCESS_CMDLINE_BYTE_LIMIT: usize = 16 * 1024;
