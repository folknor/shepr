use std::time::Duration;

/// Poll interval for waiting on app responses and client disconnects. It keeps
/// cancellation responsive without busy polling.
pub(crate) const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bound on how long an ordinary (non-wait, non-stream) request waits for the
/// app main loop to answer. Without one, a stalled main loop hangs every CLI
/// call and every agent hook that shells out to the CLI.
///
/// Most requests are answered in the same loop turn. The slowest legitimate
/// case is a `pane.read`/`agent.read` of alternate-screen history, which the
/// server serves by scrolling the agent, harvesting output, and restoring the
/// viewport, each phase under its own bound (`MAX_DURATION` and
/// `MAX_RESTORE_DURATION` in shepr-server's limits). A second read of the same
/// pane is parked until the first finishes, so a queued read can spend one
/// full worst-case read waiting before its own begins. The deadline must
/// cover both back to back with margin to spare, or a legitimate queued read
/// is reported as a stalled main loop. Requests that carry their own timeout
/// (`events.wait`) are dispatched on their own paths and are not subject to
/// this bound.
pub(crate) const ORDINARY_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Extra client-side allowance beyond the server request deadline, so the
/// server can return its more specific timeout response first.
const ORDINARY_RESPONSE_GRACE: Duration = Duration::from_secs(5);

/// Client response deadline derived from the server request deadline plus a
/// short allowance for the server to report that deadline.
pub(crate) const ORDINARY_RESPONSE_TIMEOUT: Duration =
    Duration::from_secs(ORDINARY_REQUEST_TIMEOUT.as_secs() + ORDINARY_RESPONSE_GRACE.as_secs());

/// Bounds how long synchronous app dispatch waits for the main loop, covering
/// a normal loop turn while making a stalled loop detectable.
pub(crate) const APP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Deadline for a client to send its first request line after connecting.
/// It gives local clients time to serialize while bounding idle peers.
pub(crate) const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Send deadline used by an otherwise unbounded client request, derived from
/// the server's initial request-line deadline.
pub(crate) const UNBOUNDED_RESPONSE_SEND_TIMEOUT: Duration = INITIAL_REQUEST_TIMEOUT;

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

/// Maximum number of recent API events retained for wait and subscription
/// consumers. The history provides a useful recent replay window
/// while keeping the shared history bounded.
pub(crate) const MAX_EVENT_HISTORY: usize = 512;

/// Largest `timeout_ms` an `events.wait` accepts: one day, the same ceiling
/// as a metadata TTL. A caller that means "until it happens" omits the
/// timeout; a larger value is refused rather than clamped, so nobody mistakes
/// a shortened wait for the one they asked for. The cap also keeps the
/// deadline far inside `Instant`'s range, so computing it cannot overflow.
pub(crate) const MAX_WAIT_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// Client-side slack past a wait's own `timeout_ms`. A wait checks its
/// deadline only after each poll, and a poll can block on an app probe for up
/// to [`APP_RESPONSE_TIMEOUT`], so the server's answer can trail the deadline
/// by one such probe. One extra second leaves a margin for scheduling and
/// delivery after the probe; the grace is not what normally ends a wait.
pub(crate) const WAIT_RESPONSE_GRACE: Duration =
    APP_RESPONSE_TIMEOUT.saturating_add(Duration::from_secs(1));

/// Maximum time a session stop waits for both session sockets to disappear,
/// leaving time for orderly shutdown before reporting a stall.
pub(crate) const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll interval while waiting for session sockets to disappear. It bounds
/// shutdown detection latency without rapid repeated probes.
pub(crate) const STOP_WAIT_POLL: Duration = Duration::from_millis(25);

/// Status probe deadline before a stop treats the server build as unknown. It
/// gives a local server time to answer while keeping stop responsive.
pub(crate) const STOP_STATUS_TIMEOUT: Duration = Duration::from_secs(2);
