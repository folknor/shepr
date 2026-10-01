use std::time::Duration;

/// Maximum retry delay for host-shutdown session checkpoints.
pub(crate) const HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

/// Initial delay after the logind shutdown signal stream is lost. The delay
/// retries promptly while avoiding a reconnect spin.
pub(crate) const SHUTDOWN_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);

/// Maximum delay while reconnecting after the logind shutdown signal stream is
/// lost. The cap bounds recovery latency while keeping repeated failures
/// inexpensive.
pub(crate) const SHUTDOWN_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Multiplier for ordinary logind reconnect backoff between retries. Doubling
/// grows quickly after failure while the delay stays capped above.
pub(crate) const SHUTDOWN_RECONNECT_BACKOFF_MULTIPLIER: u32 = 2;

/// Bounded queue capacity for events forwarded from client threads.
pub(crate) const SERVER_EVENT_CHANNEL_CAPACITY: usize = 64;

/// How long server exit waits for pane teardowns: their signal budget, plus
/// three more of it for the /proc session scans between signal rounds, which
/// the signal budget does not count.
pub(crate) const PANE_TEARDOWN_WAIT: Duration =
    shepr_mux::pane::PaneTeardownTracker::BUDGET.saturating_mul(4);

/// Minimum spacing between renders, matching a typical display refresh
/// cadence: rendering faster only produces frames no screen can show, while
/// output bursts coalesce into the next frame.
pub(crate) const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);
/// Refresh Git ahead/behind status periodically while clients are connected,
/// keeping it fresh without probing on every render.
pub(crate) const GIT_REMOTE_STATUS_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
/// Rediscover repository roots periodically so external cwd changes settle.
pub(crate) const GIT_REPO_DISCOVERY_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Wait briefly for live host colors; afterward the saved theme, if any, stays
/// the fallback for resumed agents.
pub(crate) const PENDING_AGENT_RESUME_THEME_WAIT: Duration = Duration::from_millis(750);
/// Coalesce ordinary session writes to avoid saving on every event.
pub(crate) const SESSION_SAVE_DEBOUNCE: Duration = Duration::from_secs(5);
/// Buffer app events while rendering, enough for bursts without an
/// unbounded queue of stale state transitions.
pub(crate) const APP_EVENT_CHANNEL_CAPACITY: usize = 256;
/// Limit app events per loop pass so clients still get service.
pub(crate) const APP_EVENT_DRAIN_LIMIT: usize = 64;
/// Refuse new checkout-root work when this combined count of worker threads
/// and queued completions reaches the limit. Resume checks add at most one
/// completion per restored agent pane in a finite restore batch.
pub(crate) const MAX_WORKER_COMPLETION_BACKLOG: usize = 8;
/// Limit API requests per loop pass so client and scheduled work still get service.
pub(crate) const API_REQUEST_DRAIN_LIMIT: usize = 64;
/// Bound queued API requests to the number of active API connections. Each
/// connection has at most one request in the queue at a time. Requests whose
/// connection gave up waiting stay queued, so a stalled loop can fill the
/// queue; producers then refuse new requests with `server_unavailable` at
/// once instead of growing it. A refused agent hook report is dropped, as it
/// is when the server is down.
pub(crate) const API_REQUEST_CHANNEL_CAPACITY: usize = shepr_api::MAX_ACTIVE_CONNECTIONS;
/// Limit server events per loop pass so API and scheduled work still get service.
pub(crate) const SERVER_EVENT_DRAIN_LIMIT: usize = 64;

/// First session-save retry: prompt recovery without a busy loop.
pub(crate) const SESSION_SAVE_RETRY_MIN: Duration = Duration::from_millis(250);
/// Longest session-save retry, limiting failing-disk pressure.
pub(crate) const SESSION_SAVE_RETRY_MAX: Duration = Duration::from_secs(30);
/// Repeated failed critical checkpoints release shutdown delay or exited panes;
/// persistence must not stall either indefinitely.
pub(crate) const CHECKPOINT_MAX_FAILURES: u8 = 3;

/// Retry a restored agent launch when it has not consumed its plan;
/// this bounds both launch latency and repeated work.
pub(crate) const PENDING_AGENT_RESUME_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// How long a restored agent's saved directory may take to stat before the
/// resume treats it as unavailable. A stat stuck on a hung mount cannot be
/// cancelled; past this the resume is abandoned and its thread left to finish.
pub(crate) const RESUME_CWD_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Refuse oversized copy-mode queries to bound search work per request.
pub(crate) const MAX_QUERY_BYTES: usize = 4096;
/// Limit copy-mode matches to bound each response.
pub(crate) const MAX_RETURNED_MATCHES: usize = 1024;

/// Total time a client gets to deliver its complete handshake frame.
///
/// This is a single deadline across every read of the hello, not a per-read idle
/// timeout: `shepr_platform::ipc::LocalStreamDeadlineReader` polls for readiness with only
/// the time left before each read, so a peer trickling bytes cannot
/// hold the handshake thread open. The deadline leaves room for OS timer slack,
/// thread scheduling, and cleanup overhead.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);
/// Maximum time a client stream writer may make no progress before disconnecting it.
pub(crate) const CLIENT_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// First retry after automatic workspace creation fails, such as when the
/// configured shell stops resolving after server launch.
pub(crate) const DEFAULT_WORKSPACE_RETRY_MIN: Duration = Duration::from_millis(250);
/// Cap on the doubling retry delay of automatic workspace creation, so it
/// still recovers soon after the shell or working directory becomes usable.
pub(crate) const DEFAULT_WORKSPACE_RETRY_MAX: Duration = Duration::from_secs(30);
/// Bound each client's outstanding control messages, including the message
/// currently being written to its socket. Control messages do not coalesce, and
/// a healthy client can see bursts of them (a snapshot per changed projection
/// while a pane animates its title, a run of clipboard writes, mode updates on
/// reconnect), so the count sits well above a burst; the byte bound below is
/// what holds memory.
pub(crate) const CLIENT_CONTROL_QUEUE_MAX_ITEMS: usize = 1024;
/// Bound control memory per client even when a peer reads slowly but continues
/// to make enough progress to stay inside the socket stall timeout.
pub(crate) const CLIENT_CONTROL_QUEUE_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Frames needed to carry a full control queue of endpoint response bytes.
pub(crate) const MAX_ENDPOINT_RESPONSE_FRAME_COUNT: usize =
    CLIENT_CONTROL_QUEUE_MAX_BYTES.div_ceil(shepr_protocol::MAX_FRAME_SIZE);
/// Encoded size cap of one endpoint response, leaving room for frame prefixes.
pub(crate) const MAX_ENDPOINT_RESPONSE_ENCODED_BYTES: usize =
    CLIENT_CONTROL_QUEUE_MAX_BYTES - std::mem::size_of::<u32>() * MAX_ENDPOINT_RESPONSE_FRAME_COUNT;
/// How long a transport thread waits for a client it could not register to
/// receive its shutdown frame.
pub(crate) const UNREGISTERED_SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// Poll spacing while that transport thread waits for the flush.
pub(crate) const UNREGISTERED_SHUTDOWN_FLUSH_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Upper bound on the wait for client writers to flush their shutdown frames.
pub(crate) const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// Bound endpoint boot identifiers above the size of generated IDs.
pub(crate) const MAX_ENDPOINT_BOOT_ID_BYTES: usize = 128;
/// Bound endpoint request identifiers above the size of generated IDs.
pub(crate) const MAX_ENDPOINT_REQUEST_ID_BYTES: usize = 128;
/// Refresh shell cwd projections periodically when no OSC 7 report arrives.
pub(crate) const SHELL_CWD_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Give Tokio tasks a short time to stop after a failed startup; teardown continues
/// even if a task is stuck.
pub(crate) const TOKIO_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

/// The fraction of a split one resize step moves its edge by.
pub(crate) const DEFAULT_PANE_RESIZE_AMOUNT: f32 = 0.05;
