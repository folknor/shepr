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
    pub(super) stripped_modifiers: crossterm::event::KeyModifiers,
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

/// Every gesture the pointer has in flight, and the host's pixel mouse geometry.
#[derive(Default)]
pub(in crate::shell) struct Pointer {
    pub(in crate::shell) chrome_drag: Option<ClientChromeDrag>,
    pub(super) workspace_press: Option<ClientWorkspacePress>,
    pub(in crate::shell) pane_mouse_gesture: Option<ClientPaneMouseGesture>,
    pub(super) last_sidebar_divider_click: Option<Instant>,
    pub(in crate::shell) host_mouse_pixels: Option<shepr_termio::input::mouse::HostPixels>,
}

impl Pointer {
    /// Drops what an endpoint projection described. The divider click history is the
    /// pointer's own, so it survives.
    pub(in crate::shell) fn reset_for_projection(&mut self) {
        self.chrome_drag = None;
        self.workspace_press = None;
        self.pane_mouse_gesture = None;
        self.host_mouse_pixels = None;
    }
}
