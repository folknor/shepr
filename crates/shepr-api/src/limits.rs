use std::time::Duration;

/// Poll interval for waiting on app responses and client disconnects. One
/// tenth of a second keeps cancellation responsive without busy polling.
pub(crate) const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bound on how long an ordinary (non-wait, non-stream) request waits for the
/// app main loop to answer. Without one, a stalled main loop hangs every CLI
/// call and every agent hook that shells out to the CLI.
///
/// Most requests are answered in the same loop turn. The slowest legitimate
/// case is a `pane.read`/`agent.read` of alternate-screen history, which the
/// server serves by scrolling the agent and can take up to 20 s (15 s harvest
/// plus 5 s restore in `crates/shepr-server/src/server/alt_screen_read.rs`), and a second read of
/// the same pane is parked until the first finishes. A minute covers that
/// with margin. Requests that carry their own timeout (`events.wait`,
/// `agent.wait`, `pane.wait_for_output`, `agent.prompt` with `wait`) are
/// dispatched on their own paths and are not subject to this bound.
pub(crate) const ORDINARY_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Extra client-side allowance beyond the server request deadline, so the
/// server can return its more specific timeout response first.
const ORDINARY_RESPONSE_GRACE: Duration = Duration::from_secs(5);

/// Client response deadline derived from the server request deadline plus a
/// short allowance for the server to report that deadline.
pub(crate) const ORDINARY_RESPONSE_TIMEOUT: Duration =
    Duration::from_secs(ORDINARY_REQUEST_TIMEOUT.as_secs() + ORDINARY_RESPONSE_GRACE.as_secs());

/// Bounds how long synchronous app dispatch waits for the main loop. Five
/// seconds leaves a stalled loop detectable while covering a normal loop turn.
pub(crate) const APP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Deadline for a client to send its first request line after connecting.
/// Five seconds gives local clients time to serialize while bounding idle peers.
pub(crate) const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Send deadline used by an otherwise unbounded client request, derived from
/// the server's initial request-line deadline.
pub(crate) const UNBOUNDED_RESPONSE_SEND_TIMEOUT: Duration = INITIAL_REQUEST_TIMEOUT;

/// Bounds how long the server waits for a busy caller's request ID before
/// refusing the connection without one. Half a second gives a live local
/// client time to send its line without letting it stall refusal handling.
pub(crate) const BUSY_REQUEST_ID_TIMEOUT: Duration = Duration::from_millis(500);

/// Bounds writes to an API client so a stalled peer cannot hold a worker. Five
/// seconds allows a short local scheduling stall without tying up a thread.
pub(crate) const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum bytes accepted for an initial request line, shared with the wire
/// protocol's request-size limit.
pub(crate) const MAX_INITIAL_REQUEST_BYTES: usize = shepr_protocol::MAX_INITIAL_REQUEST_BYTES;

/// Read chunk size for initial API request lines. Eight KiB amortizes reads
/// while keeping each stack buffer small and fixed.
pub(crate) const INITIAL_REQUEST_READ_CHUNK_BYTES: usize = 8 * 1024;

/// Maximum concurrently served API connections. This bounds worker threads
/// and request-owned stream state while allowing several clients and hooks.
pub(crate) const MAX_ACTIVE_CONNECTIONS: usize = 64;

/// Maximum busy connections queued for request-ID extraction; additional
/// refusals are sent immediately so the accept loop stays available.
pub(crate) const BUSY_REFUSAL_QUEUE: usize = 16;

/// First accept-loop retry delay. Ten milliseconds avoids a tight error loop
/// while attempting quick recovery after a transient resource failure.
pub(crate) const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);

/// Maximum accept-loop retry delay. The one-second ceiling bounds recovery
/// latency during persistent resource failures while exponential backoff rests.
pub(crate) const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// Maximum number of recent API events retained for wait and subscription
/// consumers. Five hundred twelve events provide a useful recent replay window
/// while keeping the shared history bounded.
pub(crate) const MAX_EVENT_HISTORY: usize = 512;

/// Slack past a wait's own `timeout_ms`. At its deadline a wait still makes a
/// final app probe (bounded by [`APP_RESPONSE_TIMEOUT`]), and
/// `agent.prompt --wait` chains a submission step and two status waits that
/// can each overrun by one such probe. This only has to exceed those
/// overruns; it is not what normally ends a wait.
pub(crate) const WAIT_RESPONSE_GRACE: Duration = Duration::from_secs(30);

/// Maximum time spent waiting for a prompt effect to appear as agent activity.
/// Five seconds bounds a stalled submission while allowing normal detection.
pub(crate) const AGENT_PROMPT_EFFECT_TIMEOUT_MS: u64 = 5_000;

/// Allows the agent-prompt handler's app response to trail the user deadline
/// long enough for the app's own timeout response to arrive. One second gives
/// that final status a chance to win without materially extending the wait.
pub(crate) const AGENT_PROMPT_RESPONSE_GRACE: Duration = Duration::from_secs(1);

/// Maximum time a session stop waits for both session sockets to disappear.
/// Fifteen seconds leaves time for orderly shutdown before reporting a stall.
pub(crate) const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll interval while waiting for session sockets to disappear. Twenty-five
/// milliseconds bounds shutdown detection latency without rapid repeated probes.
pub(crate) const STOP_WAIT_POLL: Duration = Duration::from_millis(25);

/// Status probe deadline before a stop treats the server build as unknown.
/// Two seconds gives a local server time to answer while keeping stop responsive.
pub(crate) const STOP_STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum regex-output subscriptions per API stream, bounding repeated regex
/// work on each pane update.
pub(crate) const MAX_REGEX_MATCH_SUBSCRIPTIONS: usize = 32;

/// Maximum compiled regex program size for API output matching. The 256 KiB
/// cap limits memory spent on a caller-supplied expression.
pub(crate) const MATCH_REGEX_SIZE_LIMIT: usize = 256 * 1024;

/// Maximum lazy DFA cache size for API output matching, separately bounding
/// the regex engine's cached automaton memory to 256 KiB.
pub(crate) const MATCH_REGEX_DFA_SIZE_LIMIT: usize = 256 * 1024;
