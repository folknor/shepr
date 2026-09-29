use std::time::Duration;

/// Delay between checks while a session save worker is still running.
pub(crate) const SESSION_SAVE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum retry delay for host-shutdown session checkpoints.
pub(crate) const HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

/// Bounded queue capacity for events forwarded from client threads.
pub(crate) const SERVER_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Maximum manifest reload requests waiting behind the running reload. The API's
/// connection limit bounds them too; this cap holds whatever that limit becomes.
pub(crate) const AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY: usize = 64;

/// How long server exit waits for pane teardowns: their signal budget plus
/// additional time for the /proc session scans between signal rounds.
pub(crate) const PANE_TEARDOWN_WAIT: Duration =
    shepr_mux::pane::PaneTeardownTracker::BUDGET.saturating_mul(4);

/// Minimum spacing between renders to bound presentation work.
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

/// Agent start's default wait, long enough for ordinary shell initialization.
pub(crate) const DEFAULT_AGENT_START_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound the maximum agent start wait accepted from the API so a failed launch
/// eventually answers its caller.
pub const MAX_AGENT_START_TIMEOUT: Duration = Duration::from_secs(300);
/// Give an agent time to settle after submitting its launch command.
pub const AGENT_START_SETTLE_DELAY: Duration = Duration::from_secs(3);
/// Retry a restored agent launch when it has not consumed its plan;
/// this bounds both launch latency and repeated work.
pub(crate) const PENDING_AGENT_RESUME_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// How long a restored managed agent's name waits, once its resume command is
/// typed, for the agent's process (or a hook report from it) to appear. Only
/// the process has to show up, not reach a prompt, so this is generous. It is
/// the same hold the mux uses for detecting that agent during startup.
pub(crate) const MANAGED_AGENT_RESUME_TIMEOUT: Duration =
    shepr_mux::pane::MANAGED_AGENT_RESUME_TIMEOUT;
/// Pause after writing an agent prompt so the receiving TUI can process it.
pub(crate) const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

// Alt-screen history reads: the read first waits for the pane to stop
// changing, then sends scroll steps of `WHEEL_STEP_EVENTS` wheel events. Each
// step may take `STEP_TIMEOUT` and needs `OUTPUT_QUIET` of quiet output before
// harvesting. `MAX_DURATION` caps the traversal; restoration has its own
// `MAX_RESTORE_DURATION` so failure to settle cannot leave the user's viewport
// displaced indefinitely.

/// Initial settling time; avoids scrolling through a still-changing pane.
pub(crate) const INITIAL_QUIET: Duration = Duration::from_millis(10);
/// A quiet output window avoids harvesting an incomplete wheel response.
pub(crate) const OUTPUT_QUIET: Duration = Duration::from_millis(10);
/// A step deadline keeps an unresponsive scroll from stalling traversal.
pub(crate) const STEP_TIMEOUT: Duration = Duration::from_millis(120);
/// Bounds the total alt-screen history traversal.
pub(crate) const MAX_DURATION: Duration = Duration::from_secs(15);
/// Bounds viewport restoration after traversal stops.
pub(crate) const MAX_RESTORE_DURATION: Duration = Duration::from_secs(5);
/// Wheel events per step advance history without a large viewport jump.
pub(crate) const WHEEL_STEP_EVENTS: usize = 3;

/// Largest `lines` a read accepts. Larger requests are rejected rather than
/// quietly shortened, so a caller never mistakes a capped read for the whole
/// history it asked for.
pub(crate) const MAX_READ_LINES: u32 = 1000;
/// Recent reads default to approximately one tall terminal screen.
pub(crate) const DEFAULT_RECENT_READ_LINES: usize = 80;
/// A layout accepts a bounded number of panes to limit each apply operation's work.
pub(crate) const MAX_LAYOUT_PANES: usize = 24;
/// A layout accepts a bounded split depth to limit recursive walks.
pub(crate) const MAX_LAYOUT_DEPTH: usize = 16;
/// Refuse oversized copy-mode queries to bound search work per request.
pub(crate) const MAX_QUERY_BYTES: usize = 4096;
/// Limit copy-mode matches to bound each response.
pub(crate) const MAX_RETURNED_MATCHES: usize = 1024;

/// Longest metadata TTL, so stale agent metadata expires eventually.
pub(crate) const METADATA_TTL_MAX_MS: u64 = 86_400_000;
/// Smallest nonzero metadata TTL accepted by the API.
pub(crate) const METADATA_TTL_MIN_MS: u64 = 1;
/// Bound source labels so metadata cannot dominate agent rows.
pub(crate) const METADATA_SOURCE_MAX_CHARS: usize = 80;
/// Limit token keys changed per request to bound update work.
pub(crate) const MAX_METADATA_TOKEN_KEYS_PER_REQUEST: usize = 16;
/// Limit token keys per resource to bound persistent metadata.
pub(crate) const MAX_METADATA_TOKEN_KEYS_PER_RESOURCE: usize = 32;
/// Bound token key length so lookup names stay compact.
pub(crate) const MAX_METADATA_TOKEN_KEY_LEN: usize = 32;
/// Bound token value length so status text stays compact.
pub(crate) const MAX_METADATA_TOKEN_VALUE_LEN: usize = 80;
/// Characters kept from a pane's reported presentation text, after control
/// characters are dropped.
pub(crate) const MAX_PRESENTATION_TEXT_CHARS: usize = 80;

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
/// Minimum accepted attached client size.
///
/// Narrow observers must be allowed to drive narrow renders, otherwise the
/// server wraps pane content against a wider width and the client sees the
/// right edge clipped.
pub(crate) const MIN_CLIENT_COLS: u16 = 1;
/// The row counterpart of `MIN_CLIENT_COLS`.
pub(crate) const MIN_CLIENT_ROWS: u16 = 1;
/// Bound expanded input events per batch to limit dispatch work.
pub(crate) const MAX_INPUT_EVENT_BATCH: usize = 4096;
/// Endpoint request bound is the protocol's shared payload bound.
pub(crate) const MAX_ENDPOINT_COMMAND_BYTES: usize = shepr_protocol::MAX_ENDPOINT_COMMAND_BYTES;
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

/// Cap agent labels to keep sidebar names compact.
pub(crate) const MAX_AGENT_NAME_LEN: usize = 32;
/// Resize by a small visible step when the caller omits an amount.
pub(crate) const DEFAULT_PANE_RESIZE_AMOUNT: f32 = 0.05;
/// Bound the requested resize fraction to avoid extreme pane jumps.
pub(crate) const MAX_PANE_RESIZE_AMOUNT: f32 = 0.5;
