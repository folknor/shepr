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

/// Git probes that may be running or waiting to be reaped at once. A probe
/// stuck on a hung mount keeps its slot until the kernel releases it; past
/// this, new probes are refused rather than piling up processes. Refresh
/// threads run one probe at a time, so this is mostly stuck probes.
pub(crate) const GIT_CHILD_BUDGET: usize = 16;

/// Maximum retained bytes from each Git output stream. Config listings are
/// user-controlled; exceeding this budget fails the probe instead of returning
/// a truncated listing that could hide dependencies from the cache.
pub(crate) const MAX_GIT_PIPE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum bytes read from one loose Git ref file; far above any real ref,
/// it bounds the read of a corrupt or hostile file.
pub(crate) const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;

/// Bytes of a HEAD file read when deciding whether a directory is a Git
/// directory. Git's own check (`validate_headref` in setup.c) reads at most
/// this many into a 256-byte buffer; a valid symref or object ID fits.
pub(crate) const MAX_GIT_HEAD_VALIDATION_BYTES: u64 = 255;

/// Retry delay after Git status refresh fails, avoiding repeated filesystem
/// and subprocess work for a broken or unavailable checkout. It is also how
/// long a branch config whose dependencies cannot be tracked (`ConfigCtx` with
/// `Dependencies::Uncacheable`) is reused before Git is asked again.
pub(crate) const GIT_STATUS_RETRY_DELAY: Duration = Duration::from_secs(30);
