//! The mouse selection lifecycle and how snapshots and presented surfaces react to it.

use crate::shell::input::word_selection::ClientWordSelection;
use ratatui::layout::Rect;
use shepr_protocol::{ClientShellSnapshot, PaneSurfaceFrame};

/// Repeated clicks on the same spot within the gesture interval are a double click.
///
/// This keeps the gesture in the usual short desktop double-click window.
pub(in crate::shell) const DOUBLE_CLICK_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(350);

#[derive(Clone, Debug)]
pub(in crate::shell) struct ClientPaneClick {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) viewport_row: u16,
    pub(in crate::shell) col: u16,
    pub(in crate::shell) at: std::time::Instant,
}

impl ClientPaneClick {
    pub(in crate::shell) fn is_double_click_for(&self, next: &Self) -> bool {
        self.pane_id == next.pane_id
            && next.at.duration_since(self.at) <= DOUBLE_CLICK_WINDOW
            && self.viewport_row.abs_diff(next.viewport_row) <= 1
            && self.col.abs_diff(next.col) <= 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientSelectionAutoscrollDirection {
    Up,
    Down,
}

#[derive(Clone, Debug)]
pub(in crate::shell) struct ClientSelectionAutoscroll {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) direction: ClientSelectionAutoscrollDirection,
    pub(in crate::shell) last_mouse_column: u16,
    pub(in crate::shell) last_mouse_row: u16,
    pub(in crate::shell) inner_rect: Rect,
    pub(in crate::shell) offset_from_bottom: usize,
    pub(in crate::shell) max_offset_from_bottom: usize,
}

/// The selected pane as the previously presented surface showed it. The selection
/// invalidation needs only this, so no previous surface is cloned, and an in-place patch
/// can capture it before changing the surface.
pub(in crate::shell) enum PreviousPane {
    /// Nothing was presented: nothing shows the selection's coordinates still describe
    /// this pane's grid, and highlighting them could mark stale cells. Invalidate.
    NoSurface,
    /// No selection, or the pane is missing from a compared surface: keep it.
    Absent,
    /// Compare with the next surface.
    Present(PaneFacts),
}

pub(in crate::shell) struct PaneFacts {
    inner_width: u16,
    inner_height: u16,
    alternate_screen_active: bool,
    content_revision: shepr_protocol::ContentRevision,
}

/// The live mouse range, pending focus, gestures and timers form one lifecycle.
/// Copy-mode anchors stay in `CopySession` so focus return can rebuild the projected
/// range; click history can outlive a cleared range for double-clicks.
#[derive(Default)]
pub(in crate::shell) struct MouseSelection {
    pub(in crate::shell) selection:
        Option<shepr_term::selection::Selection<shepr_protocol::PublicPaneId>>,
    pub(in crate::shell) focus_pending: Option<shepr_protocol::PublicPaneId>,
    pub(in crate::shell) last_pane_click: Option<ClientPaneClick>,
    pub(in crate::shell) autoscroll: Option<ClientSelectionAutoscroll>,
    pub(in crate::shell) autoscroll_deadline: Option<std::time::Instant>,
    pub(in crate::shell) highlight_clear_deadline: Option<std::time::Instant>,
    pub(in crate::shell) repaint_deadline: Option<std::time::Instant>,
    pub(in crate::shell) word_gesture: Option<ClientWordSelection>,
}

impl MouseSelection {
    /// End the range interaction while preserving click history for double-click detection.
    pub(in crate::shell) fn clear_range(&mut self) {
        self.selection = None;
        self.focus_pending = None;
        self.autoscroll = None;
        self.autoscroll_deadline = None;
        self.highlight_clear_deadline = None;
        self.repaint_deadline = None;
        self.word_gesture = None;
    }

    pub(in crate::shell) fn clear(&mut self) {
        self.clear_range();
        self.last_pane_click = None;
    }

    pub(in crate::shell) fn stop_autoscroll(&mut self) {
        self.autoscroll = None;
        self.autoscroll_deadline = None;
    }

    /// The pane the gesture or selection is in; a word gesture takes precedence.
    fn pane_id(&self) -> Option<&shepr_protocol::PublicPaneId> {
        self.word_gesture
            .as_ref()
            .map(|gesture| &gesture.pane_id)
            .or_else(|| self.selection.as_ref().map(|selection| &selection.pane_id))
    }

    /// The selected or word-gesture pane as `previous` showed it.
    pub(in crate::shell) fn facts_in(&self, previous: Option<&PaneSurfaceFrame>) -> PreviousPane {
        let Some(pane_id) = self.pane_id() else {
            return PreviousPane::Absent;
        };
        let Some(surface) = previous else {
            return PreviousPane::NoSurface;
        };
        surface
            .panes
            .iter()
            .find(|p| &p.pane_id == pane_id)
            .map_or(PreviousPane::Absent, |p| {
                PreviousPane::Present(PaneFacts {
                    inner_width: p.inner_rect.width,
                    inner_height: p.inner_rect.height,
                    alternate_screen_active: p.alternate_screen_active,
                    content_revision: p.content_revision,
                })
            })
    }

    /// Word-gesture and selection focus-loss rules for a new snapshot: the range ends when
    /// its pane is gone or focus moved elsewhere.
    pub(in crate::shell) fn reconcile_snapshot(&mut self, snapshot: &ClientShellSnapshot) {
        let selection_focus_lost = if let Some(gesture) = self.word_gesture.as_mut() {
            let focused_pane = snapshot.focused_pane_id.as_ref();
            // Remember confirmed focus across intermediate snapshots with no
            // focused pane, without rejecting the gesture's in-flight focus request.
            gesture.focus_confirmed |= focused_pane == Some(&gesture.pane_id);
            !snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == gesture.pane_id)
                || (gesture.focus_confirmed
                    && focused_pane.is_some_and(|pane_id| pane_id != &gesture.pane_id))
        } else if let Some(selection) = self.selection.as_ref() {
            let focused_pane = snapshot.focused_pane_id.as_ref();
            let focused_here = focused_pane == Some(&selection.pane_id);
            // Like the word-gesture guard above: a selection started in an
            // unfocused pane survives snapshots that predate its focus
            // request, and only a focus change after that ends it.
            let awaiting_focus =
                !focused_here && self.focus_pending.as_ref() == Some(&selection.pane_id);
            if focused_here {
                self.focus_pending = None;
            }
            !snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == selection.pane_id)
                || (!focused_here && !awaiting_focus)
        } else {
            false
        };
        if selection_focus_lost {
            self.clear();
        }
    }

    /// A presented surface or patch replaced the one `before` describes: the range ends
    /// if its pane changed size or screen, or a word gesture's content moved.
    pub(in crate::shell) fn surface_presented(
        &mut self,
        before: PreviousPane,
        surface: &PaneSurfaceFrame,
    ) {
        let next = self
            .pane_id()
            .and_then(|id| surface.panes.iter().find(|p| &p.pane_id == id));
        let invalidated = match (before, next) {
            (PreviousPane::NoSurface, _) => true,
            (PreviousPane::Present(previous), Some(next)) => {
                previous.inner_width != next.inner_rect.width
                    || previous.inner_height != next.inner_rect.height
                    || previous.alternate_screen_active != next.alternate_screen_active
                    // Ordinary selections are live buffer ranges. Only word gestures
                    // cache content-dependent boundaries that output can invalidate.
                    || (self.word_gesture.is_some()
                        && previous.content_revision != next.content_revision)
            }
            _ => false,
        };
        if invalidated {
            self.clear_range();
        }
    }

    /// A frame was drawn: the repaint a selection timer owed is satisfied.
    pub(in crate::shell) fn frame_drawn(&mut self) {
        self.repaint_deadline = None;
    }
}
