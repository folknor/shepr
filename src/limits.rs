use std::time::Duration;

/// How many times the local server is offered a restart. A server replaced
/// between the observation and the stop is a new occupant and is offered once
/// more; beyond that something keeps restarting it.
pub(crate) const MAX_LOCAL_OFFERS: usize = 2;

/// How long `status` waits for the server's status answer. It matches the
/// API client's ordinary response window (the server's own request deadline
/// plus a short grace), so a slow but working loop still answers in time.
pub(crate) const STATUS_ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
