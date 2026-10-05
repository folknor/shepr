/// How many times the local server is offered a restart. A server replaced
/// between the observation and the stop is a new occupant and is offered once
/// more; beyond that something keeps restarting it.
pub(crate) const MAX_LOCAL_OFFERS: usize = 2;
