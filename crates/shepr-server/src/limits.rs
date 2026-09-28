use std::time::Duration;

/// Delay between checks while a session save worker is still running.
pub(crate) const SESSION_SAVE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum retry delay for host-shutdown session checkpoints.
pub(crate) const HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

/// Bounded queue capacity for events forwarded from client threads.
pub(crate) const SERVER_EVENT_CHANNEL_CAPACITY: usize = 64;
