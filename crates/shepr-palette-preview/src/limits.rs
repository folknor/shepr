//! Bounds of the terminal colour query.

use std::time::Duration;

/// How long the preview waits for the terminal to answer its colour queries.
/// A terminal answers the device attributes query sent after them once every
/// colour reply is out, so this only runs out on a terminal that answers
/// nothing.
pub(crate) const QUERY_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Bytes read from the terminal per read while collecting the replies.
pub(crate) const QUERY_READ_CHUNK_BYTES: usize = 4096;
