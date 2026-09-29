use std::time::Duration;

/// Delay between checks while the session persister is still running a save.
pub(crate) const SESSION_SAVE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum retry delay for host-shutdown session checkpoints.
pub(crate) const HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

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
/// Refresh Git ahead/behind status periodically while it is visible, keeping it
/// fresh without probing on every render.
pub(crate) const GIT_REMOTE_STATUS_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
/// Rediscover repository roots periodically so external cwd changes settle.
pub(crate) const GIT_REPO_DISCOVERY_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Wait briefly for restored agent theme reports before assigning a fallback.
pub(crate) const PENDING_AGENT_RESUME_THEME_WAIT: Duration = Duration::from_millis(750);
/// Coalesce ordinary session writes to avoid saving on every event.
pub(crate) const SESSION_SAVE_DEBOUNCE: Duration = Duration::from_secs(5);
/// Buffer app events while rendering, enough for bursts without an
/// unbounded queue of stale state transitions.
pub(crate) const APP_EVENT_CHANNEL_CAPACITY: usize = 256;
/// Limit app events per loop pass so clients still get service.
pub(crate) const APP_EVENT_DRAIN_LIMIT: usize = 64;

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

/// Refuse oversized copy-mode queries to bound search work per request.
pub(crate) const MAX_QUERY_BYTES: usize = 4096;
/// Limit copy-mode matches to bound each response.
pub(crate) const MAX_RETURNED_MATCHES: usize = 1024;

/// Total time a client gets to deliver its complete handshake frame.
///
/// This is a single deadline across every read of the hello, not a per-read idle
/// timeout: `shepr_platform::ipc::DeadlineReader` polls for readiness with only
/// the time left before each read, so a peer trickling bytes cannot
/// hold the handshake thread open. The deadline leaves room for OS timer slack,
/// thread scheduling, and cleanup overhead.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);
/// How long a transport thread waits for a client it could not register to
/// receive its shutdown frame.
pub(crate) const UNREGISTERED_SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// Poll spacing while that transport thread waits for the flush.
pub(crate) const UNREGISTERED_SHUTDOWN_FLUSH_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Upper bound on the wait for client writers to flush their shutdown frames.
pub(crate) const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// Bound expanded input events per batch to limit dispatch work.
pub(crate) const MAX_INPUT_EVENT_BATCH: usize = 4096;
/// Bound endpoint boot identifiers above the size of generated IDs.
pub(crate) const MAX_ENDPOINT_BOOT_ID_BYTES: usize = 128;
/// Bound endpoint request identifiers above the size of generated IDs.
pub(crate) const MAX_ENDPOINT_REQUEST_ID_BYTES: usize = 128;
/// Refresh shell cwd projections periodically when no OSC 7 report arrives.
pub(crate) const SHELL_CWD_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Give Tokio tasks a short time to stop after a failed startup; teardown continues
/// even if a task is stuck.
pub(crate) const TOKIO_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

/// Refresh the tab bar clock periodically so it displays the current time.
pub(crate) const DATETIME_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Bound captured status-command bytes to prevent unbounded buffers.
pub(crate) const MAX_COMMAND_LINE_BYTES: usize = 4096;
/// Bound status text so a segment cannot fill the tab bar.
pub(crate) const MAX_TAB_BAR_TEXT_CHARS: usize = 80;
/// Read status output in chunks smaller than its capture cap.
pub(crate) const TAB_BAR_STATUS_READ_BUFFER_BYTES: usize = 1024;
/// Tab bar status commands use the system POSIX shell, independently of the
/// interactive pane shell, so command syntax is stable across panes.
pub(crate) const TAB_BAR_COMMAND_SHELL: &str = "/bin/sh";
/// Run the status command as a login shell (`-l`) so the user's login profile
/// supplies its command environment, and execute the configured text (`-c`).
pub(crate) const TAB_BAR_COMMAND_SHELL_ARGS: &str = "-lc";

/// Resize by a small visible step when the caller omits an amount.
pub(crate) const DEFAULT_PANE_RESIZE_AMOUNT: f32 = 0.05;
/// Bound the requested resize fraction to avoid extreme pane jumps.
pub(crate) const MAX_PANE_RESIZE_AMOUNT: f32 = 0.5;
