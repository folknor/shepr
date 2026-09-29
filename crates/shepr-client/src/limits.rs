//! Client timing, capacity, and presentation limits.

use std::time::{Duration, Instant};

/// One monotonic deadline, with all arithmetic and remaining-time conversions in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Deadline(Instant);

impl Deadline {
    pub(super) const fn at(instant: Instant) -> Self {
        Self(instant)
    }

    pub(super) fn after(now: Instant, duration: Duration) -> Self {
        Self(now + duration)
    }

    pub(super) const fn instant(self) -> Instant {
        self.0
    }

    pub(super) fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    pub(super) fn remaining(self, now: Instant) -> Option<Duration> {
        self.0.checked_duration_since(now)
    }

    pub(super) fn remaining_millis_i32(self, now: Instant) -> Option<i32> {
        self.remaining(now).map(|remaining| {
            i32::try_from(remaining.as_millis())
                .unwrap_or(i32::MAX)
                .max(1)
        })
    }

    pub(super) fn is_expired(self, now: Instant) -> bool {
        now >= self.0
    }
}

/// Repeated clicks on the same spot within the gesture interval are a double click.
///
/// This keeps the gesture in the usual short desktop double-click window.
pub(super) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(350);
/// Minimum spacing between requests sent by scrollbar and split drags.
///
/// This caps updates near the usual desktop frame cadence.
pub(super) const MOUSE_DRAG_SEND_INTERVAL: Duration = Duration::from_millis(33);
/// Tick spacing for scrolling a selection while the pointer is outside the pane.
///
/// This keeps edge scrolling responsive without scheduling at every input event.
pub(super) const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(30);
/// Minimum spacing of the frames a selection drag rebuilds.
///
/// This bounds redraw work to a practical frame cadence.
pub(super) const SELECTION_REPAINT_INTERVAL: Duration = Duration::from_millis(16);
/// The longest the client loop sleeps when no shell timer is due sooner.
///
/// The short delay bounds input and resize latency when no timer is armed.
pub(super) const MAX_CLIENT_TIMER_DELAY: Duration = Duration::from_millis(100);
/// Poll spacing for terminal size changes that do not arrive through a signal.
///
/// The interval keeps polling responsive while avoiding a busy loop.
pub(super) const TERMINAL_RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Bound runtime shutdown so terminal restoration and process exit are not held by idle tasks.
///
/// A brief drain window gives cooperative tasks time to finish without stalling exit.
pub(super) const CLIENT_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
/// Bound SSH helper cleanup while the client is exiting.
///
/// The timeout allows ordinary helper teardown but keeps exit bounded.
pub(super) const SSH_RESOURCE_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
/// Time an endpoint error stays visible without another input event.
///
/// The timeout leaves time to read a transient error before it clears.
pub(super) const ENDPOINT_ERROR_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an endpoint notice card stays up before it hides itself. A click on the card hides
/// it sooner; the timeout is what dismisses it when `ui.mouse_capture` is off.
/// The timeout keeps a notice available through a short recovery without leaving stale cards up.
pub(super) const ENDPOINT_NOTICE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one endpoint frame write, or an input flush, may block.
///
/// The timeout absorbs short socket stalls and fails a wedged endpoint promptly.
pub(super) const ENDPOINT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll spacing while an endpoint writer waits for socket progress.
///
/// The interval keeps stalled writes responsive without a tight polling loop.
pub(super) const ENDPOINT_IO_POLL_INTERVAL: Duration = Duration::from_millis(2);
/// Deadline for the best-effort Detach flush while the endpoint registry is dropping.
///
/// A brief flush gives the courtesy message a chance to leave before shutdown disconnects.
pub(super) const ENDPOINT_DETACH_FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

/// Bound the keyboard-capability query's wait for a host terminal response.
///
/// A short wait covers normal terminal replies while keeping startup interactive.
pub(super) const HOST_KEYBOARD_QUERY_TIMEOUT: Duration = Duration::from_millis(250);
/// Maximum host input buffered while the keyboard-capability query is pending.
///
/// The capacity holds terminal replies while bounding input from an unresponsive host.
pub(super) const MAX_BUFFERED_HOST_INPUT: usize = 64 * 1024;
/// Scratch buffer size for each read from the outer terminal.
///
/// The chunk keeps blocking reads page-sized and bounds each temporary read buffer.
pub(super) const HOST_INPUT_READ_CHUNK_BYTES: usize = 4096;

/// Time to wait for the server's complete Welcome reply during the handshake.
/// This is an overall deadline for the frame, not a per-read idle timeout.
///
/// A local client talks to an already-connected server, so this deadline only
/// needs room for the welcome response. A configured machine's endpoint shell that is
/// not the active surface also waits on a fresh SSH connection, including key
/// exchange and authentication, which needs more room on high-latency links.
pub(super) const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Allows a fresh remote SSH connection and its welcome reply to finish on
/// high-latency links.
pub(super) const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for a client request sent to an endpoint.
///
/// The deadline allows slow remote reads while preventing a request from waiting forever.
pub(super) const ENDPOINT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum retired request IDs retained per endpoint to ignore late responses.
///
/// The window tolerates a burst of cancelled requests without unbounded per-endpoint growth.
pub(super) const MAX_RETIRED_REQUESTS_PER_ENDPOINT: usize = 128;

/// Maximum time an endpoint surface activation may remain pending.
///
/// The timeout allows a slow endpoint to acknowledge activation without leaving input blocked.
pub(super) const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);
/// Endpoint heartbeat interval shared with the server's core timing policy.
pub(super) const HEARTBEAT_INTERVAL: Duration = shepr_core::limits::HEARTBEAT_INTERVAL;
/// Expire an endpoint that has not returned a heartbeat within this interval.
///
/// The timeout permits missed scheduling and transport jitter before marking the endpoint offline.
pub(super) const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

/// Initial reconnect delay before exponential backoff.
///
/// The delay retries quickly after a transient local or SSH failure.
pub(super) const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
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
pub(super) const STABLE_CONNECTION_PERIOD: Duration = Duration::from_secs(60);
/// Same bound as `MAX_RETRY_DELAY`, for the same prompt-retry guarantee.
pub(super) const ATTENTION_RETRY_DELAY: Duration = MAX_RETRY_DELAY;
/// What `ATTEMPT_BUDGET` allows beyond one cold SSH round trip.
pub(super) const SSH_ATTEMPT_SLACK: Duration = Duration::from_secs(10);
/// The longest one connection attempt may run: the SSH discovery commands, the bridge and
/// the endpoint handshake all stop at this deadline. Without it an attempt against a host
/// that stalls could hold the endpoint indefinitely (each discovery command may
/// take `shepr_core::limits::SSH_ROUND_TRIP_TIMEOUT`, the handshake
/// `REMOTE_HANDSHAKE_READ_TIMEOUT`), and the next attempt waited for it, which broke the
/// retry bound. `do_handshake` takes this deadline and stops at whichever
/// of it and the handshake timeout comes first.
///
/// A healthy attempt needs far less: every discovery command already had
/// to fit a cold SSH connect into `SSH_ROUND_TRIP_TIMEOUT`. It stays below
/// `MAX_RETRY_DELAY` to leave room for tearing a timed-out bridge down.
///
/// The budget is the same for every attempt, including one that has to run full
/// discovery of the remote executable. Most attempts do not: a reconnect launches the
/// bridge from the remembered executable.
/// With the default managed ssh config every command after the first reuses one shared
/// connection (ControlMaster), so only one cold connect is paid.
/// The case that can overrun is a cache miss or a stale remembered path on a slow link
/// without connection sharing, where each of discovery's several round trips, a status
/// probe per candidate, and the bridge each need their own cold connect.
/// That case is handled by resuming, not by a larger budget: the machine connector
/// keeps what discovery completed when an attempt ends on a timeout or other link
/// failure (any other error clears it) and the next attempt continues from there, and it
/// keeps a freshly discovered executable when only the bridge ran out of time. No
/// discovery round trip may take longer than `SSH_ROUND_TRIP_TIMEOUT`, and the budget
/// exceeds it by `SSH_ATTEMPT_SLACK`, so every attempt that starts with discovery
/// completes at least one, and discovery finishes after a bounded number of attempts;
/// after that the bridge and handshake need to fit one attempt, as on every ordinary
/// reconnect. A larger discovery budget would stretch the retry bound exactly where
/// the link is slowest, and would still fail on an even slower link.
pub(super) const ATTEMPT_BUDGET: Duration =
    shepr_core::limits::SSH_ROUND_TRIP_TIMEOUT.saturating_add(SSH_ATTEMPT_SLACK);

/// Maximum queued frame batches waiting for the endpoint writer.
///
/// The queue absorbs short input bursts while limiting queued command objects.
pub(super) const MAX_QUEUED_BATCHES: usize = 256;
/// Maximum bytes coalesced into one endpoint writer batch.
///
/// The cap bounds each write batch so a large burst does not monopolize the writer.
pub(super) const MAX_BATCH_BYTES: usize = 64 * 1024;
/// Maximum endpoint writer backlog, leaving room for frames already in flight
/// while bounding queued memory.
pub(super) const MAX_QUEUED_BYTES: usize = 2 * shepr_protocol::MAX_FRAME_SIZE;

/// Maximum time Ctrl+V waits for the clipboard helper in a modal input.
///
/// The timeout keeps a stalled clipboard owner from freezing modal input.
pub(super) const MODAL_PASTE_CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(500);
/// Keep a completed word-selection highlight visible for this interval.
///
/// The timeout leaves brief visual feedback after the selection copy completes.
pub(super) const WORD_SELECTION_HIGHLIGHT_TIMEOUT: Duration = Duration::from_millis(500);
/// Keep workspace navigation feedback visible while its focus request is pending.
///
/// The timeout covers the normal focus round trip without leaving stale feedback on screen.
pub(super) const WORKSPACE_HIGHLIGHT_TIMEOUT: Duration = Duration::from_secs(1);

/// Fallback terminal cell width when the host does not report pixel geometry.
///
/// The conventional fallback width maps cell coordinates when the host omits pixel geometry.
pub(super) const DEFAULT_CELL_WIDTH_PX: u32 = 8;
/// Fallback terminal cell height when the host does not report pixel geometry.
///
/// The conventional fallback height maps cell coordinates when the host omits pixel geometry.
pub(super) const DEFAULT_CELL_HEIGHT_PX: u32 = 16;
/// Smallest tab width that still leaves room for its label.
pub(super) const MIN_TAB_WIDTH: u16 = 8;
/// Columns the new-tab button occupies in the tab strip.
pub(super) const NEW_TAB_WIDTH: u16 = 3;
/// Rows the workspace section header occupies above the workspace entries.
pub(super) const WORKSPACE_HEADER_ROWS: u16 = 2;
/// Columns each tab-strip scroll button occupies.
pub(super) const TAB_SCROLL_BUTTON_WIDTH: u16 = 3;
/// Minimum tab-strip width with a new-tab button and both scroll buttons.
pub(super) const MIN_TAB_STRIP_WIDTH: u16 =
    MIN_TAB_WIDTH + NEW_TAB_WIDTH + TAB_SCROLL_BUTTON_WIDTH.saturating_mul(2);

/// Minimum height retained by each section of the expanded sidebar.
pub(super) const MIN_EXPANDED_SIDEBAR_SECTION_ROWS: u16 = 3;
/// Minimum context-menu width, before the screen width is applied.
pub(super) const MIN_CONTEXT_MENU_WIDTH: u16 = 14;
/// Maximum width of the client navigator overlay, so it leaves terminal
/// context visible on a wide screen.
pub(super) const MAX_NAVIGATOR_OVERLAY_WIDTH: u16 = 116;
/// Maximum height of the client navigator overlay.
pub(super) const MAX_NAVIGATOR_OVERLAY_HEIGHT: u16 = 42;
/// Below this width the navigator overlay is not drawn at all.
pub(super) const MIN_NAVIGATOR_OVERLAY_WIDTH: u16 = 4;
/// Below this height the navigator overlay is not drawn at all.
pub(super) const MIN_NAVIGATOR_OVERLAY_HEIGHT: u16 = 9;

/// Maximum sanitized machine diagnostic text shown in the sidebar, in
/// characters.
pub(super) const MAX_MACHINE_DIAGNOSTIC_CHARS: usize = 4096;
/// Maximum lines scrolled for each pointer row beyond a selection edge.
///
/// Scaling lines with pointer distance makes edge scrolling accelerate smoothly.
pub(super) const SELECTION_EDGE_SCROLL_LINES_PER_ROW: usize = 3;
/// Minimum lines moved on an edge-scroll tick.
///
/// This keeps the first edge-scroll step visible.
pub(super) const MIN_SELECTION_EDGE_SCROLL_LINES: usize = 3;
/// Maximum lines moved on an edge-scroll tick.
///
/// The cap prevents a small pointer movement from skipping too far.
pub(super) const MAX_SELECTION_EDGE_SCROLL_LINES: usize = 15;

/// Event queue capacity shared by the resize and server-reader threads.
///
/// The capacity absorbs short bursts without allowing unlimited event accumulation.
pub(super) const CLIENT_EVENT_QUEUE_CAPACITY: usize = 256;
/// Event queue capacity for endpoint supervisor notifications.
///
/// The capacity covers endpoint status bursts while keeping the queue bounded.
pub(super) const ENDPOINT_SUPERVISOR_EVENT_QUEUE_CAPACITY: usize = 64;
/// Channel capacity for the asynchronous clipboard helper.
///
/// The channel suffices because every read has its receiver and completes once.
pub(super) const CLIPBOARD_RESULT_QUEUE_CAPACITY: usize = 1;

const _: () = assert!(ENDPOINT_IO_POLL_INTERVAL.as_millis() < ENDPOINT_WRITE_TIMEOUT.as_millis());

#[cfg(test)]
mod tests {
    use super::Deadline;
    use std::time::{Duration, Instant};

    #[test]
    fn deadline_remaining_and_min_use_the_supplied_time() {
        let start = Instant::now();
        let deadline = Deadline::after(start, Duration::from_millis(10));
        assert_eq!(
            deadline.remaining(start + Duration::from_millis(3)),
            Some(Duration::from_millis(7))
        );
        assert_eq!(
            deadline.remaining_millis_i32(start + Duration::from_millis(3)),
            Some(7)
        );

        let earlier = Deadline::at(start + Duration::from_millis(5));
        let combined = deadline.min(earlier);
        assert_eq!(combined.instant(), earlier.instant());
        assert_eq!(
            combined.remaining(start + Duration::from_millis(5)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            combined.remaining_millis_i32(start + Duration::from_millis(5)),
            Some(1)
        );
        assert!(combined.is_expired(start + Duration::from_millis(5)));
    }
}
