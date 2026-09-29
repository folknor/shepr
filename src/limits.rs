use std::time::Duration;

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
