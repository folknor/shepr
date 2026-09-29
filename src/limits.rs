use std::time::Duration;

/// Poll interval while the CLI waits for an agent to become ready. It keeps
/// readiness detection prompt without rapid status requests.
pub(crate) const AGENT_START_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Time to allow a pane shell to become ready again after a failed start.
/// The timeout gives shell prompt detection a short recovery window before retry.
pub(crate) const PANE_SHELL_READINESS_RETRY_TIMEOUT: Duration = Duration::from_secs(2);

/// Default readiness timeout for `agent start`, in milliseconds. It allows
/// slower agent startup while keeping an omitted timeout bounded.
pub(crate) const DEFAULT_AGENT_START_TIMEOUT_MS: u64 = 30_000;

/// Maximum wait for the fresh local server's client socket, shared with the
/// remote launch helper so both use the same startup window.
pub(crate) const SERVER_READY_TIMEOUT: Duration = shepr_remote::local_server::SERVER_READY_TIMEOUT;

/// Minimum display width for the session name column, leaving room for the
/// usual session names before the next column starts.
pub(crate) const SESSION_TABLE_NAME_WIDTH: usize = 20;

/// Minimum display width for the session status column, enough for `stopped`.
pub(crate) const SESSION_TABLE_STATUS_WIDTH: usize = 8;

/// Minimum display width for the session directory column, keeping common
/// paths readable before the socket column starts.
pub(crate) const SESSION_TABLE_DIRECTORY_WIDTH: usize = 48;
