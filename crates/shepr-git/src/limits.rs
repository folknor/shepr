//! Runtime budgets for Git probes and the status cache.

use std::time::Duration;

/// How long one Git probe may run before it is killed. This bounds hung Git
/// reads so they cannot stall workspace and sidebar updates.
pub(crate) const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a worker thread that owes a refresh may go without progress (a
/// step started, a refresh computed or published) before it is abandoned. A
/// step is one target's checkout discovery or one checkout's status: several
/// Git probes, each bounded by [`GIT_COMMAND_TIMEOUT`], around filesystem calls
/// that have no deadline. The bound sits well above a step whose probes all
/// time out, so in practice only a step blocked outside its probes, as on a
/// hung mount, reaches it.
pub(crate) const GIT_REFRESH_STALL_BOUND: Duration = GIT_COMMAND_TIMEOUT.saturating_mul(12);

/// Most abandoned worker threads left alive at once. Each is blocked for good
/// on a path no later refresh visits while it lives, so this bounds the threads
/// a hung mount can hold; past it a stalled refresh is waited out instead.
pub(crate) const MAX_ABANDONED_GIT_REFRESH_THREADS: usize = 4;

/// Polling interval while waiting for a Git probe and its output readers. The
/// interval keeps exit detection responsive without a busy loop.
pub(crate) const GIT_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Maximum bytes read from one loose Git ref file; far above any real ref,
/// it bounds the read of a corrupt or hostile file.
pub(crate) const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;

/// Retry delay after Git status refresh fails, avoiding repeated filesystem
/// and subprocess work for a broken or unavailable checkout.
pub(crate) const GIT_STATUS_RETRY_DELAY: Duration = Duration::from_secs(30);
