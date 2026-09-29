use std::time::Duration;

/// How many times the local server is offered a restart. A server replaced
/// between the observation and the stop is a new occupant and is offered once
/// more; beyond that something keeps restarting it.
pub(crate) const MAX_LOCAL_OFFERS: usize = 2;

/// Maximum wait for the fresh local server's client socket, shared with the
/// remote launch helper so both use the same startup window.
pub(crate) const SERVER_READY_TIMEOUT: Duration = shepr_remote::local_server::SERVER_READY_TIMEOUT;
