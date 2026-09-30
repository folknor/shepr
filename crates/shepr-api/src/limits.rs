use std::time::Duration;

/// Bound on how long a request waits for the app main loop to answer. Without
/// one, a stalled main loop hangs every CLI call and every agent hook report.
///
/// Every request (status, detection capture and explain, hook reports, stop)
/// is answered within a loop turn or two and returns a
/// bounded response, so this only has to sit comfortably above one slow turn
/// while still failing a stalled loop promptly.
pub(crate) const ORDINARY_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Extra client-side allowance beyond the server request deadline, so the
/// server can return its more specific timeout response first.
const ORDINARY_RESPONSE_GRACE: Duration = Duration::from_secs(5);

/// Client response deadline derived from the server request deadline plus a
/// short allowance for the server to report that deadline.
pub(crate) const ORDINARY_RESPONSE_TIMEOUT: Duration =
    Duration::from_secs(ORDINARY_REQUEST_TIMEOUT.as_secs() + ORDINARY_RESPONSE_GRACE.as_secs());

/// Deadline for a client to send its first request line after connecting.
/// It gives local clients time to serialize while bounding idle peers.
pub(crate) const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds how long the server waits for a busy caller's request ID before
/// refusing the connection without one. It gives a live local client time to
/// send its line without letting it stall refusal handling.
pub(crate) const BUSY_REQUEST_ID_TIMEOUT: Duration = Duration::from_millis(500);

/// Bounds writes to an API client so a stalled peer cannot hold a worker while
/// allowing for a short local scheduling stall.
pub(crate) const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum bytes accepted for an initial request line, shared with the wire
/// protocol's request-size limit.
pub(crate) const MAX_INITIAL_REQUEST_BYTES: usize = shepr_protocol::MAX_INITIAL_REQUEST_BYTES;

/// Read chunk size for initial API request lines. A fixed chunk amortizes reads
/// while keeping each stack buffer small and fixed.
pub(crate) const INITIAL_REQUEST_READ_CHUNK_BYTES: usize = 8 * 1024;

/// Maximum concurrently served API connections. This bounds worker threads
/// and request-owned stream state while allowing several clients and hooks.
pub(crate) const MAX_ACTIVE_CONNECTIONS: usize = 64;

/// Maximum busy connections queued for request-ID extraction; additional
/// refusals are sent immediately so the accept loop stays available.
pub(crate) const BUSY_REFUSAL_QUEUE: usize = 16;

/// First accept-loop retry delay. The short pause avoids a tight error loop
/// while attempting quick recovery after a transient resource failure.
pub(crate) const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);

/// Maximum accept-loop retry delay. The ceiling bounds recovery
/// latency during persistent resource failures while exponential backoff rests.
pub(crate) const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// Maximum time a server stop waits for the named server to stop answering, or
/// for both sockets to disappear when the stop was not conditional.
pub(crate) const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum time a server stop waits for a data-directory lease after the
/// stopped server no longer answers or its sockets are gone. The server
/// releases its lease before removing its sockets; a later holder may be a new
/// process using the same data directory.
pub(crate) const STOP_LEASE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-request deadline while polling the server's boot identity after a stop.
pub(crate) const STOP_STATUS_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Poll interval while waiting for a server to stop answering or its sockets
/// to disappear. It bounds shutdown detection latency without rapid repeated
/// probes.
pub(crate) const STOP_WAIT_POLL: Duration = Duration::from_millis(25);
