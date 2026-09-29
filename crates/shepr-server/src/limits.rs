use std::time::Duration;

/// Delay between checks while a session save worker is still running.
pub(crate) const SESSION_SAVE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum retry delay for host-shutdown session checkpoints.
pub(crate) const HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

/// Bounded queue capacity for events forwarded from client threads.
pub(crate) const SERVER_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Maximum manifest reload requests waiting behind the running reload. The API's
/// connection limit bounds them too; this cap holds whatever that limit becomes.
pub(crate) const AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY: usize = 64;

/// How long server exit waits for pane teardowns: their signal budget, plus
/// three more of it for the /proc session scans between signal rounds.
pub(crate) const PANE_TEARDOWN_WAIT: Duration =
    shepr_mux::pane::PaneTeardownTracker::BUDGET.saturating_mul(4);
