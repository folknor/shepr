use std::time::Duration;

/// Bound on how long a request waits for the app main loop to answer. Without
/// one, a stalled main loop hangs every CLI call and every agent hook report.
///
/// Every request (status, detection capture and explain, hook reports, stop)
/// is answered within a loop turn or two and returns a
/// bounded response, so this only has to sit comfortably above one slow turn
/// while still failing a stalled loop promptly.
pub(crate) const ORDINARY_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum ordinary client wait for a local server whose listen backlog is
/// full. This matches the platform connect bound; the response budget starts
/// after this separate connect budget.
pub(crate) const ORDINARY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Extra client-side allowance beyond the server request deadline, so the
/// server can return its more specific timeout response first.
const ORDINARY_RESPONSE_GRACE: Duration = Duration::from_secs(5);

/// One client budget for writing and reading, derived from the server request
/// deadline plus a short allowance for the server to report that deadline.
pub(crate) const ORDINARY_RESPONSE_TIMEOUT: Duration =
    Duration::from_secs(ORDINARY_REQUEST_TIMEOUT.as_secs() + ORDINARY_RESPONSE_GRACE.as_secs());

/// One overall connect, write and response budget for a status ping. `ping`
/// is answered on the connection thread and never waits for the app loop, so
/// it needs none of the ordinary response window. Launch probes and
/// `ApiClient::ping` share this bound.
pub const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Maximum API connections reading their request, or writing an answer given
/// without the app loop (ping, the stops, parse errors and refusals). With
/// [`MAX_APP_REQUESTS_IN_FLIGHT`] it bounds API worker threads and their
/// stream state while allowing several clients and hooks.
pub(crate) const MAX_API_INGRESS_CONNECTIONS: usize = 64;

/// Maximum app-bound requests queued or executing. A socket timeout keeps its
/// app slot until the app resolves or drops the request, so abandoned queue
/// entries remain included in this bound. Kept apart from ingress so requests
/// held by a stalled loop cannot keep a stop from being read.
pub const MAX_APP_REQUESTS_IN_FLIGHT: usize = 64;

/// Maximum connections queued for the refuser thread, of either kind: over a
/// kind's admission limit, or over the classification limit with no first
/// byte yet. With the queue full, a known API connection is refused at once
/// without its request ID and any other connection is closed, so the accept
/// loop stays available.
pub(crate) const BUSY_REFUSAL_QUEUE: usize = 16;

/// First accept-loop retry delay. The short pause avoids a tight error loop
/// while attempting quick recovery after a transient resource failure.
pub(crate) const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);

/// Maximum accept-loop retry delay. The ceiling bounds recovery
/// latency during persistent resource failures while exponential backoff rests.
pub(crate) const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// Maximum concurrently served TUI connections, admitted separately from API
/// connections. A TUI connection's thread performs the handshake and then
/// reads until disconnect, holding its admission throughout, so this bounds
/// both handshake workers and connected client reader threads.
pub(crate) const MAX_ACTIVE_CLIENT_CONNECTIONS: usize = 64;

/// Maximum connection threads that exist only to wait for a peer's first byte,
/// which decides whether it is an API or a TUI connection. Over it, the
/// refuser waits a short bound for the byte instead, so silent peers cannot
/// make the server refuse a peer whose own kind has room.
pub(crate) const MAX_UNCLASSIFIED_CONNECTIONS: usize = 64;

/// Overall bound on reading a refused TUI connection's preamble and hello, so
/// an excess client cannot monopolize the refuser.
pub(crate) const BUSY_CLIENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(250);
