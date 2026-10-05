//! Mouse routing for the client shell. Events here are raw host input bound
//! for panes; input content must stay out of logs and error messages here
//! (log content-free kinds instead).

use shepr_protocol::ClientPaneInputEvent;

use crate::shell::input::pointer::{
    ClientChromeDrag, ClientPaneMouseGesture, ClientWorkspacePress, Throttle,
};
use crate::shell::input::scroll_lanes::{ScrollAnswer, ScrollWant};
use crate::shell::input::selection::{
    ClientPaneClick, ClientSelectionAutoscroll, ClientSelectionAutoscrollDirection,
};
use crate::shell::ledger::{Submitted, Ticket, Work};
use crate::shell::state::ClientShellMode;
use crossterm::event::MouseButton;
use crossterm::event::MouseEventKind;

use crate::shell::state::{ClientShellEndpointError, ClientShellInput, ClientShellState, Repaint};
use crate::shell::view::{PaneHit, PaneSplitHit};

use shepr_protocol::ClientMousePosition;

use crate::shell::input::events::{PaneInputBatchAccounting, push_target_event};
use ratatui::layout::Rect;

use crate::limits::{
    MAX_SELECTION_EDGE_SCROLL_LINES, MIN_SELECTION_EDGE_SCROLL_LINES, MOUSE_DRAG_SEND_INTERVAL,
    MOUSE_WHEEL_SCROLL_LINES, SELECTION_AUTOSCROLL_INTERVAL, SELECTION_EDGE_SCROLL_LINES_PER_ROW,
    SELECTION_REPAINT_INTERVAL,
};
use crossterm::event::MouseEvent;
use std::time::Instant;

/// Whether the surface still publishes the split `hit` was read from: the same
/// path at the same layout epoch. A topology change advances the epoch, so a
/// path that now names another split is not found.
fn surface_has_split_of(surface: &shepr_protocol::PaneSurfaceFrame, hit: &PaneSplitHit) -> bool {
    surface
        .splits
        .iter()
        .any(|split| split.epoch == hit.epoch && split.path == hit.path)
}

/// The split a press at `point` grabs. Hit rects overlap where a border meets the borders
/// of the splits inside it (the junction of a top/bottom border with the left/right border
/// below it), and with gaps a hit rect is wider than the divider line. Of the splits whose
/// hit rect holds the point, the one whose divider line is nearest the pressed cell along
/// its own axis wins, and at an exact junction the outermost (shallowest) one, so a press on
/// a border's own line never grabs a border crossing it.
fn split_hit_at(splits: &[PaneSplitHit], point: (u16, u16)) -> Option<&PaneSplitHit> {
    splits
        .iter()
        .filter(|hit| crate::shell::input::hit_test::contains(hit.hit_rect, point))
        .min_by_key(|hit| {
            let off_line = match hit.direction {
                shepr_protocol::PaneSurfaceSplitDirection::Horizontal => point.0.abs_diff(hit.pos),
                shepr_protocol::PaneSurfaceSplitDirection::Vertical => point.1.abs_diff(hit.pos),
            };
            (off_line, hit.path.len())
        })
}

fn selection_cell(column: u16, row: u16, pane: Rect) -> (shepr_term::ViewportRow, u16) {
    let column = column.clamp(pane.x, pane.x.saturating_add(pane.width.saturating_sub(1)));
    let row = row.clamp(pane.y, pane.y.saturating_add(pane.height.saturating_sub(1)));
    // The clamps keep both inside the pane, so neither subtraction can fail.
    (
        shepr_term::ViewportRow::on_screen(row, pane.y).unwrap_or(shepr_term::ViewportRow(0)),
        column.saturating_sub(pane.x),
    )
}

impl ClientShellState {
    /// Moves the sidebar edge during a width drag, collapsing the sidebar below its minimum
    /// width (see `ChromeLayout::drag_edge_to`). The retained pane surface stays on screen,
    /// clipped to the new pane area; the endpoint resize waits for the release (see
    /// `ClientChromeDrag::SidebarWidth`).
    fn set_sidebar_width_from_column(&mut self, column: u16, outcome: &mut ClientShellInput) {
        if self.chrome.drag_edge_to(column.saturating_add(1)) {
            outcome.repaint = true;
            if let Some(ClientChromeDrag::SidebarWidth { resize_pending }) =
                self.pointer.chrome_drag.as_mut()
            {
                *resize_pending = true;
            } else {
                outcome.resize = true;
            }
        }
    }

    /// Finishes a sidebar drag as its release would: a width drag owes the endpoint its
    /// resize and both sidebar drags owe the preferences file the dragged value. Used for a
    /// real release and for a drag whose release was lost. Other drag kinds owe nothing here.
    fn settle_chrome_drag(&mut self, drag: &ClientChromeDrag, outcome: &mut ClientShellInput) {
        match drag {
            ClientChromeDrag::SidebarWidth { resize_pending } => {
                outcome.resize |= *resize_pending;
                self.persist_chrome_preferences(outcome);
            }
            ClientChromeDrag::SidebarSection => {
                self.persist_chrome_preferences(outcome);
            }
            _ => {}
        }
    }

    /// Does a sidebar drag's owed work (the width drag's resize, both drags'
    /// persistence) without ending the drag, for when its release may or may
    /// not still arrive. Other drag kinds are left alone: they finish on
    /// their release, or are abandoned at their last sent value by the next
    /// press.
    pub(super) fn settle_sidebar_drag_in_place(&mut self, outcome: &mut ClientShellInput) {
        let resize_owed = match self.pointer.chrome_drag.as_mut() {
            Some(ClientChromeDrag::SidebarWidth { resize_pending }) => {
                std::mem::take(resize_pending)
            }
            Some(ClientChromeDrag::SidebarSection) => false,
            _ => return,
        };
        outcome.resize |= resize_owed;
        self.persist_chrome_preferences(outcome);
    }

    fn set_sidebar_section_from_row(&mut self, row: u16, outcome: &mut ClientShellInput) {
        let divider = self.presentation.shown().sidebar_divider();
        if divider.height == 0 {
            return;
        }
        let ratio = row.saturating_sub(divider.y) as f32 / divider.height as f32;
        let ratio = crate::shell::sidebar::sidebar_tokens::SectionSplit::from_drag(ratio);
        if self.chrome.set_split(ratio) {
            outcome.repaint = true;
        }
    }

    fn pane_scrollbar_offset(
        hit: &PaneHit,
        row: u16,
        grab_row_offset: Option<u16>,
    ) -> Option<usize> {
        let track = hit.scrollbar_rect?;
        let metrics = hit.scroll?;
        (metrics.max_offset_from_bottom > 0).then(|| match grab_row_offset {
            Some(grab_row_offset) => shepr_term::scroll::scrollbar_offset_from_drag_row(
                metrics,
                crate::shell::view::list::scroll_track(track),
                row,
                grab_row_offset,
            ),
            None => shepr_term::scroll::scrollbar_offset_from_row(
                metrics,
                crate::shell::view::list::scroll_track(track),
                row,
            ),
        })
    }

    pub(in crate::shell) fn push_pane_scroll_offset(
        &mut self,
        pane_id: shepr_protocol::PublicPaneId,
        offset_from_bottom: usize,
        outcome: &mut ClientShellInput,
    ) {
        if matches!(
            self.scroll_lanes.want(&pane_id, offset_from_bottom),
            ScrollWant::Send
        ) {
            self.dispatch_pane_scroll(pane_id, offset_from_bottom, outcome);
        }
    }
    fn dispatch_pane_scroll(
        &mut self,
        pane_id: shepr_protocol::PublicPaneId,
        offset: usize,
        outcome: &mut ClientShellInput,
    ) {
        let flight = self.ledger.ticket();
        let submitted = self.submit(
            shepr_protocol::command::EndpointCommand::PaneScroll(
                shepr_protocol::command::PaneScrollParams {
                    pane_id,
                    offset_from_bottom: offset,
                },
            ),
            Work::PaneScroll { pane_id, flight },
            outcome,
        );
        if submitted == Submitted::Opened {
            self.scroll_lanes.sent(pane_id, flight, offset);
        } else {
            self.scroll_lanes.send_failed(&pane_id);
        }
    }
    pub(in crate::shell) fn answer_pane_scroll(
        &mut self,
        flight: Ticket,
        pane_id: &shepr_protocol::PublicPaneId,
        result: Result<shepr_protocol::command::PaneInfoReply, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) -> Repaint {
        match result {
            Ok(shepr_protocol::command::PaneInfoReply { pane }) if &pane.pane_id == pane_id => {
                if let ScrollAnswer::Next(Some(offset)) = self.scroll_lanes.answered(
                    pane_id,
                    flight,
                    pane.scroll.map(|s| s.offset_from_bottom),
                ) {
                    self.dispatch_pane_scroll(*pane_id, offset, outcome);
                }
                Repaint::Unchanged
            }
            Ok(_) | Err(_) => {
                if self.scroll_lanes.failed(pane_id, flight) {
                    Repaint::Needed
                } else {
                    Repaint::Unchanged
                }
            }
        }
    }

    pub(super) fn stop_selection_autoscroll(&mut self) {
        self.mouse_selection.stop_autoscroll();
    }

    fn selection_edge_scroll_lines(distance: u16) -> usize {
        usize::from(distance)
            .saturating_mul(SELECTION_EDGE_SCROLL_LINES_PER_ROW)
            .clamp(
                MIN_SELECTION_EDGE_SCROLL_LINES,
                MAX_SELECTION_EDGE_SCROLL_LINES,
            )
    }

    fn selection_scroll_metrics(&self, hit: &PaneHit) -> Option<shepr_term::ScrollMetrics> {
        let metrics = hit.scroll?;
        Some(
            self.mouse_selection
                .autoscroll
                .as_ref()
                .filter(|autoscroll| autoscroll.pane_id == hit.pane_id)
                .map_or(metrics, |autoscroll| {
                    shepr_term::ScrollMetrics::new(
                        autoscroll.offset_from_bottom,
                        autoscroll.max_offset_from_bottom,
                        metrics.viewport_rows,
                        metrics.history_origin,
                    )
                }),
        )
    }

    fn active_selection_pane(&self) -> Option<PaneHit> {
        let pane_id = if let Some(gesture) = self.mouse_selection.word_gesture.as_ref() {
            if gesture.released {
                return None;
            }
            &gesture.pane_id
        } else {
            self.mouse_selection
                .selection
                .as_ref()
                .filter(|selection| selection.is_in_progress())?
                .pane_id()
        };
        self.presentation
            .pane_hits()
            .iter()
            .find(|hit| &hit.pane_id == pane_id)
            .cloned()
    }

    fn update_selection_cursor_with_metrics(
        &mut self,
        hit: &PaneHit,
        column: u16,
        row: u16,
        metrics: Option<shepr_term::ScrollMetrics>,
        outcome: &mut ClientShellInput,
        now: Instant,
    ) {
        // Selections hold absolute rows. Without the pane's scroll origin a
        // viewport row cannot be mapped to one, so the selection is not moved.
        let Some(metrics) = metrics else {
            return;
        };
        let (viewport_row, col) = selection_cell(column, row, hit.inner_rect);
        let absolute_row = metrics.absolute_row_at_viewport(viewport_row);
        if self.mouse_selection.word_gesture.is_some() {
            self.drag_word_selection(shepr_term::Point::new(absolute_row, col), outcome, now);
        } else if let Some(selection) = self.mouse_selection.selection.as_mut() {
            selection.drag(shepr_term::Point::new(absolute_row, col));
        }
    }

    fn update_selection_drag(
        &mut self,
        hit: &PaneHit,
        column: u16,
        row: u16,
        now: Instant,
        outcome: &mut ClientShellInput,
    ) {
        let metrics = self.selection_scroll_metrics(hit);
        let was_dragging = self
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_term::selection::Selection::is_dragging);
        let moved_from_anchor = self
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| {
                let anchor = selection.anchor_position();
                let anchor_col = hit.inner_rect.x.saturating_add(anchor.col).clamp(
                    hit.inner_rect.x,
                    hit.inner_rect
                        .x
                        .saturating_add(hit.inner_rect.width.saturating_sub(1)),
                );
                let row_moved = metrics.map_or_else(
                    || {
                        selection.is_just_click()
                            && self
                                .mouse_selection
                                .last_pane_click
                                .as_ref()
                                .filter(|click| selection.belongs_to(&click.pane_id))
                                .is_some_and(|click| {
                                    hit.inner_rect.y.saturating_add(click.viewport_row) != row
                                })
                    },
                    |metrics| match anchor.row.viewport_row(metrics.viewport_top_row()) {
                        shepr_term::ViewportPosition::Above
                        | shepr_term::ViewportPosition::Below => true,
                        shepr_term::ViewportPosition::At(anchor_row) => {
                            anchor_row.0 >= hit.inner_rect.height
                                || hit.inner_rect.y.checked_add(anchor_row.0) != Some(row)
                        }
                    },
                );
                row_moved || anchor_col != column
            });
        self.update_selection_cursor_with_metrics(hit, column, row, metrics, outcome, now);
        let is_dragging = self
            .mouse_selection
            .word_gesture
            .as_ref()
            .map_or(was_dragging || moved_from_anchor, |gesture| gesture.dragged);
        if is_dragging {
            if let Some(selection) = self.mouse_selection.selection.as_mut()
                && selection.is_just_click()
            {
                selection.force_dragging();
            }
            self.mouse_selection.last_pane_click = None;
        }
        if !is_dragging {
            self.stop_selection_autoscroll();
            return;
        }

        let Some(metrics) = metrics else {
            self.stop_selection_autoscroll();
            return;
        };
        let top = hit.inner_rect.y;
        let bottom = hit
            .inner_rect
            .y
            .saturating_add(hit.inner_rect.height.saturating_sub(1));
        let (direction, immediate_lines) = if row < top {
            (
                ClientSelectionAutoscrollDirection::Up,
                Self::selection_edge_scroll_lines(top - row),
            )
        } else if row > bottom {
            (
                ClientSelectionAutoscrollDirection::Down,
                Self::selection_edge_scroll_lines(row - bottom),
            )
        } else if row == top {
            (ClientSelectionAutoscrollDirection::Up, 0)
        } else if row == bottom {
            (ClientSelectionAutoscrollDirection::Down, 0)
        } else {
            self.stop_selection_autoscroll();
            return;
        };

        let offset_from_bottom = match direction {
            ClientSelectionAutoscrollDirection::Up => metrics
                .offset_from_bottom
                .saturating_add(immediate_lines)
                .min(metrics.max_offset_from_bottom),
            ClientSelectionAutoscrollDirection::Down => {
                metrics.offset_from_bottom.saturating_sub(immediate_lines)
            }
        };
        if offset_from_bottom != metrics.offset_from_bottom {
            let projected = metrics.with_offset(offset_from_bottom);
            self.update_selection_cursor_with_metrics(
                hit,
                column,
                row,
                Some(projected),
                outcome,
                now,
            );
            self.push_pane_scroll_offset(hit.pane_id, offset_from_bottom, outcome);
        }
        self.mouse_selection.autoscroll = Some(ClientSelectionAutoscroll {
            pane_id: hit.pane_id,
            direction,
            last_mouse_column: column,
            last_mouse_row: row,
            inner_rect: hit.inner_rect,
            offset_from_bottom,
            max_offset_from_bottom: metrics.max_offset_from_bottom,
        });
        self.mouse_selection.autoscroll_deadline = Some(now + SELECTION_AUTOSCROLL_INTERVAL);
    }

    fn scroll_in_progress_selection(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
        now: Instant,
    ) -> bool {
        if !matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            return false;
        }
        let Some(hit) = self.active_selection_pane() else {
            return false;
        };
        let Some(metrics) = self.selection_scroll_metrics(&hit) else {
            return false;
        };
        let offset_from_bottom = match mouse.kind {
            MouseEventKind::ScrollUp => metrics
                .offset_from_bottom
                .saturating_add(usize::from(MOUSE_WHEEL_SCROLL_LINES))
                .min(metrics.max_offset_from_bottom),
            MouseEventKind::ScrollDown => metrics
                .offset_from_bottom
                .saturating_sub(usize::from(MOUSE_WHEEL_SCROLL_LINES)),
            _ => unreachable!(),
        };
        if offset_from_bottom != metrics.offset_from_bottom {
            let projected = metrics.with_offset(offset_from_bottom);
            self.update_selection_cursor_with_metrics(
                &hit,
                mouse.column,
                mouse.row,
                Some(projected),
                outcome,
                now,
            );
            self.push_pane_scroll_offset(hit.pane_id, offset_from_bottom, outcome);
            outcome.repaint = true;
        }
        true
    }

    fn request_selection_drag_repaint(&mut self, now: Instant) -> bool {
        // This gate follows the last composed frame so a suppressed drag repaints at the next
        // eligible frame deadline; it is not an input-send throttle.
        let deadline = self
            .presentation
            .composed_at()
            .map(|last| last + SELECTION_REPAINT_INTERVAL);
        self.mouse_selection.repaint_deadline = deadline.filter(|deadline| now < *deadline);
        self.mouse_selection.repaint_deadline.is_none()
    }

    pub(in crate::shell) fn tick_selection_autoscroll(
        &mut self,
        now: std::time::Instant,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        if self
            .mouse_selection
            .repaint_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.mouse_selection.repaint_deadline = None;
            outcome.repaint = true;
        }
        if self
            .mouse_selection
            .autoscroll_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return outcome;
        }
        let Some(mut autoscroll) = self.mouse_selection.autoscroll.clone() else {
            self.mouse_selection.autoscroll_deadline = None;
            return outcome;
        };
        let dragging = self.mouse_selection.word_gesture.as_ref().map_or_else(
            || {
                self.mouse_selection
                    .selection
                    .as_ref()
                    .is_some_and(|selection| {
                        selection.belongs_to(&autoscroll.pane_id) && selection.is_dragging()
                    })
            },
            |gesture| gesture.pane_id == autoscroll.pane_id && gesture.dragged && !gesture.released,
        );
        if !dragging {
            self.stop_selection_autoscroll();
            return outcome;
        }
        let Some(hit) = self
            .presentation
            .pane_hits()
            .iter()
            .find(|hit| hit.pane_id == autoscroll.pane_id)
            .cloned()
        else {
            self.stop_selection_autoscroll();
            return outcome;
        };
        if hit.inner_rect != autoscroll.inner_rect {
            self.stop_selection_autoscroll();
            return outcome;
        }
        let next_offset = match autoscroll.direction {
            ClientSelectionAutoscrollDirection::Up => autoscroll
                .offset_from_bottom
                .saturating_add(1)
                .min(autoscroll.max_offset_from_bottom),
            ClientSelectionAutoscrollDirection::Down => {
                autoscroll.offset_from_bottom.saturating_sub(1)
            }
        };
        if next_offset == autoscroll.offset_from_bottom {
            self.stop_selection_autoscroll();
            return outcome;
        }
        let Some(scroll) = hit.scroll else {
            self.stop_selection_autoscroll();
            return outcome;
        };
        autoscroll.offset_from_bottom = next_offset;
        let metrics = shepr_term::ScrollMetrics::new(
            next_offset,
            autoscroll.max_offset_from_bottom,
            scroll.viewport_rows,
            scroll.history_origin,
        );
        self.update_selection_cursor_with_metrics(
            &hit,
            autoscroll.last_mouse_column,
            autoscroll.last_mouse_row,
            Some(metrics),
            &mut outcome,
            now,
        );
        self.push_pane_scroll_offset(autoscroll.pane_id, next_offset, &mut outcome);
        self.mouse_selection.autoscroll = Some(autoscroll);
        self.mouse_selection.autoscroll_deadline = Some(now + SELECTION_AUTOSCROLL_INTERVAL);
        outcome.repaint = true;
        outcome
    }

    fn pane_split_target_is_current(
        &self,
        hit: &PaneSplitHit,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<bool> {
        let snapshot = self.endpoints.active.snapshot()?;
        let surface = self.pane_surface()?;
        if snapshot.revision != surface.projection_revision {
            return None;
        }
        Some(
            snapshot.focused_workspace_id.as_ref() == Some(workspace_id)
                && surface_has_split_of(surface, hit),
        )
    }

    fn pane_split_topology_matches_hit(
        &self,
        hit: &PaneSplitHit,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> bool {
        let Some(snapshot) = self.endpoints.active.snapshot() else {
            return false;
        };
        if snapshot.focused_workspace_id.as_ref() != Some(workspace_id) {
            return false;
        }

        // A split-ratio update advances the projection without changing the tree. During that
        // revision gap, the snapshot has no layout tree, so compare the hit with every surface
        // topology received for this boot, including one waiting for its snapshot.
        let matches_hit = |surface: &shepr_protocol::PaneSurfaceFrame| {
            surface.boot_id == snapshot.boot_id && surface_has_split_of(surface, hit)
        };
        let current_matches = self.pane_surface().is_some_and(matches_hit);
        let waiting_matches = self
            .presentation
            .surfaces
            .waiting_baseline()
            .is_none_or(matches_hit);
        current_matches && waiting_matches
    }

    fn pane_split_ratio(
        hit: &PaneSplitHit,
        grab_offset: i32,
        point: (u16, u16),
    ) -> shepr_core::layout::SplitRatio {
        let (pointer, origin, length) = match hit.direction {
            shepr_protocol::PaneSurfaceSplitDirection::Horizontal => {
                (i32::from(point.0), i32::from(hit.area.x), hit.area.width)
            }
            shepr_protocol::PaneSurfaceSplitDirection::Vertical => {
                (i32::from(point.1), i32::from(hit.area.y), hit.area.height)
            }
        };
        shepr_core::layout::SplitRatio::clamped(
            (pointer + grab_offset - origin) as f32 / f32::from(length.max(1)),
        )
    }

    fn workspace_drop_target_at(
        &self,
        point: (u16, u16),
    ) -> Option<(Option<shepr_protocol::WorkspaceId>, u16)> {
        let drop_bottom = if self.presentation.shown().new_workspace().height > 0 {
            self.presentation.shown().new_workspace().y
        } else {
            self.presentation.shown().workspace_body().bottom()
        };
        if self.presentation.shown().workspace_body().height == 0
            || point.1
                < self
                    .presentation
                    .shown()
                    .workspace_body()
                    .y
                    .saturating_sub(1)
            || point.1 >= drop_bottom
            || self.presentation.shown().workspaces().any(|hit| {
                hit.location.endpoint != *self.endpoints.presented()
                    && crate::shell::input::hit_test::contains(hit.rect, point)
            })
        {
            return None;
        }
        let mut slots = self
            .presentation
            .shown()
            .workspaces()
            .filter(|hit| hit.location.endpoint == *self.endpoints.presented())
            .filter_map(|hit| {
                hit.location
                    .workspace_id()
                    .map(|workspace_id| (Some(workspace_id), hit.rect.y.saturating_sub(1)))
            })
            .collect::<Vec<_>>();
        let snapshot = self.endpoints.active.snapshot()?;
        let last_hit = self
            .presentation
            .shown()
            .workspaces()
            .rev()
            .find(|hit| hit.location.endpoint == *self.endpoints.presented())?;
        let last_workspace_id = last_hit.location.workspace_id()?;
        let last_position = snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.workspace_id == last_workspace_id)?;
        let before = snapshot
            .workspaces
            .get(last_position + 1)
            .map(|workspace| workspace.workspace_id);
        let row = last_hit.rect.bottom();
        if row < drop_bottom {
            slots.push((before, row));
        }
        slots
            .into_iter()
            .enumerate()
            .min_by_key(|(index, (_, row))| (point.1.abs_diff(*row), *index))
            .map(|(_, target)| target)
    }

    fn workspace_move_command(
        &self,
        source_workspace_id: &shepr_protocol::WorkspaceId,
        before_workspace_id: Option<&shepr_protocol::WorkspaceId>,
    ) -> Option<shepr_protocol::command::EndpointCommand> {
        let snapshot = self.endpoints.active.snapshot()?;
        let source = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == *source_workspace_id)?;
        if before_workspace_id == Some(source_workspace_id) {
            return None;
        }
        let source_position = snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.workspace_id == *source_workspace_id)?;
        let remaining = snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.workspace_id != *source_workspace_id)
            .collect::<Vec<_>>();
        let insert_position = match before_workspace_id {
            Some(target) => remaining
                .iter()
                .position(|workspace| workspace.workspace_id == *target)?,
            None => remaining.len(),
        };
        if insert_position == source_position {
            return None;
        }

        Some(shepr_protocol::command::EndpointCommand::WorkspaceMove(
            shepr_protocol::command::WorkspaceMoveParams {
                workspace_id: source.workspace_id,
                before_workspace_id: before_workspace_id.copied(),
            },
        ))
    }

    pub(super) fn handle_mouse_with_accounting(
        &mut self,
        mouse: MouseEvent,
        now: Instant,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        let point = (mouse.column, mouse.row);
        // A new press while a chrome drag is still recorded means its release was lost (the
        // terminal lost focus mid-drag, or mouse reporting was toggled). Settle it before
        // anything else, including presses that overlays or pane gestures handle below.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(drag) = self.pointer.chrome_drag.take()
        {
            self.settle_chrome_drag(&drag, outcome);
        }
        if self.mode.is(ClientShellMode::Navigate)
            && self.workspace_preview_action_blocked()
            && self.overlay.is_none()
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
        {
            self.mode.set(self.copy_or_terminal_mode());
            outcome.repaint = true;
        }
        if let Some(gesture) = self.pointer.pane_mouse_gesture.as_ref() {
            let gesture_event = matches!(
                mouse.kind,
                MouseEventKind::Drag(button) | MouseEventKind::Up(button)
                    if button == gesture.button
            );
            if gesture_event {
                let button = gesture.button;
                let modifiers = mouse.modifiers;
                let hit = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| hit.pane_id == gesture.hit.pane_id)
                    .cloned()
                    .unwrap_or_else(|| gesture.hit.clone());
                let position = self.pane_mouse_position(&hit, mouse);
                if let Some(gesture) = self.pointer.pane_mouse_gesture.as_mut() {
                    gesture.last_event = mouse;
                    gesture.last_position = position;
                }
                self.push_pane_mouse_event(&hit, mouse, modifiers, outcome, accounting);
                if mouse.kind == MouseEventKind::Up(button) {
                    self.pointer.pane_mouse_gesture = None;
                }
                return;
            }
            if matches!(
                mouse.kind,
                MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_)
            ) {
                return;
            }
        }
        if self.notices.visible().is_some()
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && crate::shell::input::hit_test::contains(
                self.presentation.shown().notification_toast(),
                point,
            )
        {
            self.notices.advance();
            outcome.repaint = true;
            return;
        }
        if mouse.kind == MouseEventKind::Drag(MouseButton::Left) {
            match self.pointer.chrome_drag.as_ref() {
                Some(ClientChromeDrag::SidebarWidth { .. }) => {
                    self.set_sidebar_width_from_column(mouse.column, outcome);
                    return;
                }
                Some(ClientChromeDrag::SidebarSection) => {
                    self.set_sidebar_section_from_row(mouse.row, outcome);
                    return;
                }
                Some(ClientChromeDrag::WorkspaceScrollbar { grab_row_offset }) => {
                    if let Some(metrics) = self.presentation.shown().workspace_scroll_metrics() {
                        let offset = shepr_term::scroll::scrollbar_start_from_drag_row(
                            metrics,
                            crate::shell::view::list::scroll_track(
                                self.presentation.shown().workspace_scrollbar(),
                            ),
                            mouse.row,
                            *grab_row_offset,
                        );
                        if self.sidebar_scroll.scroll_workspaces_to(offset) {
                            outcome.repaint = true;
                        }
                    }
                    return;
                }
                Some(ClientChromeDrag::AgentScrollbar { grab_row_offset }) => {
                    if let Some(metrics) = self.presentation.shown().agent_scroll_metrics() {
                        let offset = shepr_term::scroll::scrollbar_start_from_drag_row(
                            metrics,
                            crate::shell::view::list::scroll_track(
                                self.presentation.shown().agent_scrollbar(),
                            ),
                            mouse.row,
                            *grab_row_offset,
                        );
                        if self.sidebar_scroll.scroll_agents_to(offset) {
                            outcome.repaint = true;
                        }
                    }
                    return;
                }
                Some(ClientChromeDrag::PaneScrollbar {
                    hit,
                    grab_row_offset,
                    last_sent_offset,
                    throttle,
                }) => {
                    let current_hit = self
                        .presentation
                        .pane_hits()
                        .iter()
                        .find(|current| current.pane_id == hit.pane_id)
                        .cloned()
                        .unwrap_or_else(|| hit.clone());
                    let Some(offset) = Self::pane_scrollbar_offset(
                        &current_hit,
                        mouse.row,
                        Some(*grab_row_offset),
                    ) else {
                        self.pointer.chrome_drag = None;
                        return;
                    };
                    let mut next_throttle = *throttle;
                    let should_send = *last_sent_offset != Some(offset) && next_throttle.admit(now);
                    if should_send {
                        if let Some(ClientChromeDrag::PaneScrollbar {
                            last_sent_offset,
                            throttle,
                            ..
                        }) = self.pointer.chrome_drag.as_mut()
                        {
                            *last_sent_offset = Some(offset);
                            *throttle = next_throttle;
                        }
                        self.push_pane_scroll_offset(current_hit.pane_id, offset, outcome);
                    }
                    return;
                }
                Some(ClientChromeDrag::PaneSplit {
                    hit,
                    workspace_id,
                    grab_offset,
                    last_sent_ratio,
                    throttle,
                }) => {
                    let hit = hit.clone();
                    let workspace_id = *workspace_id;
                    let grab_offset = *grab_offset;
                    let last_sent_ratio = *last_sent_ratio;
                    let mut next_throttle = *throttle;
                    match self.pane_split_target_is_current(&hit, &workspace_id) {
                        Some(true) => {}
                        Some(false) => {
                            self.pointer.chrome_drag = None;
                            return;
                        }
                        None => return,
                    }
                    let ratio = Self::pane_split_ratio(&hit, grab_offset, point);
                    // The endpoint already has this ratio; resending it would cost a
                    // command round trip and the throttle slot the next change needs.
                    if last_sent_ratio == Some(ratio) {
                        return;
                    }
                    let should_send = next_throttle.admit(now);
                    if should_send
                        && let Some(ClientChromeDrag::PaneSplit {
                            last_sent_ratio,
                            throttle,
                            ..
                        }) = self.pointer.chrome_drag.as_mut()
                    {
                        *last_sent_ratio = Some(ratio);
                        *throttle = next_throttle;
                    }
                    if should_send {
                        self.push_endpoint_command(
                            shepr_protocol::command::EndpointCommand::LayoutSetSplitRatio(
                                shepr_protocol::command::LayoutSetSplitRatioParams {
                                    workspace_id,
                                    path: hit.path,
                                    epoch: hit.epoch,
                                    ratio,
                                },
                            ),
                            outcome,
                        );
                    }
                    return;
                }
                Some(ClientChromeDrag::Workspace { .. }) => {
                    let target = self.workspace_drop_target_at(point);
                    if let Some(ClientChromeDrag::Workspace {
                        target: current, ..
                    }) = self.pointer.chrome_drag.as_mut()
                    {
                        *current = target;
                    }
                    outcome.repaint = true;
                    return;
                }
                None => {}
            }
            if let Some(press) = self.pointer.workspace_press.as_ref() {
                let delta = mouse
                    .column
                    .abs_diff(press.start_column)
                    .max(mouse.row.abs_diff(press.start_row));
                if delta >= 1 {
                    let source_workspace_id = press.location.workspace_id();
                    let draggable = self.endpoint_workspace_is_draggable(press);
                    if draggable
                        && let Some(source_workspace_id) = source_workspace_id
                        && let Some(target) = self.workspace_drop_target_at(point)
                    {
                        self.pointer.chrome_drag = Some(ClientChromeDrag::Workspace {
                            source_workspace_id,
                            target: Some(target),
                        });
                        outcome.repaint = true;
                    }
                }
                return;
            }
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
            if let Some(drag) = self.pointer.chrome_drag.take() {
                self.pointer.workspace_press = None;
                match drag {
                    ClientChromeDrag::Workspace {
                        source_workspace_id,
                        target,
                    } => {
                        if let Some((before_workspace_id, _)) = target
                            && let Some(command) = self.workspace_move_command(
                                &source_workspace_id,
                                before_workspace_id.as_ref(),
                            )
                        {
                            self.push_endpoint_command(command, outcome);
                        }
                        outcome.repaint = true;
                    }
                    ClientChromeDrag::PaneScrollbar {
                        hit,
                        grab_row_offset,
                        last_sent_offset,
                        ..
                    } => {
                        let current_hit = self
                            .presentation
                            .pane_hits()
                            .iter()
                            .find(|current| current.pane_id == hit.pane_id)
                            .cloned()
                            .unwrap_or(hit);
                        if let Some(offset) = Self::pane_scrollbar_offset(
                            &current_hit,
                            mouse.row,
                            Some(grab_row_offset),
                        ) && last_sent_offset != Some(offset)
                        {
                            self.push_pane_scroll_offset(current_hit.pane_id, offset, outcome);
                        }
                    }
                    ClientChromeDrag::PaneSplit {
                        hit,
                        workspace_id,
                        grab_offset,
                        last_sent_ratio,
                        ..
                    } => {
                        let target_is_current =
                            self.pane_split_topology_matches_hit(&hit, &workspace_id);
                        let ratio = Self::pane_split_ratio(&hit, grab_offset, point);
                        if target_is_current && last_sent_ratio.is_none_or(|sent| sent != ratio) {
                            self.push_endpoint_command(
                                shepr_protocol::command::EndpointCommand::LayoutSetSplitRatio(
                                    shepr_protocol::command::LayoutSetSplitRatioParams {
                                        workspace_id,
                                        path: hit.path,
                                        epoch: hit.epoch,
                                        ratio,
                                    },
                                ),
                                outcome,
                            );
                        }
                    }
                    drag @ (ClientChromeDrag::SidebarWidth { .. }
                    | ClientChromeDrag::SidebarSection) => {
                        self.settle_chrome_drag(&drag, outcome);
                    }
                    ClientChromeDrag::WorkspaceScrollbar { .. }
                    | ClientChromeDrag::AgentScrollbar { .. } => {}
                }
                return;
            }
            if let Some(press) = self.pointer.workspace_press.take() {
                self.finish_endpoint_workspace_press(press, outcome);
                return;
            }
        }
        // An open overlay takes every mouse event that reaches it; chrome drags, gestures and
        // toasts above have already had theirs.
        if self.overlay.is_some() {
            self.route_overlay_mouse(mouse, outcome);
            return;
        }

        if mouse.kind == MouseEventKind::Drag(MouseButton::Left) {
            let selection_hit = self.active_selection_pane();
            if let Some(hit) = selection_hit {
                self.update_selection_drag(&hit, mouse.column, mouse.row, now, outcome);
                // Consume every motion, but do not rebuild a frame for every intermediate position.
                outcome.repaint |=
                    !outcome.actions.is_empty() || self.request_selection_drag_repaint(now);
                return;
            }
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left)
            && self.mouse_selection.word_gesture.is_some()
        {
            self.finish_word_selection(outcome, now);
            outcome.repaint = true;
            return;
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left)
            && self.mouse_selection.selection.is_some()
        {
            self.stop_selection_autoscroll();
            let copied = self
                .mouse_selection
                .selection
                .as_mut()
                .is_some_and(shepr_term::selection::Selection::finish);
            if copied && self.config.copy_on_select {
                self.request_selection_copy(outcome);
                self.mouse_selection.clear();
            } else if self
                .mouse_selection
                .selection
                .as_ref()
                .is_some_and(shepr_term::selection::Selection::is_just_click)
            {
                self.mouse_selection.clear_range();
            }
            if copied {
                self.mouse_selection.last_pane_click = None;
            }
            outcome.repaint = true;
            return;
        }
        if self.scroll_in_progress_selection(mouse, outcome, now) {
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                let pane_hit = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| crate::shell::input::hit_test::contains(hit.inner_rect, point))
                    .cloned();
                if let Some(hit) = pane_hit {
                    let pane_owns_right_click = self
                        .endpoints
                        .active
                        .snapshot()
                        .and_then(|snapshot| {
                            snapshot
                                .panes
                                .iter()
                                .find(|pane| pane.pane_id == hit.pane_id)
                        })
                        .is_some_and(|pane| pane.right_click_passthrough)
                        && mouse.modifiers.is_empty();
                    if hit.mouse_reporting && pane_owns_right_click {
                        self.push_pane_mouse_event(
                            &hit,
                            mouse,
                            mouse.modifiers,
                            outcome,
                            accounting,
                        );
                        self.push_endpoint_command(
                            shepr_protocol::command::EndpointCommand::PaneFocus(
                                shepr_protocol::command::PaneTarget {
                                    pane_id: hit.pane_id,
                                },
                            ),
                            outcome,
                        );
                        self.pointer.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                            last_position: self.pane_mouse_position(&hit, mouse),
                            hit,
                            button: MouseButton::Right,
                            last_event: mouse,
                        });
                        return;
                    }
                }
                if !self.config.mouse_capture {
                    return;
                }
                let workspace_id = (!self.chrome.collapsed())
                    .then(|| self.active_endpoint_workspace_at(point))
                    .flatten();
                if let Some(workspace_id) = workspace_id {
                    self.open_workspace_context_menu(workspace_id, mouse.column, mouse.row);
                    outcome.repaint = true;
                    return;
                }
                let pane_id = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| crate::shell::input::hit_test::contains(hit.rect, point))
                    .map(|hit| hit.pane_id);
                if let Some(pane_id) = pane_id {
                    self.open_pane_context_menu(pane_id, mouse.column, mouse.row);
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().agent_body(),
                    point,
                ) =>
            {
                let next = self.sidebar_scroll.agent_start().saturating_sub(1);
                if self.sidebar_scroll.scroll_agents_to(next) {
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollDown
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().agent_body(),
                    point,
                ) =>
            {
                let next = self
                    .sidebar_scroll
                    .agent_start()
                    .saturating_add(1)
                    .min(self.presentation.shown().agent_max_scroll());
                if self.sidebar_scroll.scroll_agents_to(next) {
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().workspace_body(),
                    point,
                ) =>
            {
                let next = self.sidebar_scroll.workspace_start().saturating_sub(1);
                if self.sidebar_scroll.scroll_workspaces_to(next) {
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollDown
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().workspace_body(),
                    point,
                ) =>
            {
                let next = self
                    .sidebar_scroll
                    .workspace_start()
                    .saturating_add(1)
                    .min(self.presentation.shown().workspace_max_scroll());
                if self.sidebar_scroll.scroll_workspaces_to(next) {
                    outcome.repaint = true;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let previous_pane_click = self.mouse_selection.last_pane_click.take();
                if self.mouse_selection.selection.is_some() {
                    outcome.repaint = true;
                }
                self.mouse_selection.clear();
                self.pointer.workspace_press = None;
                // A drag still recorded here lost its release; the press at the top of this
                // function already settled it (see `settle_chrome_drag`). Split and pane
                // scrollbar drags are abandoned at the last value that was sent: the next
                // press starts a new gesture and the endpoint's state is consistent, so
                // the final throttled position is not replayed.
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().sidebar_divider(),
                    point,
                ) {
                    let double_click =
                        self.pointer.last_sidebar_divider_click.is_some_and(|last| {
                            now.duration_since(last) <= crate::limits::DOUBLE_CLICK_WINDOW
                        });
                    self.pointer.last_sidebar_divider_click = Some(now);
                    if double_click {
                        self.chrome.reset_width();
                        outcome.repaint = true;
                        outcome.resize = true;
                        self.persist_chrome_preferences(outcome);
                    } else {
                        self.pointer.chrome_drag = Some(ClientChromeDrag::SidebarWidth {
                            resize_pending: false,
                        });
                        self.set_sidebar_width_from_column(mouse.column, outcome);
                    }
                    return;
                }
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().section_divider(),
                    point,
                ) {
                    self.pointer.chrome_drag = Some(ClientChromeDrag::SidebarSection);
                    self.set_sidebar_section_from_row(mouse.row, outcome);
                    return;
                }
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().workspace_scrollbar(),
                    point,
                ) {
                    if let Some(metrics) = self.presentation.shown().workspace_scroll_metrics() {
                        if let Some(grab_row_offset) =
                            shepr_term::scroll::scrollbar_thumb_grab_offset(
                                metrics,
                                crate::shell::view::list::scroll_track(
                                    self.presentation.shown().workspace_scrollbar(),
                                ),
                                mouse.row,
                            )
                        {
                            self.pointer.chrome_drag =
                                Some(ClientChromeDrag::WorkspaceScrollbar { grab_row_offset });
                        } else {
                            let offset = shepr_term::scroll::scrollbar_start_from_row(
                                metrics,
                                crate::shell::view::list::scroll_track(
                                    self.presentation.shown().workspace_scrollbar(),
                                ),
                                mouse.row,
                            );
                            if self.sidebar_scroll.scroll_workspaces_to(offset) {
                                outcome.repaint = true;
                            }
                        }
                    }
                    return;
                }
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().agent_scrollbar(),
                    point,
                ) {
                    if let Some(metrics) = self.presentation.shown().agent_scroll_metrics() {
                        if let Some(grab_row_offset) =
                            shepr_term::scroll::scrollbar_thumb_grab_offset(
                                metrics,
                                crate::shell::view::list::scroll_track(
                                    self.presentation.shown().agent_scrollbar(),
                                ),
                                mouse.row,
                            )
                        {
                            self.pointer.chrome_drag =
                                Some(ClientChromeDrag::AgentScrollbar { grab_row_offset });
                        } else {
                            let offset = shepr_term::scroll::scrollbar_start_from_row(
                                metrics,
                                crate::shell::view::list::scroll_track(
                                    self.presentation.shown().agent_scrollbar(),
                                ),
                                mouse.row,
                            );
                            if self.sidebar_scroll.scroll_agents_to(offset) {
                                outcome.repaint = true;
                            }
                        }
                    }
                    return;
                }
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().agent_sort_toggle(),
                    point,
                ) {
                    let sort = match self.agent_panel_sort_chrome.value() {
                        shepr_config::AgentPanelSortConfig::Spaces => {
                            shepr_config::AgentPanelSortConfig::Priority
                        }
                        shepr_config::AgentPanelSortConfig::Priority => {
                            shepr_config::AgentPanelSortConfig::Spaces
                        }
                    };
                    self.set_agent_panel_sort(sort);
                    self.sidebar_scroll.reset_agents();
                    self.persist_chrome_preferences(outcome);
                    outcome.repaint = true;
                    return;
                }
                if self.handle_endpoint_machine_click(point, outcome) {
                    return;
                }
                if self.handle_machine_entry_click(point, outcome) {
                    return;
                }
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().global_launcher(),
                    point,
                ) {
                    self.toggle_global_menu();
                    outcome.repaint = true;
                    return;
                }
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().new_workspace(),
                    point,
                ) {
                    self.record_binding(&shepr_termio::input::KeybindAction::NewWorkspace, outcome);
                    return;
                }
                if crate::shell::input::hit_test::contains(
                    self.presentation.shown().sidebar_toggle(),
                    point,
                ) {
                    self.chrome.toggle_collapsed();
                    outcome.repaint = true;
                    outcome.resize = true;
                    self.persist_chrome_preferences(outcome);
                    return;
                }
                let workspace_press = self
                    .presentation
                    .shown()
                    .workspaces()
                    .find(|hit| crate::shell::input::hit_test::contains(hit.rect, point))
                    .map(|hit| ClientWorkspacePress {
                        location: hit.location.clone(),
                        start_column: mouse.column,
                        start_row: mouse.row,
                    });
                if let Some(workspace_press) = workspace_press {
                    self.pointer.workspace_press = Some(workspace_press);
                    return;
                }
                if self.handle_endpoint_agent_click(point, outcome) {
                    return;
                }
                let scrollbar_hit = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| {
                        hit.scrollbar_rect.is_some_and(|rect| {
                            crate::shell::input::hit_test::contains(rect, point)
                        }) && hit
                            .scroll
                            .is_some_and(|metrics| metrics.max_offset_from_bottom > 0)
                    })
                    .cloned();
                if let Some(hit) = scrollbar_hit {
                    // The focused copy pane derives Copy mode from its parked session.
                    let next_mode = if self.copy.as_ref().is_some_and(|copy_mode| {
                        copy_mode.pane_id == hit.pane_id
                            && copy_mode.pane_is_focused(self.focused_pane_id().as_ref())
                    }) {
                        ClientShellMode::Copy
                    } else {
                        ClientShellMode::Terminal
                    };
                    self.mode.set(next_mode);
                    self.push_endpoint_command(
                        shepr_protocol::command::EndpointCommand::PaneFocus(
                            shepr_protocol::command::PaneTarget {
                                pane_id: hit.pane_id,
                            },
                        ),
                        outcome,
                    );
                    let (Some(track), Some(metrics)) = (hit.scrollbar_rect, hit.scroll) else {
                        return;
                    };
                    if let Some(grab_row_offset) = shepr_term::scroll::scrollbar_thumb_grab_offset(
                        metrics,
                        crate::shell::view::list::scroll_track(track),
                        mouse.row,
                    ) {
                        self.pointer.chrome_drag = Some(ClientChromeDrag::PaneScrollbar {
                            hit,
                            grab_row_offset,
                            last_sent_offset: None,
                            throttle: Throttle::new(MOUSE_DRAG_SEND_INTERVAL),
                        });
                    } else if let Some(offset) = Self::pane_scrollbar_offset(&hit, mouse.row, None)
                    {
                        self.push_pane_scroll_offset(hit.pane_id, offset, outcome);
                    }
                    return;
                }
                let split_hit =
                    split_hit_at(self.presentation.shown().pane_splits(), point).cloned();
                if let Some(hit) = split_hit {
                    let Some(workspace_id) = self
                        .endpoints
                        .active
                        .snapshot()
                        .and_then(|snapshot| snapshot.focused_workspace_id)
                    else {
                        return;
                    };
                    let pointer = match hit.direction {
                        shepr_protocol::PaneSurfaceSplitDirection::Horizontal => mouse.column,
                        shepr_protocol::PaneSurfaceSplitDirection::Vertical => mouse.row,
                    };
                    self.pointer.chrome_drag = Some(ClientChromeDrag::PaneSplit {
                        grab_offset: i32::from(hit.pos) - i32::from(pointer),
                        last_sent_ratio: None,
                        throttle: Throttle::new(MOUSE_DRAG_SEND_INTERVAL),
                        hit,
                        workspace_id,
                    });
                    return;
                }
                let pane_hit = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| crate::shell::input::hit_test::contains(hit.rect, point))
                    .cloned();
                if let Some(hit) = pane_hit {
                    if hit.mouse_reporting
                        && crate::shell::input::hit_test::contains(hit.inner_rect, point)
                    {
                        self.push_pane_mouse_event(
                            &hit,
                            mouse,
                            mouse.modifiers,
                            outcome,
                            accounting,
                        );
                        self.pointer.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                            last_position: self.pane_mouse_position(&hit, mouse),
                            hit: hit.clone(),
                            button: MouseButton::Left,
                            last_event: mouse,
                        });
                    } else if crate::shell::input::hit_test::contains(hit.inner_rect, point) {
                        let click = ClientPaneClick {
                            pane_id: hit.pane_id,
                            viewport_row: mouse.row.saturating_sub(hit.inner_rect.y),
                            col: mouse.column.saturating_sub(hit.inner_rect.x),
                            at: now,
                        };
                        if let Some(metrics) = hit.scroll {
                            if mouse.modifiers.is_empty()
                                && previous_pane_click
                                    .as_ref()
                                    .is_some_and(|previous| previous.is_double_click_for(&click))
                            {
                                self.request_word_selection(
                                    &hit,
                                    metrics,
                                    click.viewport_row,
                                    click.col,
                                    outcome,
                                );
                            } else {
                                if mouse.modifiers.is_empty() {
                                    self.mouse_selection.last_pane_click = Some(click);
                                }
                                self.mouse_selection.focus_pending =
                                    (self.focused_pane_id().as_ref() != Some(&hit.pane_id))
                                        .then_some(hit.pane_id);
                                let (viewport_row, col) =
                                    selection_cell(mouse.column, mouse.row, hit.inner_rect);
                                let absolute_row = metrics.absolute_row_at_viewport(viewport_row);
                                self.mouse_selection.selection =
                                    Some(shepr_term::selection::Selection::anchor(
                                        hit.pane_id,
                                        shepr_term::Point::new(absolute_row, col),
                                    ));
                            }
                        } else {
                            // Selections hold absolute rows. Without the scroll
                            // origin this click cannot be anchored in them; it
                            // only clears the old selection.
                            self.mouse_selection.selection = None;
                        }
                    }
                    self.push_endpoint_command(
                        shepr_protocol::command::EndpointCommand::PaneFocus(
                            shepr_protocol::command::PaneTarget {
                                pane_id: hit.pane_id,
                            },
                        ),
                        outcome,
                    );
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                if let Some(hit) = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| {
                        crate::shell::input::hit_test::contains(hit.inner_rect, point)
                            && hit.mouse_reporting
                    })
                    .cloned()
                {
                    self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome, accounting);
                    self.pointer.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                        last_position: self.pane_mouse_position(&hit, mouse),
                        hit,
                        button: MouseButton::Middle,
                        last_event: mouse,
                    });
                }
            }
            MouseEventKind::Moved => {
                if let Some(hit) = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| {
                        crate::shell::input::hit_test::contains(hit.inner_rect, point)
                            && hit.mouse_reporting
                    })
                    .cloned()
                {
                    self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome, accounting);
                }
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                if let Some(hit) = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| crate::shell::input::hit_test::contains(hit.inner_rect, point))
                    .cloned()
                {
                    if self.focused_pane_id().as_ref() != Some(&hit.pane_id) {
                        self.push_endpoint_command(
                            shepr_protocol::command::EndpointCommand::PaneFocus(
                                shepr_protocol::command::PaneTarget {
                                    pane_id: hit.pane_id,
                                },
                            ),
                            outcome,
                        );
                    }
                    self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome, accounting);
                }
            }
            // Left and middle releases and drags outside a gesture, and the
            // rest, change nothing here.
            _ => {}
        }
    }

    fn pane_mouse_position(&self, hit: &PaneHit, mouse: MouseEvent) -> ClientMousePosition {
        let column = mouse.column.saturating_sub(hit.inner_rect.x);
        let row = mouse.row.saturating_sub(hit.inner_rect.y);
        let cell = ClientMousePosition::Cell { column, row };
        let Some(pixels) = self.pointer.host_mouse_pixels else {
            return cell;
        };
        let Some(presented) = hit.presented else {
            return cell;
        };
        let Some(extent) =
            shepr_term::mouse::pixel_mouse_eligible(self.host_cell, hit.pixel_mouse, presented)
        else {
            return cell;
        };
        pixels
            .pane_position(hit.inner_rect, extent)
            .map_or(cell, |(x, y)| ClientMousePosition::Pixels {
                column,
                row,
                report: shepr_term::mouse::PixelReport::new(x, y, extent),
            })
    }

    fn push_pane_mouse_event(
        &self,
        hit: &PaneHit,
        mouse: MouseEvent,
        modifiers: crossterm::event::KeyModifiers,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        let kind = shepr_protocol::ClientMouseKind::from_host(mouse.kind);
        let position = self.pane_mouse_position(hit, mouse);
        push_target_event(
            hit.pane_id,
            ClientPaneInputEvent::Mouse {
                kind,
                position,
                modifiers: shepr_protocol::WireModifiers::from_host(modifiers),
                lines: MOUSE_WHEEL_SCROLL_LINES,
            },
            outcome,
            accounting,
        );
    }
}

#[cfg(test)]
impl ClientShellState {
    /// One mouse event as its own input batch, for tests that drive the
    /// handler directly.
    pub(in crate::shell) fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        now: Instant,
        outcome: &mut ClientShellInput,
    ) {
        let mut accounting = PaneInputBatchAccounting::default();
        self.handle_mouse_with_accounting(mouse, now, outcome, &mut accounting);
    }
}

#[cfg(test)]
mod tests;
