//! Runtime budgets for Git probes and the status cache.

use std::time::Duration;

/// How long one Git probe may run before it is killed. This bounds hung Git
/// reads so they cannot stall workspace and sidebar updates.
pub(crate) const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval while waiting for a Git probe and its output readers. The
/// interval keeps exit detection responsive without a busy loop.
pub(crate) const GIT_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Maximum bytes read from one loose Git ref file; far above any real ref,
/// it bounds the read of a corrupt or hostile file.
pub(crate) const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;

/// Retry delay after Git status refresh fails, avoiding repeated filesystem
/// and subprocess work for a broken or unavailable checkout.
pub(crate) const GIT_STATUS_RETRY_DELAY: Duration = Duration::from_secs(30);
