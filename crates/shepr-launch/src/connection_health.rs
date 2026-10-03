//! The one timing fact both ends of a client connection share. The client
//! probes every endpoint, local or SSH, after [`HEARTBEAT_INTERVAL`] of
//! silence, and the server answers. Anything that relays the connection and
//! expires it when no byte moves, as the remote host's SSH bridge does, must
//! wait several of these intervals before it gives up, and asserts that
//! against this value.

pub use crate::limits::HEARTBEAT_INTERVAL;
