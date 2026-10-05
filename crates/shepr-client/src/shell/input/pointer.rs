//! Pointer gestures in flight: chrome drags, workspace presses, pane mouse gestures and the
//! host's pixel mouse reporting.

use crate::shell::navigation::location::Location;
use crate::shell::view::{PaneHit, PaneSplitHit};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(in crate::shell) struct Throttle {
    interval: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub(super) fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    pub(super) fn admit(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval)
        {
            self.last = Some(now);
            true
        } else {
            false
        }
    }
}

pub(in crate::shell) struct ClientPaneMouseGesture {
    pub(super) hit: PaneHit,
    pub(super) button: crossterm::event::MouseButton,
    pub(super) last_event: crossterm::event::MouseEvent,
    pub(super) last_position: shepr_protocol::ClientMousePosition,
}

pub(in crate::shell) struct ClientWorkspacePress {
    pub(in crate::shell) location: Location,
    pub(super) start_column: u16,
    pub(super) start_row: u16,
}

pub(in crate::shell) enum ClientChromeDrag {
    /// Dragging the sidebar edge. The width follows the pointer, but the endpoint is resized
    /// once, on release: each resize reflows every PTY, and one per column crossed would make
    /// every pane redraw repeatedly mid-drag.
    SidebarWidth {
        resize_pending: bool,
    },
    SidebarSection,
    WorkspaceScrollbar {
        grab_row_offset: u16,
    },
    AgentScrollbar {
        grab_row_offset: u16,
    },
    Workspace {
        source_workspace_id: shepr_protocol::WorkspaceId,
        target: Option<(Option<shepr_protocol::WorkspaceId>, u16)>,
    },
    PaneSplit {
        hit: PaneSplitHit,
        workspace_id: shepr_protocol::WorkspaceId,
        grab_offset: i32,
        last_sent_ratio: Option<shepr_core::layout::SplitRatio>,
        throttle: Throttle,
    },
    PaneScrollbar {
        hit: PaneHit,
        grab_row_offset: u16,
        last_sent_offset: Option<usize>,
        throttle: Throttle,
    },
}

/// What a sidebar drag ended by a projection reset still owes: the preference save, and
/// for a width drag that moved the edge, the endpoint resize. The shell settles it on its
/// next timer pass (`ClientShellState::tick_timers`), which has an outcome to carry both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) struct OwedSidebarSettle {
    pub(in crate::shell) resize: bool,
}

/// Every gesture the pointer has in flight, and the host's pixel mouse geometry.
#[derive(Default)]
pub(in crate::shell) struct Pointer {
    pub(in crate::shell) chrome_drag: Option<ClientChromeDrag>,
    pub(super) workspace_press: Option<ClientWorkspacePress>,
    pub(in crate::shell) pane_mouse_gesture: Option<ClientPaneMouseGesture>,
    pub(super) last_sidebar_divider_click: Option<Instant>,
    pub(in crate::shell) host_mouse_pixels: Option<shepr_termio::input::mouse::HostPixels>,
    /// A sidebar drag's owed work, kept when a projection reset ended the drag.
    pub(in crate::shell) owed_sidebar_settle: Option<OwedSidebarSettle>,
}

impl Pointer {
    /// Drops what an endpoint projection described. The divider click history is the
    /// pointer's own, so it survives. A sidebar drag is the client's own chrome: the
    /// gesture ends, but what it owes (the width drag's endpoint resize, both drags'
    /// preference save) is kept in `owed_sidebar_settle` instead of being dropped.
    pub(in crate::shell) fn reset_for_projection(&mut self) {
        let owed = match self.chrome_drag.take() {
            Some(ClientChromeDrag::SidebarWidth { resize_pending }) => Some(resize_pending),
            Some(ClientChromeDrag::SidebarSection) => Some(false),
            _ => None,
        };
        if let Some(resize) = owed {
            let earlier = self.owed_sidebar_settle.is_some_and(|owed| owed.resize);
            self.owed_sidebar_settle = Some(OwedSidebarSettle {
                resize: resize || earlier,
            });
        }
        self.workspace_press = None;
        self.pane_mouse_gesture = None;
        self.host_mouse_pixels = None;
    }
}
