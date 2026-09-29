use std::time::Duration;

/// Shared bound for random-name collisions when staging private sockets and
/// SSH configuration paths.
pub(super) const RANDOM_NAME_ATTEMPTS: u32 = 16;

/// Shared polling interval while waiting for Git and clipboard helper children.
pub(super) const HELPER_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Poll interval used to bound cancellation latency on client streams.
pub(super) const CLIENT_STREAM_POLL_INTERVAL_MS: i32 = 100;

/// Startup allowance for clipboard selection owners before detaching them.
pub(super) const CLIPBOARD_OWNER_STARTUP_WAIT: Duration = Duration::from_millis(100);

/// Interval between SSH agent liveness probes.
pub(super) const SSH_AGENT_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Initial delay after a shutdown signal stream is lost.
pub(super) const SHUTDOWN_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);

/// Maximum delay while reconnecting after a shutdown signal stream is lost.
pub(super) const SHUTDOWN_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Read chunk size for SSH bridge forwarding.
pub(super) const REMOTE_BRIDGE_COPY_BUFFER_BYTES: usize = 16 * 1024;

/// Read chunk size for bounded child output.
pub(super) const LIMITED_READ_BUFFER_BYTES: usize = 8 * 1024;
