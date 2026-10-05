//! Client timing, capacity, and presentation limits.

use std::time::Duration;

// Pointer gestures: clicks, drags and selection edge scrolling.

/// Repeated clicks on the same spot within the gesture interval are a double click.
///
/// This keeps the gesture in the usual short desktop double-click window.
pub(crate) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(350);
/// Minimum spacing between requests sent by scrollbar and split drags.
///
/// This caps updates near the usual desktop frame cadence.
pub(crate) const MOUSE_DRAG_SEND_INTERVAL: Duration = Duration::from_millis(33);
/// Tick spacing for scrolling a selection while the pointer is outside the pane.
///
/// This keeps edge scrolling responsive without scheduling at every input event.
pub(crate) const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(30);
/// Minimum spacing of the frames a selection drag rebuilds.
///
/// This bounds redraw work to a practical frame cadence.
pub(crate) const SELECTION_REPAINT_INTERVAL: Duration = Duration::from_millis(16);
/// Pane scrollback lines moved by one mouse wheel notch.
///
/// A few lines make a wheel step useful without jumping a large part of the
/// visible history. It is fixed: there is no setting for it.
pub(crate) const MOUSE_WHEEL_SCROLL_LINES: u16 = 3;
/// Maximum lines scrolled for each pointer row beyond a selection edge.
///
/// Scaling lines with pointer distance makes edge scrolling accelerate smoothly.
pub(crate) const SELECTION_EDGE_SCROLL_LINES_PER_ROW: usize = 3;
/// Minimum lines moved on an edge-scroll tick.
///
/// This keeps the first edge-scroll step visible.
pub(crate) const MIN_SELECTION_EDGE_SCROLL_LINES: usize = 3;
/// Maximum lines moved on an edge-scroll tick.
///
/// The cap prevents a small pointer movement from skipping too far.
pub(crate) const MAX_SELECTION_EDGE_SCROLL_LINES: usize = 15;

// How long transient feedback stays on screen.

/// Time an endpoint error stays visible without another input event.
///
/// The timeout leaves time to read a transient error before it clears.
pub(crate) const ENDPOINT_ERROR_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an endpoint notice card stays up before it hides itself. A click on the card hides
/// it sooner; the timeout is what dismisses it when `ui.mouse_capture` is off.
///
/// The timeout keeps a notice available through a short recovery without leaving stale cards up.
pub(crate) const ENDPOINT_NOTICE_TIMEOUT: Duration = Duration::from_secs(10);
/// Keep a completed word-selection highlight visible for this interval.
///
/// The timeout leaves brief visual feedback after the selection copy completes.
pub(crate) const WORD_SELECTION_HIGHLIGHT_TIMEOUT: Duration = Duration::from_millis(500);
/// Keep workspace navigation feedback visible while its focus request is pending.
///
/// The timeout covers the normal focus round trip without leaving stale feedback on screen.
pub(crate) const WORKSPACE_HIGHLIGHT_TIMEOUT: Duration = Duration::from_secs(1);

// The host terminal: its input, its size, and repainting after it refused output.

/// Bound the keyboard-capability query's wait for a host terminal response.
///
/// A short wait covers normal terminal replies while keeping startup interactive.
pub(crate) const HOST_KEYBOARD_QUERY_TIMEOUT: Duration = Duration::from_millis(250);
/// Maximum host input buffered while the keyboard-capability query is pending.
///
/// The capacity holds terminal replies while bounding input from an unresponsive host.
pub(crate) const MAX_BUFFERED_HOST_INPUT: usize = 64 * 1024;
/// Scratch buffer size for each read from the outer terminal.
///
/// The chunk keeps blocking reads page-sized and bounds each temporary read buffer.
pub(crate) const HOST_INPUT_READ_CHUNK_BYTES: usize = 4096;
/// Poll spacing for terminal size changes that do not arrive through a signal.
///
/// The interval keeps polling responsive while avoiding a busy loop.
pub(crate) const TERMINAL_RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Fallback terminal cell width when the host does not report pixel geometry.
///
/// The conventional fallback width maps cell coordinates when the host omits pixel geometry.
pub(crate) const DEFAULT_CELL_WIDTH_PX: u32 = 8;
/// Fallback terminal cell height when the host does not report pixel geometry.
///
/// The conventional fallback height maps cell coordinates when the host omits pixel geometry.
pub(crate) const DEFAULT_CELL_HEIGHT_PX: u32 = 16;
/// The first wait before repainting after the host refused a frame or patch.
///
/// A short first wait redraws promptly after a transient refusal; the wait doubles from here
/// while the host keeps refusing.
pub(crate) const REFUSED_OUTPUT_RETRY_MIN: Duration = Duration::from_millis(50);
/// Factor the refused-output repaint wait grows by after each refusal, up to
/// `REFUSED_OUTPUT_RETRY_MAX`.
///
/// Doubling reaches the ceiling within a handful of refusals, so a host that stays broken
/// quickly stops being rewritten often, while one brief refusal costs only a short wait.
pub(crate) const REFUSED_OUTPUT_RETRY_GROWTH: u32 = 2;
/// The longest wait between repaints while the host keeps refusing them.
///
/// The ceiling keeps a host that stays broken from being rewritten on every loop turn while
/// still showing the presentation soon after it recovers.
pub(crate) const REFUSED_OUTPUT_RETRY_MAX: Duration = Duration::from_secs(2);

// The clipboard helper.

/// Maximum time Ctrl+V waits for the clipboard helper in a modal input.
///
/// The timeout keeps a stalled clipboard owner from freezing modal input.
pub(crate) const MODAL_PASTE_CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(500);
/// Channel capacity for the asynchronous clipboard helper.
///
/// The channel suffices because every read has its receiver and completes once.
pub(crate) const CLIPBOARD_RESULT_QUEUE_CAPACITY: usize = 1;

// The client loop and shutdown.

/// Event queue capacity shared by host input, resize, endpoint readers, supervisors and quit.
///
/// The capacity absorbs short bursts without allowing unlimited event accumulation.
pub(crate) const CLIENT_EVENT_QUEUE_CAPACITY: usize = 256;
/// Bound runtime shutdown so terminal restoration and process exit are not held by idle tasks.
///
/// A brief drain window gives cooperative tasks time to finish without stalling exit.
pub(crate) const CLIENT_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
/// Bound SSH helper cleanup while the client is exiting.
///
/// The timeout allows ordinary helper teardown but keeps exit bounded.
pub(crate) const SSH_RESOURCE_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);

// Endpoint writes: frame and flush deadlines and the writer's queue.

/// How long one endpoint frame write, or an input flush, may block.
///
/// The timeout absorbs short socket stalls and fails a wedged endpoint promptly.
pub(crate) const ENDPOINT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll spacing while an endpoint writer waits for socket progress.
///
/// The interval keeps stalled writes responsive without a tight polling loop.
pub(crate) const ENDPOINT_IO_POLL_INTERVAL: Duration = Duration::from_millis(2);
/// Deadline for the best-effort Detach flush while the endpoint registry is dropping.
///
/// A brief flush gives the courtesy message a chance to leave before shutdown disconnects.
pub(crate) const ENDPOINT_DETACH_FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

const _: () = assert!(ENDPOINT_IO_POLL_INTERVAL.as_millis() < ENDPOINT_WRITE_TIMEOUT.as_millis());

/// Maximum queued frame batches waiting for the endpoint writer.
///
/// The queue absorbs short input bursts while limiting queued command objects.
pub(crate) const MAX_QUEUED_BATCHES: usize = 256;
/// Maximum bytes coalesced into one endpoint writer batch.
///
/// The cap bounds each write batch so a large burst does not monopolize the writer.
pub(crate) const MAX_BATCH_BYTES: usize = 64 * 1024;
/// Maximum endpoint writer backlog, leaving room for frames already in flight
/// while bounding queued memory.
pub(crate) const MAX_QUEUED_BYTES: usize = 2 * shepr_protocol::MAX_FRAME_SIZE;

// The endpoint handshake, requests and machine moves.

/// Time to wait for the server's complete Welcome reply during the handshake.
/// This is an overall deadline for the frame, not a per-read idle timeout.
///
/// A local client talks to an already-connected server, so this deadline only
/// needs room for the welcome response. A configured machine's endpoint also
/// waits on a fresh SSH connection, including key exchange and authentication,
/// which needs more room on high-latency links (`REMOTE_HANDSHAKE_READ_TIMEOUT`).
pub(crate) const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Allows a fresh remote SSH connection and its welcome reply to finish on
/// high-latency links.
pub(crate) const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for a client request sent to an endpoint.
///
/// The deadline allows slow remote reads while preventing a request from waiting forever.
pub(crate) const ENDPOINT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a move may stay Preparing before it fails and the shown endpoint stays.
///
/// A move prepares only a target that is already connected and has a snapshot, so no SSH
/// connect falls inside this wait: it covers round trips on a live connection (the surface
/// activation acknowledgement, the focus answer, and a coherent snapshot and surface pair). A
/// target that has not answered by then has stalled, and the move gives up on it instead of
/// staying pending.
pub(crate) const ENDPOINT_MOVE_TIMEOUT: Duration = Duration::from_secs(5);

// Connection health and reconnect backoff.

/// Endpoint heartbeat interval, the connection-health cadence the remote
/// host's SSH bridge expiry is checked against.
pub(crate) const HEARTBEAT_INTERVAL: Duration = shepr_launch::connection_health::HEARTBEAT_INTERVAL;
/// Expire an endpoint after this much transport silence, measured when the reader receives a
/// complete frame rather than when the client loop processes it.
///
/// The timeout allows ordinary network delay before marking a machine endpoint offline.
pub(crate) const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

const _: () = assert!(HEARTBEAT_INTERVAL.as_millis() < HEARTBEAT_TIMEOUT.as_millis());

/// Initial reconnect delay before exponential backoff.
///
/// The delay retries quickly after a transient local or SSH failure.
pub(crate) const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
/// Every endpoint, Local or configured machine, retries at least this often, so an open
/// client picks a machine up promptly once it is reachable again; a longer backoff would
/// leave it offline long after it came back.
///
/// An attempt's own failure schedules the next one from when that attempt started, not
/// from when it gave up, and no attempt runs longer than `ATTEMPT_BUDGET`. Together they
/// keep the promise with an attempt already in flight: from any moment, the next attempt
/// starts once the current one ends or its retry delay (counted from its start) is up,
/// whichever is later, within the retry bound.
pub(crate) const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// A connection stable for this interval resets its accumulated retry state.
///
/// The interval distinguishes a durable connection from a brief success between failures.
pub(crate) const STABLE_CONNECTION_PERIOD: Duration = Duration::from_secs(60);
/// Same bound as `MAX_RETRY_DELAY`, for the same prompt-retry guarantee.
pub(crate) const ATTENTION_RETRY_DELAY: Duration = MAX_RETRY_DELAY;
/// The longest one connection attempt may run: the SSH discovery commands, the bridge and
/// the endpoint handshake all stop at this deadline. Without it an attempt against a host
/// that stalls could hold the endpoint indefinitely (each discovery command may
/// take one SSH command timeout, the handshake
/// `REMOTE_HANDSHAKE_READ_TIMEOUT`), and the next attempt waited for it, which broke the
/// retry bound. `do_handshake_for_endpoint` takes this deadline and stops at whichever
/// of it and the handshake timeout comes first.
///
/// A healthy attempt needs far less: every discovery command already had
/// to fit a cold SSH connect into one SSH command timeout. It stays below
/// `MAX_RETRY_DELAY` to leave room for tearing a timed-out bridge down.
///
/// The budget is the same for every attempt, including one that has to run full
/// discovery of the remote executable. With a valid disk hint, the first connection
/// uses two SSH round trips: one to verify the installed client and sibling server,
/// then one to start the bridge and carry the handshake. The handshake checks the
/// running server's identity, so the connector does not issue a separate server-status
/// query. Once the executable is verified, an ordinary reconnect uses only the bridge
/// round trip and handshake.
/// The case that can overrun is a cache miss or a stale remembered path on a slow link
/// without connection sharing, where each of discovery's several round trips and the
/// bridge each need their own cold connect.
/// That case is handled by resuming, not by a larger budget: the machine connector
/// keeps completed discovery steps when an attempt ends on a transient network failure
/// or a full-round-trip timeout that may be waiting for authentication. SSH process
/// failures and remote command errors clear that progress. It also keeps a freshly
/// discovered executable when only the bridge ran out of time. No discovery round trip
/// may take longer than one SSH command timeout, and the budget, which
/// `shepr_remote` defines as that timeout plus a fixed slack, exceeds it, so every
/// attempt that starts with discovery completes at least one, and discovery
/// finishes after a bounded number of attempts;
/// after that the bridge and the handshake need to fit one attempt, as on every
/// ordinary reconnect. A larger discovery budget would stretch the retry bound exactly where
/// the link is slowest, and would still fail on an even slower link.
pub(crate) const ATTEMPT_BUDGET: Duration = shepr_remote::SSH_CONNECTION_ATTEMPT_BUDGET;

// An attempt, and so the retry that follows it, must fit the retry bound.
const _: () = assert!(ATTEMPT_BUDGET.as_millis() < MAX_RETRY_DELAY.as_millis());

/// The longest an operator's Restart of a configured machine may run: the conditional
/// stop of its server of another build, then an attempt that starts this build's
/// server. Only the operator starts one, so it is outside the automatic retry bound.
pub(crate) const RESTART_ATTEMPT_BUDGET: Duration = shepr_remote::SSH_RESTART_ATTEMPT_BUDGET;

// Sidebar geometry.

/// Rows the workspace section header occupies above the workspace entries: the title and a
/// blank row. A workspace drop marker may sit on the blank row, above the first workspace.
pub(crate) const WORKSPACE_HEADER_ROWS: u16 = 2;
/// Rows the workspace section footer (the new-workspace and launcher row) occupies below the
/// workspace entries. The expanded sidebar keeps the row out of the entry list whether or not
/// the footer is drawn.
pub(crate) const WORKSPACE_FOOTER_ROWS: u16 = 1;
/// Columns of the footer's menu launcher click target, at the right edge of the workspace
/// section. It covers the four-column `menu` label and the two columns before it.
pub(crate) const GLOBAL_LAUNCHER_HIT_WIDTH: u16 = 6;
/// Rows the expanded sidebar's agent section header occupies above the agent entries: the
/// section divider, the title and sort toggle, and a blank row.
pub(crate) const AGENT_PANEL_HEADER_ROWS: u16 = 3;
/// Minimum height retained by each section of the expanded sidebar. The sections split only
/// when the sidebar is tall enough for both minimums.
pub(crate) const MIN_EXPANDED_SIDEBAR_SECTION_ROWS: u16 = 3;
/// Below this height the collapsed sidebar shows workspaces only, with no divider or agent
/// rows. At it, half the height (rounded up) holds workspaces and the divider still leaves two
/// agent rows.
pub(crate) const MIN_COLLAPSED_SIDEBAR_SPLIT_ROWS: u16 = 7;

// Overlay popups and menus.

/// Minimum context-menu width, before the screen width is applied.
pub(crate) const MIN_CONTEXT_MENU_WIDTH: u16 = 14;
/// Screen columns a centred overlay popup leaves free, split evenly either side, so the
/// terminal it covers stays visible at its edges.
pub(crate) const OVERLAY_POPUP_HORIZONTAL_MARGIN: u16 = 4;
/// Screen rows a centred overlay popup leaves free, split evenly above and below.
pub(crate) const OVERLAY_POPUP_VERTICAL_MARGIN: u16 = 2;
/// Below this width, after the margins, no overlay popup is drawn at all.
pub(crate) const MIN_OVERLAY_POPUP_WIDTH: u16 = 4;
/// Below this height, after the margins, no overlay popup is drawn at all.
pub(crate) const MIN_OVERLAY_POPUP_HEIGHT: u16 = 4;
/// Rows, or navigator entries, one mouse wheel notch moves in the Help and navigator overlays.
pub(crate) const OVERLAY_WHEEL_SCROLL_ROWS: isize = 3;
/// Maximum width of the client navigator overlay, so it leaves terminal
/// context visible on a wide screen.
pub(crate) const MAX_NAVIGATOR_OVERLAY_WIDTH: u16 = 116;
/// Maximum height of the client navigator overlay.
pub(crate) const MAX_NAVIGATOR_OVERLAY_HEIGHT: u16 = 42;
/// Below this height the navigator overlay is not drawn at all.
pub(crate) const MIN_NAVIGATOR_OVERLAY_HEIGHT: u16 = 9;
/// Maximum width of the Help overlay; a narrower screen shrinks it and the bindings wrap.
pub(crate) const MAX_HELP_OVERLAY_WIDTH: u16 = 76;
/// Maximum height of the Help overlay; the bindings scroll inside it.
pub(crate) const MAX_HELP_OVERLAY_HEIGHT: u16 = 22;
/// Columns a configured machine's state entry is indented under its machine row, as far
/// as the machine's workspaces are, so it reads as nested like them.
pub(crate) const MACHINE_ENTRY_INDENT: u16 = 2;
/// Width of a machine's Restart question: room for its longest line inside the border.
pub(crate) const CONFIRM_RESTART_WIDTH: u16 = 72;
/// Height of a machine's Restart question: its title, four lines, a blank row and the
/// buttons, inside the border.
pub(crate) const CONFIRM_RESTART_HEIGHT: u16 = 9;
/// Below this width inside its border the Help overlay is not drawn at all; the 13-column
/// close button sits at the right of the title row.
pub(crate) const MIN_HELP_OVERLAY_INNER_WIDTH: u16 = 20;
/// Below this height inside its border the Help overlay is not drawn at all: three header rows
/// and two footer rows leave one row of bindings.
pub(crate) const MIN_HELP_OVERLAY_INNER_HEIGHT: u16 = 6;

// Notice and machine diagnostic text.

/// Maximum sanitized machine diagnostic text shown in the sidebar, in
/// characters.
pub(crate) const MAX_MACHINE_DIAGNOSTIC_CHARS: usize = 4096;
/// Body rows an automatic notice card shows. A multi-line ssh error must not
/// cover the UI unasked; the machine badge opens the full diagnostic.
pub(crate) const MAX_AUTOMATIC_NOTICE_BODY_ROWS: usize = 3;
/// Rows between the top of the pane area (below any banner or placeholder line there) and a
/// notice card, so the card does not sit flush in the corner.
pub(crate) const NOTICE_CARD_TOP_MARGIN: u16 = 1;
/// Columns between a notice card and the right edge of the pane area, for the same reason.
/// A card is at most the pane area's width less this margin.
pub(crate) const NOTICE_CARD_RIGHT_MARGIN: u16 = 2;

// Copy mode.

/// Keys held for copy mode while one of its requests is in flight.
///
/// Copy-mode keys wait for the request ahead of them so motions stay in order; past this many,
/// later keys are dropped with a notice instead of growing the queue without bound.
pub(crate) const MAX_COPY_INPUT_QUEUE: usize = 256;
