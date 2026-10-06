//! Timeouts, intervals, retry schedules and capacity bounds of the server.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Session saves and checkpoints
// ---------------------------------------------------------------------------

/// Coalesce ordinary session writes to avoid saving on every event.
pub(crate) const SESSION_SAVE_DEBOUNCE: Duration = Duration::from_secs(5);
/// First session-save retry: prompt recovery without a busy loop.
pub(crate) const SESSION_SAVE_RETRY_MIN: Duration = Duration::from_millis(250);
/// Longest session-save retry, limiting failing-disk pressure.
pub(crate) const SESSION_SAVE_RETRY_MAX: Duration = Duration::from_secs(30);
/// First retry of a failed pane-exit or host-shutdown checkpoint, independent
/// of ordinary autosaves. Each later retry doubles it, and
/// `CHECKPOINT_MAX_FAILURES` alone bounds the schedule, so there is no cap.
/// The attempts themselves are unbounded filesystem IO, so the schedule's sum
/// is not a bound on how long a checkpoint holds logind's shutdown delay.
pub(crate) const CHECKPOINT_RETRY_MIN: Duration = Duration::from_millis(250);
/// Repeated failed critical checkpoints release shutdown delay or exited panes;
/// persistence must not stall either indefinitely.
pub(crate) const CHECKPOINT_MAX_FAILURES: u8 = 3;

// ---------------------------------------------------------------------------
// Event loop channels and per-pass fairness
// ---------------------------------------------------------------------------

/// Buffer app events while rendering, enough for bursts without an
/// unbounded queue of stale state transitions.
pub(crate) const APP_EVENT_CHANNEL_CAPACITY: usize = 256;
/// Bounded queue capacity for events forwarded from client threads.
pub(crate) const SERVER_EVENT_CHANNEL_CAPACITY: usize = 64;
/// Bound queued API requests to the number of app-bound API requests in
/// flight. Each has at most one request in the queue at a time, and timed-out
/// connection workers keep their app slots until the request is resolved or
/// dropped. A full queue is refused as `endpoint_busy`, matching app-slot
/// admission. A refused agent hook report is dropped, as it is when the server
/// is down.
pub(crate) const API_REQUEST_CHANNEL_CAPACITY: usize = shepr_api::MAX_APP_REQUESTS_IN_FLIGHT;

// The three drain limits below share one purpose: each source of loop work
// gets a bounded share of a pass, so no source starves the others.

/// Limit app events per loop pass so clients still get service.
pub(crate) const APP_EVENT_DRAIN_LIMIT: usize = 64;
/// Limit server events per loop pass so API and scheduled work still get service.
pub(crate) const SERVER_EVENT_DRAIN_LIMIT: usize = 64;
/// Limit API requests per loop pass so client and scheduled work still get service.
pub(crate) const API_REQUEST_DRAIN_LIMIT: usize = 64;

// ---------------------------------------------------------------------------
// Loop cadence
// ---------------------------------------------------------------------------

/// Minimum spacing between renders, matching a typical display refresh
/// cadence: rendering faster only produces frames no screen can show, while
/// output bursts coalesce into the next frame.
pub(crate) const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);
/// Refresh shell cwd projections periodically when no OSC 7 report arrives.
pub(crate) const SHELL_CWD_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Refresh Git ahead/behind status periodically while clients are connected,
/// keeping it fresh without probing on every render.
pub(crate) const GIT_REMOTE_STATUS_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
/// Check an in-flight refresh for lost completion or stalled progress.
pub(crate) const GIT_LOST_REFRESH_CHECK_INTERVAL: Duration = Duration::from_millis(1500);
/// Rediscover repository roots periodically so external cwd changes settle.
pub(crate) const GIT_REPO_DISCOVERY_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// First retry after automatic workspace creation fails, such as when the
/// configured shell stops resolving after server launch.
pub(crate) const DEFAULT_WORKSPACE_RETRY_MIN: Duration = Duration::from_millis(250);
/// Cap on the doubling retry delay of automatic workspace creation, so it
/// still recovers soon after the shell or working directory becomes usable.
pub(crate) const DEFAULT_WORKSPACE_RETRY_MAX: Duration = Duration::from_secs(30);
/// Wait briefly for live host colors; afterward the saved theme, if any, stays
/// the fallback for resumed agents.
pub(crate) const PENDING_AGENT_RESUME_THEME_WAIT: Duration = Duration::from_millis(750);

// ---------------------------------------------------------------------------
// Client connections
// ---------------------------------------------------------------------------

/// Total time a client gets to deliver its complete handshake frame.
///
/// This is a single deadline across every read of the hello, not a per-read idle
/// timeout: `shepr_platform::ipc::LocalStreamDeadlineReader` polls for readiness with only
/// the time left before each read, so a peer trickling bytes cannot
/// hold the handshake thread open. The deadline stays below
/// `HANDSHAKE_CLOSE_BOUND`, leaving room for OS timer slack, thread scheduling,
/// and cleanup overhead.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);
/// How soon after its accept a connection that has not completed its handshake
/// is closed.
const HANDSHAKE_CLOSE_BOUND: Duration = Duration::from_secs(5);

const _: () = assert!(HANDSHAKE_TIMEOUT.as_millis() < HANDSHAKE_CLOSE_BOUND.as_millis());
/// Maximum time a client stream writer may make no progress before disconnecting it.
pub(crate) const CLIENT_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound each client's outstanding control messages, including the message
/// currently being written to its socket. Control messages do not coalesce, and
/// a healthy client can see bursts of them (a snapshot per changed projection
/// while a pane animates its title, a run of clipboard writes, mode updates on
/// reconnect), so the count sits well above a burst; the byte bound below is
/// what holds memory.
pub(crate) const CLIENT_CONTROL_QUEUE_MAX_ITEMS: usize = 1024;
/// Bound control memory per client even when a peer reads slowly but continues
/// to make enough progress to stay inside the socket stall timeout. The
/// endpoint response and held reply bounds below derive from it.
pub(crate) const CLIENT_CONTROL_QUEUE_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Frames needed to carry a full control queue of endpoint response bytes.
const MAX_ENDPOINT_RESPONSE_FRAME_COUNT: usize =
    CLIENT_CONTROL_QUEUE_MAX_BYTES.div_ceil(shepr_protocol::MAX_FRAME_SIZE);
/// Encoded size cap of one endpoint response, leaving room for frame prefixes.
pub(crate) const MAX_ENDPOINT_RESPONSE_ENCODED_BYTES: usize =
    CLIENT_CONTROL_QUEUE_MAX_BYTES - std::mem::size_of::<u32>() * MAX_ENDPOINT_RESPONSE_FRAME_COUNT;
/// Most endpoint replies held for one client until their release. A
/// same-build client has one command in flight per endpoint, so a legitimate
/// backlog is one or two entries; a client past this is dropped, not
/// buffered for.
pub(crate) const MAX_HELD_ENDPOINT_REPLIES: usize = 64;
/// Most framed reply bytes held for one client. A held reply
/// drains into the control lane, whose own byte budget is this size.
pub(crate) const MAX_HELD_ENDPOINT_REPLY_BYTES: usize = CLIENT_CONTROL_QUEUE_MAX_BYTES;

// ---------------------------------------------------------------------------
// Copy mode
// ---------------------------------------------------------------------------

/// Refuse oversized copy-mode queries to bound search work per request.
pub(crate) const MAX_QUERY_BYTES: usize = 4096;
/// Limit copy-mode matches to bound each response.
pub(crate) const MAX_RETURNED_MATCHES: usize = 1024;

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// How long a workspace's geometry must hold before its panes' terminal grids
/// and PTYs follow a resize that may be one step of a gesture (a split border
/// or host window being dragged, ratio commands replayed from a slow link).
/// Every PTY resize sends its child a SIGWINCH, and a shell redraws its prompt
/// on each one; the terminal reflows wrapped lines in between, so a burst of
/// them leaves stacked prompt fragments behind. Long enough to span the gaps
/// between a drag's steps (the client's drag send throttle is several times
/// shorter), short enough that the pane settles about when the pointer stops.
/// Layout and drawing follow at once; only the resize waits.
pub(crate) const PANE_RESIZE_SETTLE: Duration = Duration::from_millis(120);

/// The fraction of a split one keyboard resize step moves its edge by.
pub(crate) const PANE_RESIZE_STEP: shepr_core::layout::RatioDelta =
    shepr_core::layout::RatioDelta::new(0.05);

// ---------------------------------------------------------------------------
// Startup and shutdown
// ---------------------------------------------------------------------------

/// Give Tokio tasks a short time to stop after a failed startup; teardown continues
/// even if a task is stuck.
pub(crate) const TOKIO_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
/// How long server exit waits for pane teardowns, including the mux's
/// scan allowance derived from its signal sequence. Session scans remain
/// unbounded, so unfinished panes are reported when this allowance expires.
pub(crate) const PANE_TEARDOWN_WAIT: Duration = shepr_mux::pane::PaneTeardownTracker::SHUTDOWN_WAIT;
/// Upper bound on the wait for client writers to flush their shutdown frames.
pub(crate) const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a transport thread waits for a client it could not register to
/// receive its shutdown frame.
pub(crate) const UNREGISTERED_SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// Poll spacing while that transport thread waits for the flush.
pub(crate) const UNREGISTERED_SHUTDOWN_FLUSH_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Initial delay after the logind shutdown signal stream is lost. The delay
/// retries promptly while avoiding a reconnect spin.
pub(crate) const SHUTDOWN_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
/// Maximum delay while reconnecting after the logind shutdown signal stream is
/// lost. The cap bounds recovery latency while keeping repeated failures
/// inexpensive.
pub(crate) const SHUTDOWN_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);
/// A watch must stay connected this long before its reconnect failure streak resets.
pub(crate) const SHUTDOWN_RECONNECT_STABLE_TIME: Duration = Duration::from_secs(60);
