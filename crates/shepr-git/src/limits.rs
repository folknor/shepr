//! Runtime budgets for Git probes and the status cache.

use std::time::Duration;

/// How long one Git probe may run before it is killed. This bounds hung Git
/// reads so they cannot stall workspace and sidebar updates.
pub(crate) const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a worker thread that owes a refresh may go without progress
/// before abandonment. Discovery/status boundaries and each direct filesystem
/// access announce progress; an access replaces the nominal job paths with
/// its physical mount paths. Git probes have their own deadlines. The bound
/// sits well above a job whose probes all time out, so ordinary probe failures
/// do not consume abandonment slots.
pub(crate) const GIT_REFRESH_STALL_BOUND: Duration = GIT_COMMAND_TIMEOUT.saturating_mul(12);

/// Most abandoned worker threads left alive at once. Filesystem accesses
/// quarantine all mount points of their device, so a mount that stays hung
/// holds one thread. Independent filesystem or destructor stalls can fill
/// the global budget; past it a stalled refresh is waited out.
pub(crate) const MAX_ABANDONED_GIT_REFRESH_THREADS: usize = 4;

/// Polling interval while waiting for a Git probe and its output readers. The
/// interval keeps exit detection responsive without a busy loop.
pub(crate) const GIT_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long a probe that timed out waits for its killed child to be reaped
/// before the child is handed to the background reaper. A child in an
/// uninterruptible wait on a hung mount ignores SIGKILL until the kernel call
/// returns, so the caller must not wait for it.
pub(crate) const GIT_KILL_REAP_GRACE: Duration = Duration::from_millis(250);

/// Most killed Git children the background reaper holds at once. A child past
/// it is dropped unreaped, a zombie until shepr exits, rather than growing
/// without bound under a mount that stays hung.
pub(crate) const MAX_UNREAPED_GIT_CHILDREN: usize = 16;

/// How often the background reaper checks the children it holds.
pub(crate) const UNREAPED_GIT_CHILD_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bytes read from a Git probe's output pipe per read call. Large enough that
/// typical status output drains in a few reads.
pub(crate) const GIT_PIPE_READ_CHUNK_BYTES: usize = 8192;

/// Maximum retained bytes from each Git output stream. Config listings are
/// user-controlled; exceeding this budget fails the probe instead of returning
/// a truncated listing that could hide dependencies from the cache.
pub(crate) const MAX_GIT_PIPE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum bytes read from one loose Git ref file; far above any real ref,
/// it bounds the read of a corrupt or hostile file.
pub(crate) const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;

/// Retry delay after Git status refresh fails, avoiding repeated filesystem
/// and subprocess work for a broken or unavailable checkout. It is also how
/// long a branch config whose dependencies cannot be tracked (`ConfigCtx` with
/// `Dependencies::Uncacheable`) is reused before Git is asked again.
pub(crate) const GIT_STATUS_RETRY_DELAY: Duration = Duration::from_secs(30);
