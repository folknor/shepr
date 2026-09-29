use std::time::Duration;

/// Maximum wait for the fresh local server's client socket, shared with the
/// remote launch helper so both use the same startup window.
pub(crate) const SERVER_READY_TIMEOUT: Duration = shepr_remote::local_server::SERVER_READY_TIMEOUT;
