//! Client timing limits, named once so the event loop, shell input and the
//! endpoint writer agree on them.

use std::time::Duration;

/// Two clicks on the same spot within this window are a double click.
pub(super) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(350);
/// Minimum spacing of the requests a scrollbar or split drag sends.
pub(super) const MOUSE_DRAG_SEND_INTERVAL: Duration = Duration::from_millis(33);
pub(super) const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(30);
/// Minimum spacing of the frames a selection drag rebuilds.
pub(super) const SELECTION_REPAINT_INTERVAL: Duration = Duration::from_millis(16);
/// The longest the client loop sleeps when no shell timer is due sooner.
pub(super) const MAX_CLIENT_TIMER_DELAY: Duration = Duration::from_millis(100);
pub(super) const CLIENT_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
pub(super) const SSH_RESOURCE_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
pub(super) const ENDPOINT_ERROR_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an endpoint notice card stays up before it hides itself. A click on the card hides
/// it sooner; the timeout is what dismisses it when `ui.mouse_capture` is off.
pub(super) const ENDPOINT_NOTICE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one endpoint frame write, or an input flush, may block.
pub(super) const ENDPOINT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
