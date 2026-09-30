//! Mouse routing for the client shell. Events here are raw host input bound
//! for panes; input content must stay out of logs and error messages here
//! (log content-free kinds instead).

use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(super) struct Throttle {
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

fn selection_cell(column: u16, row: u16, pane: Rect) -> (shepr_vt::ViewportRow, u16) {
    let column = column.clamp(pane.x, pane.x + pane.width.saturating_sub(1));
    let row = row.clamp(pane.y, pane.y + pane.height.saturating_sub(1));
    (shepr_vt::ViewportRow(row - pane.y), column - pane.x)
}

impl ClientShellState {
    /// Moves the sidebar edge during a width drag. The retained pane surface stays on screen,
    /// clipped to the new pane area; the endpoint resize waits for the release (see
    /// `ClientChromeDrag::SidebarWidth`).
    fn set_sidebar_width_from_column(&mut self, column: u16, outcome: &mut ClientShellInput) {
        let width = self
            .config
            .sidebar_bounds
            .clamp_width(column.saturating_add(1));
        if self.sidebar_width != width {
            self.sidebar_width = width;
            self.sidebar_width_manual = true;
            outcome.repaint = true;
            if let Some(ClientChromeDrag::SidebarWidth { resize_pending }) =
                self.chrome_drag.as_mut()
            {
                *resize_pending = true;
            } else {
                outcome.resize = true;
            }
        }
    }

    fn set_sidebar_section_from_row(&mut self, row: u16, outcome: &mut ClientShellInput) {
        let divider = self.hits.sidebar_divider;
        if divider.height == 0 {
            return;
        }
        let ratio = row.saturating_sub(divider.y) as f32 / divider.height as f32;
        let ratio = super::sidebar_tokens::SectionSplit::from_drag(ratio);
        if self.sidebar_section_split != ratio {
            self.sidebar_section_split = ratio;
            self.sidebar_section_split_manual = true;
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
            Some(grab_row_offset) => shepr_termio::scroll::scrollbar_offset_from_drag_row(
                metrics,
                track,
                row,
                grab_row_offset,
            ),
            None => shepr_termio::scroll::scrollbar_offset_from_row(metrics, track, row),
        })
    }

    pub(super) fn push_pane_scroll_offset(
        &mut self,
        pane_id: shepr_protocol::PublicPaneId,
        offset_from_bottom: usize,
        outcome: &mut ClientShellInput,
    ) {
        self.pane_scroll_targets
            .insert(pane_id.clone(), offset_from_bottom);
        if self.pane_scroll_in_flight.contains_key(&pane_id) {
            self.pane_scroll_queued.insert(pane_id, offset_from_bottom);
            return;
        }
        self.dispatch_pane_scroll_offset(&pane_id, offset_from_bottom, outcome);
    }

    fn dispatch_pane_scroll_offset(
        &mut self,
        pane_id: &shepr_protocol::PublicPaneId,
        offset_from_bottom: usize,
        outcome: &mut ClientShellInput,
    ) {
        if self.snapshot.is_none() {
            return;
        }
        self.next_scroll_serial = self.next_scroll_serial.saturating_add(1);
        let serial = self.next_scroll_serial;
        self.pane_scroll_targets
            .insert(pane_id.to_owned(), offset_from_bottom);
        self.pane_scroll_in_flight
            .insert(pane_id.to_owned(), serial);
        if !self.push_endpoint_command_with_kind(
            shepr_protocol::command::EndpointCommand::PaneScroll(
                shepr_protocol::command::PaneScrollParams {
                    pane_id: pane_id.clone(),
                    offset_from_bottom: offset_from_bottom as u64,
                },
            ),
            PendingEndpointKind::PaneScroll {
                pane_id: pane_id.to_owned(),
                serial,
            },
            outcome,
        ) {
            self.pane_scroll_targets.remove(pane_id);
            self.pane_scroll_in_flight.remove(pane_id);
        }
    }

    pub(super) fn complete_pane_scroll(
        &mut self,
        pane_id: &shepr_protocol::PublicPaneId,
        serial: u64,
        result: Result<shepr_protocol::command::EndpointReply, ClientShellEndpointError>,
        now: std::time::Instant,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.pane_scroll_in_flight.get(pane_id).copied() != Some(serial) {
            return false;
        }
        self.pane_scroll_in_flight.remove(pane_id);
        let repaint = match result {
            Ok(shepr_protocol::command::EndpointReply::PaneInfo { pane })
                if pane.pane_id == *pane_id =>
            {
                if let Some(scroll) = pane.scroll
                    && self.pane_scroll_targets.contains_key(pane_id)
                {
                    self.pane_scroll_targets.insert(
                        pane_id.to_owned(),
                        usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX),
                    );
                }
                false
            }
            Ok(_) => {
                self.pane_scroll_queued.remove(pane_id);
                self.pane_scroll_targets.remove(pane_id);
                self.set_endpoint_error("endpoint returned an unexpected pane-scroll result", now);
                true
            }
            Err(_) => {
                self.pane_scroll_queued.remove(pane_id);
                self.pane_scroll_targets.remove(pane_id);
                true
            }
        };
        if let Some(offset) = self.pane_scroll_queued.remove(pane_id) {
            self.dispatch_pane_scroll_offset(pane_id, offset, outcome);
        }
        repaint
    }

    pub(super) fn stop_selection_autoscroll(&mut self) {
        self.selection_autoscroll = None;
        self.selection_autoscroll_deadline = None;
    }

    fn selection_edge_scroll_lines(distance: u16) -> usize {
        usize::from(distance)
            .saturating_mul(crate::limits::SELECTION_EDGE_SCROLL_LINES_PER_ROW)
            .clamp(
                crate::limits::MIN_SELECTION_EDGE_SCROLL_LINES,
                crate::limits::MAX_SELECTION_EDGE_SCROLL_LINES,
            )
    }

    fn selection_scroll_metrics(&self, hit: &PaneHit) -> Option<shepr_termio::ScrollMetrics> {
        let metrics = hit.scroll?;
        Some(
            self.selection_autoscroll
                .as_ref()
                .filter(|autoscroll| autoscroll.pane_id == hit.pane_id)
                .map_or(metrics, |autoscroll| shepr_termio::ScrollMetrics {
                    offset_from_bottom: autoscroll.offset_from_bottom,
                    max_offset_from_bottom: autoscroll.max_offset_from_bottom,
                    viewport_rows: metrics.viewport_rows,
                    history_origin: metrics.history_origin,
                }),
        )
    }

    fn active_selection_pane(&self) -> Option<PaneHit> {
        let pane_id = if let Some(gesture) = self.word_selection_gesture.as_ref() {
            if gesture.released {
                return None;
            }
            &gesture.pane_id
        } else {
            &self
                .selection
                .as_ref()
                .filter(|selection| selection.is_in_progress())?
                .pane_id
        };
        self.hits
            .panes
            .iter()
            .find(|hit| &hit.pane_id == pane_id)
            .cloned()
    }

    fn update_selection_cursor_with_metrics(
        &mut self,
        hit: &PaneHit,
        column: u16,
        row: u16,
        metrics: Option<shepr_termio::ScrollMetrics>,
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
        if self.word_selection_gesture.is_some() {
            self.drag_word_selection((absolute_row, col), outcome, now);
        } else if let Some(selection) = self.selection.as_mut() {
            selection.drag(shepr_vt::Point::new(absolute_row, col));
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
            .selection
            .as_ref()
            .is_some_and(shepr_vt::selection::Selection::is_dragging);
        let moved_from_anchor = self.selection.as_ref().is_some_and(|selection| {
            let anchor = selection.anchor_position();
            let top = metrics.map_or(
                shepr_vt::AbsRow(0),
                shepr_termio::ScrollMetrics::viewport_top_row,
            );
            let anchor_row = hit
                .inner_rect
                .y
                .saturating_add(anchor.row.viewport_row(top).0)
                .clamp(
                    hit.inner_rect.y,
                    hit.inner_rect.y + hit.inner_rect.height.saturating_sub(1),
                );
            let anchor_col = hit.inner_rect.x.saturating_add(anchor.col).clamp(
                hit.inner_rect.x,
                hit.inner_rect.x + hit.inner_rect.width.saturating_sub(1),
            );
            anchor_row != row || anchor_col != column
        });
        self.update_selection_cursor_with_metrics(hit, column, row, metrics, outcome, now);
        let is_dragging = self
            .word_selection_gesture
            .as_ref()
            .map_or(was_dragging || moved_from_anchor, |gesture| gesture.dragged);
        if is_dragging {
            if let Some(selection) = self.selection.as_mut()
                && selection.is_just_click()
            {
                selection.force_dragging();
            }
            self.last_pane_click = None;
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
        let bottom = hit.inner_rect.y + hit.inner_rect.height.saturating_sub(1);
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
            let projected = shepr_termio::ScrollMetrics {
                offset_from_bottom,
                ..metrics
            };
            self.update_selection_cursor_with_metrics(
                hit,
                column,
                row,
                Some(projected),
                outcome,
                now,
            );
            self.push_pane_scroll_offset(hit.pane_id.clone(), offset_from_bottom, outcome);
        }
        self.selection_autoscroll = Some(ClientSelectionAutoscroll {
            pane_id: hit.pane_id.clone(),
            direction,
            last_mouse_column: column,
            last_mouse_row: row,
            inner_rect: hit.inner_rect,
            offset_from_bottom,
            max_offset_from_bottom: metrics.max_offset_from_bottom,
        });
        self.selection_autoscroll_deadline =
            Some(now + crate::limits::SELECTION_AUTOSCROLL_INTERVAL);
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
                .saturating_add(usize::from(self.config.mouse_scroll_lines))
                .min(metrics.max_offset_from_bottom),
            MouseEventKind::ScrollDown => metrics
                .offset_from_bottom
                .saturating_sub(usize::from(self.config.mouse_scroll_lines)),
            _ => unreachable!(),
        };
        if offset_from_bottom != metrics.offset_from_bottom {
            let projected = shepr_termio::ScrollMetrics {
                offset_from_bottom,
                ..metrics
            };
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

    pub(super) fn request_selection_drag_repaint(&mut self, now: Instant) -> bool {
        // This gate follows the last composed frame so a suppressed drag repaints at the next
        // eligible frame deadline; it is not an input-send throttle.
        let deadline = self
            .last_composed_at
            .map(|last| last + crate::limits::SELECTION_REPAINT_INTERVAL);
        self.selection_repaint_deadline = deadline.filter(|deadline| now < *deadline);
        self.selection_repaint_deadline.is_none()
    }

    pub(crate) fn tick_selection_autoscroll(
        &mut self,
        now: std::time::Instant,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        if self
            .selection_repaint_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.selection_repaint_deadline = None;
            outcome.repaint = true;
        }
        if self
            .selection_autoscroll_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return outcome;
        }
        let Some(mut autoscroll) = self.selection_autoscroll.clone() else {
            self.selection_autoscroll_deadline = None;
            return outcome;
        };
        let dragging = self.word_selection_gesture.as_ref().map_or_else(
            || {
                self.selection.as_ref().is_some_and(|selection| {
                    selection.pane_id == autoscroll.pane_id && selection.is_dragging()
                })
            },
            |gesture| gesture.pane_id == autoscroll.pane_id && gesture.dragged && !gesture.released,
        );
        if !dragging {
            self.stop_selection_autoscroll();
            return outcome;
        }
        let Some(hit) = self
            .hits
            .panes
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
        autoscroll.offset_from_bottom = next_offset;
        let metrics = shepr_termio::ScrollMetrics {
            offset_from_bottom: next_offset,
            max_offset_from_bottom: autoscroll.max_offset_from_bottom,
            viewport_rows: hit.scroll.map_or(0, |metrics| metrics.viewport_rows),
            history_origin: hit
                .scroll
                .map_or(shepr_vt::AbsRow(0), |metrics| metrics.history_origin),
        };
        self.update_selection_cursor_with_metrics(
            &hit,
            autoscroll.last_mouse_column,
            autoscroll.last_mouse_row,
            Some(metrics),
            &mut outcome,
            now,
        );
        self.push_pane_scroll_offset(autoscroll.pane_id.clone(), next_offset, &mut outcome);
        self.selection_autoscroll = Some(autoscroll);
        self.selection_autoscroll_deadline =
            Some(now + crate::limits::SELECTION_AUTOSCROLL_INTERVAL);
        outcome.repaint = true;
        outcome
    }

    fn pane_split_target_is_current(&self, hit: &PaneSplitHit, workspace_id: &str) -> Option<bool> {
        let snapshot = self.snapshot.as_deref()?;
        let surface = self.pane_surface.as_ref()?;
        if snapshot.revision != surface.projection_revision {
            return None;
        }
        Some(
            snapshot.focused_workspace_id.as_deref() == Some(workspace_id)
                && pane_surface_topology_signature(surface) == hit.topology_signature,
        )
    }

    fn pane_split_ratio(hit: &PaneSplitHit, grab_offset: i32, point: (u16, u16)) -> f32 {
        let (pointer, origin, length) = match hit.direction {
            shepr_protocol::PaneSurfaceSplitDirection::Horizontal => {
                (i32::from(point.0), i32::from(hit.area.x), hit.area.width)
            }
            shepr_protocol::PaneSurfaceSplitDirection::Vertical => {
                (i32::from(point.1), i32::from(hit.area.y), hit.area.height)
            }
        };
        ((pointer + grab_offset - origin) as f32 / f32::from(length.max(1))).clamp(
            shepr_core::layout::MIN_SPLIT_RATIO,
            shepr_core::layout::MAX_SPLIT_RATIO,
        )
    }

    fn workspace_drop_target_at(
        &self,
        point: (u16, u16),
    ) -> Option<(Option<shepr_protocol::WorkspaceId>, u16)> {
        if self.hits.workspace_body.height == 0
            || point.1 < self.hits.workspace_body.y.saturating_sub(1)
            || point.1 >= self.hits.new_workspace.y
            || self.hits.workspaces.iter().any(|hit| {
                hit.endpoint_id != self.active_endpoint_id && super::contains(hit.rect, point)
            })
        {
            return None;
        }
        let mut slots = self
            .hits
            .workspaces
            .iter()
            .filter(|hit| hit.endpoint_id == self.active_endpoint_id)
            .map(|hit| (Some(hit.workspace_id.clone()), hit.rect.y.saturating_sub(1)))
            .collect::<Vec<_>>();
        let snapshot = self.snapshot.as_deref()?;
        let entries = render::workspace_entries(snapshot);
        let last_hit = self
            .hits
            .workspaces
            .iter()
            .rev()
            .find(|hit| hit.endpoint_id == self.active_endpoint_id)?;
        let last_position = entries.iter().position(|entry| {
            snapshot
                .workspaces
                .get(*entry)
                .is_some_and(|workspace| workspace.workspace_id == last_hit.workspace_id)
        })?;
        let before = entries.get(last_position + 1).and_then(|entry| {
            snapshot
                .workspaces
                .get(*entry)
                .map(|workspace| workspace.workspace_id.clone())
        });
        let row = last_hit.rect.bottom();
        if row < self.hits.new_workspace.y {
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
        let snapshot = self.snapshot.as_deref()?;
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

        {
            let insert_index = before_workspace_id
                .and_then(|target| {
                    snapshot
                        .workspaces
                        .iter()
                        .position(|workspace| workspace.workspace_id == *target)
                })
                .unwrap_or(snapshot.workspaces.len());
            Some(shepr_protocol::command::EndpointCommand::WorkspaceMove(
                shepr_protocol::command::WorkspaceMoveParams {
                    workspace_id: source.workspace_id.clone(),
                    insert_index,
                },
            ))
        }
    }

    pub(super) fn handle_mouse_with_accounting(
        &mut self,
        mouse: MouseEvent,
        now: Instant,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        let point = (mouse.column, mouse.row);
        if self.mode == ClientShellMode::Navigate
            && self.workspace_preview_action_blocked()
            && self.overlay.is_none()
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
        {
            self.mode = self.copy_or_terminal_mode();
            self.navigate_workspace_id = None;
            outcome.repaint = true;
        }
        if let Some(gesture) = self.pane_mouse_gesture.as_ref() {
            let gesture_event = matches!(
                mouse.kind,
                MouseEventKind::Drag(button) | MouseEventKind::Up(button)
                    if button == gesture.button
            );
            if gesture_event {
                let button = gesture.button;
                let modifiers = mouse.modifiers.difference(gesture.stripped_modifiers);
                let hit = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| hit.pane_id == gesture.hit.pane_id)
                    .cloned()
                    .unwrap_or_else(|| gesture.hit.clone());
                let position = self.pane_mouse_position(&hit, mouse);
                if let Some(gesture) = self.pane_mouse_gesture.as_mut() {
                    gesture.last_event = mouse;
                    gesture.last_position = position;
                }
                self.push_pane_mouse_event(&hit, mouse, modifiers, outcome, accounting);
                if mouse.kind == MouseEventKind::Up(button) {
                    self.pane_mouse_gesture = None;
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
        if self.visible_endpoint_notice.is_some()
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && super::contains(self.hits.notification_toast, point)
        {
            self.visible_endpoint_notice = None;
            outcome.repaint = true;
            return;
        }
        if mouse.kind == MouseEventKind::Drag(MouseButton::Left) {
            match self.chrome_drag.as_ref() {
                Some(ClientChromeDrag::SidebarWidth { .. }) => {
                    self.set_sidebar_width_from_column(mouse.column, outcome);
                    return;
                }
                Some(ClientChromeDrag::SidebarSection) => {
                    self.set_sidebar_section_from_row(mouse.row, outcome);
                    return;
                }
                Some(ClientChromeDrag::WorkspaceScrollbar { grab_row_offset }) => {
                    if let Some(metrics) = self.hits.workspace_scroll_metrics {
                        let offset = shepr_termio::scroll::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.workspace_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                        if next != self.workspace_scroll {
                            self.workspace_scroll = next;
                            outcome.repaint = true;
                        }
                    }
                    return;
                }
                Some(ClientChromeDrag::AgentScrollbar { grab_row_offset }) => {
                    if let Some(metrics) = self.hits.agent_scroll_metrics {
                        let offset = shepr_termio::scroll::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.agent_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                        if next != self.agent_scroll {
                            self.agent_scroll = next;
                            outcome.repaint = true;
                        }
                    }
                    return;
                }
                Some(ClientChromeDrag::NavigatorScrollbar { grab_row_offset }) => {
                    if let Some(metrics) = self.hits.navigator_scroll_metrics {
                        let offset = shepr_termio::scroll::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.navigator_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        self.scroll_navigator_to(
                            metrics.max_offset_from_bottom.saturating_sub(offset),
                            metrics.viewport_rows,
                        );
                        outcome.repaint = true;
                    }
                    return;
                }
                Some(ClientChromeDrag::HelpScrollbar { grab_row_offset }) => {
                    if let (Some(metrics), Some(ClientShellOverlay::Help(help))) =
                        (self.hits.help_scroll_metrics, self.overlay.as_mut())
                    {
                        let offset = shepr_termio::scroll::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.help_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                        if next != help.scroll {
                            help.scroll = next;
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
                        .hits
                        .panes
                        .iter()
                        .find(|current| current.pane_id == hit.pane_id)
                        .cloned()
                        .unwrap_or_else(|| hit.clone());
                    let Some(offset) = Self::pane_scrollbar_offset(
                        &current_hit,
                        mouse.row,
                        Some(*grab_row_offset),
                    ) else {
                        self.chrome_drag = None;
                        return;
                    };
                    let mut next_throttle = *throttle;
                    let should_send = *last_sent_offset != Some(offset) && next_throttle.admit(now);
                    if should_send {
                        if let Some(ClientChromeDrag::PaneScrollbar {
                            last_sent_offset,
                            throttle,
                            ..
                        }) = self.chrome_drag.as_mut()
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
                    throttle,
                    ..
                }) => {
                    let hit = hit.clone();
                    let workspace_id = workspace_id.clone();
                    let grab_offset = *grab_offset;
                    let mut next_throttle = *throttle;
                    match self.pane_split_target_is_current(&hit, &workspace_id) {
                        Some(true) => {}
                        Some(false) => {
                            self.chrome_drag = None;
                            return;
                        }
                        None => return,
                    }
                    let ratio = Self::pane_split_ratio(&hit, grab_offset, point);
                    let should_send = next_throttle.admit(now);
                    if should_send
                        && let Some(ClientChromeDrag::PaneSplit {
                            last_sent_ratio,
                            throttle,
                            ..
                        }) = self.chrome_drag.as_mut()
                    {
                        *last_sent_ratio = Some(ratio);
                        *throttle = next_throttle;
                    }
                    if should_send {
                        self.push_endpoint_command(
                            shepr_protocol::command::EndpointCommand::LayoutSetSplitRatio(
                                shepr_protocol::command::LayoutSetSplitRatioParams {
                                    workspace_id: workspace_id.clone(),
                                    path: hit
                                        .path
                                        .into_iter()
                                        .map(|branch| {
                                            branch == shepr_core::geometry::SplitBranch::Second
                                        })
                                        .collect(),
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
                    }) = self.chrome_drag.as_mut()
                    {
                        *current = target;
                    }
                    outcome.repaint = true;
                    return;
                }
                None => {}
            }
            if let Some(press) = self.workspace_press.as_ref() {
                let delta = mouse
                    .column
                    .abs_diff(press.start_column)
                    .max(mouse.row.abs_diff(press.start_row));
                if delta >= 1 {
                    let source_workspace_id = press.workspace_id.clone();
                    let draggable = self.endpoint_workspace_is_draggable(press);
                    if draggable && let Some(target) = self.workspace_drop_target_at(point) {
                        self.chrome_drag = Some(ClientChromeDrag::Workspace {
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
            if let Some(drag) = self.chrome_drag.take() {
                self.workspace_press = None;
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
                            .hits
                            .panes
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
                            self.pane_split_target_is_current(&hit, &workspace_id) == Some(true);
                        let ratio = Self::pane_split_ratio(&hit, grab_offset, point);
                        if target_is_current
                            && last_sent_ratio
                                .is_none_or(|sent| (sent - ratio).abs() > f32::EPSILON)
                        {
                            self.push_endpoint_command(
                                shepr_protocol::command::EndpointCommand::LayoutSetSplitRatio(
                                    shepr_protocol::command::LayoutSetSplitRatioParams {
                                        workspace_id: workspace_id.clone(),
                                        path: hit
                                            .path
                                            .into_iter()
                                            .map(|branch| {
                                                branch == shepr_core::geometry::SplitBranch::Second
                                            })
                                            .collect(),
                                        ratio,
                                    },
                                ),
                                outcome,
                            );
                        }
                    }
                    ClientChromeDrag::SidebarWidth { resize_pending } => {
                        outcome.resize |= resize_pending;
                        self.persist_chrome_preferences(outcome);
                    }
                    ClientChromeDrag::SidebarSection => {
                        self.persist_chrome_preferences(outcome);
                    }
                    ClientChromeDrag::WorkspaceScrollbar { .. }
                    | ClientChromeDrag::AgentScrollbar { .. }
                    | ClientChromeDrag::HelpScrollbar { .. }
                    | ClientChromeDrag::NavigatorScrollbar { .. } => {}
                }
                return;
            }
            if let Some(press) = self.workspace_press.take() {
                self.finish_endpoint_workspace_press(press, outcome);
                return;
            }
        }
        if matches!(self.overlay, Some(ClientShellOverlay::GlobalMenu(_))) {
            let row_hit = self
                .hits
                .global_menu_rows
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
                .copied();
            match mouse.kind {
                MouseEventKind::Moved => {
                    if let (Some((_, index)), Some(ClientShellOverlay::GlobalMenu(menu))) =
                        (row_hit, self.overlay.as_mut())
                    {
                        menu.highlighted = index;
                        outcome.repaint = true;
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.global_launcher, point) {
                        self.toggle_global_menu();
                        outcome.repaint = true;
                    } else if let Some((_, index)) = row_hit {
                        self.activate_global_menu_item(index, outcome);
                    } else {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::ContextMenu(_))) {
            let row_hit = self
                .hits
                .context_menu_rows
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
                .copied();
            match mouse.kind {
                MouseEventKind::Moved => {
                    if let (Some((_, index)), Some(ClientShellOverlay::ContextMenu(menu))) =
                        (row_hit, self.overlay.as_mut())
                    {
                        menu.highlighted = index;
                        outcome.repaint = true;
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some((_, index)) = row_hit {
                        self.activate_context_menu_item(index, outcome);
                    } else {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Help(_))) {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                        let next = help.scroll.saturating_sub(3);
                        if next != help.scroll {
                            help.scroll = next;
                            outcome.repaint = true;
                        }
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                        let next = help.scroll.saturating_add(3).min(self.hits.help_max_scroll);
                        if next != help.scroll {
                            help.scroll = next;
                            outcome.repaint = true;
                        }
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.help_scrollbar, point) {
                        if let Some(metrics) = self.hits.help_scroll_metrics {
                            if let Some(grab_row_offset) =
                                shepr_termio::scroll::scrollbar_thumb_grab_offset(
                                    metrics,
                                    self.hits.help_scrollbar,
                                    mouse.row,
                                )
                            {
                                self.chrome_drag =
                                    Some(ClientChromeDrag::HelpScrollbar { grab_row_offset });
                            } else {
                                let offset = shepr_termio::scroll::scrollbar_offset_from_row(
                                    metrics,
                                    self.hits.help_scrollbar,
                                    mouse.row,
                                );
                                if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut()
                                {
                                    help.scroll =
                                        metrics.max_offset_from_bottom.saturating_sub(offset);
                                    outcome.repaint = true;
                                }
                            }
                        }
                    } else if super::contains(self.hits.overlay_cancel, point) {
                        let search_focused = matches!(
                            self.overlay,
                            Some(ClientShellOverlay::Help(ClientHelpOverlay {
                                search_focused: true,
                                ..
                            }))
                        );
                        if search_focused {
                            if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                                help.search_focused = false;
                                help.query.clear();
                                help.scroll = 0;
                            }
                        } else {
                            self.overlay = None;
                        }
                        outcome.repaint = true;
                    } else if !super::contains(self.hits.help_popup, point) {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Navigator(_))) {
            let row_hit = self
                .hits
                .navigator_rows
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
                .cloned();
            match mouse.kind {
                MouseEventKind::Moved => {
                    if let Some((_, target)) = row_hit {
                        if let Some(ClientShellOverlay::Navigator(navigator)) =
                            self.overlay.as_mut()
                        {
                            navigator.selected = Some(target);
                        }
                        outcome.repaint = true;
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.navigator_scrollbar, point) {
                        if let Some(metrics) = self.hits.navigator_scroll_metrics {
                            if let Some(grab_row_offset) =
                                shepr_termio::scroll::scrollbar_thumb_grab_offset(
                                    metrics,
                                    self.hits.navigator_scrollbar,
                                    mouse.row,
                                )
                            {
                                self.chrome_drag =
                                    Some(ClientChromeDrag::NavigatorScrollbar { grab_row_offset });
                            } else {
                                let offset = shepr_termio::scroll::scrollbar_offset_from_row(
                                    metrics,
                                    self.hits.navigator_scrollbar,
                                    mouse.row,
                                );
                                self.scroll_navigator_to(
                                    metrics.max_offset_from_bottom.saturating_sub(offset),
                                    metrics.viewport_rows,
                                );
                                outcome.repaint = true;
                            }
                        }
                    } else if super::contains(self.hits.navigator_search, point) {
                        if let Some(ClientShellOverlay::Navigator(navigator)) =
                            self.overlay.as_mut()
                        {
                            navigator.search_focused = true;
                            navigator.filter = None;
                        }
                        outcome.repaint = true;
                    } else if let Some((_, target)) = row_hit {
                        if let Some(ClientShellOverlay::Navigator(navigator)) =
                            self.overlay.as_mut()
                        {
                            navigator.selected = Some(target);
                        }
                        self.accept_navigator_selection(outcome);
                    } else if !super::contains(self.hits.navigator_popup, point) {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                MouseEventKind::ScrollUp => {
                    self.move_navigator_selection(-3);
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown => {
                    self.move_navigator_selection(3);
                    outcome.repaint = true;
                }
                _ => {}
            }
            return;
        }
        if self.overlay.is_some() {
            if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
                return;
            }
            if super::contains(self.hits.overlay_primary, point) {
                match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Rename(_)) => self.save_rename_overlay(outcome),
                    Some(ClientShellOverlay::ConfirmClose(_)) => {
                        self.accept_close_confirmation(outcome);
                    }
                    _ => {}
                }
            } else if super::contains(self.hits.overlay_clear, point) {
                if let Some(ClientShellOverlay::Rename(rename)) = self.overlay.as_mut() {
                    rename.input.clear();
                    outcome.repaint = true;
                }
            } else {
                self.overlay = None;
                outcome.repaint = true;
            }
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
            && self.word_selection_gesture.is_some()
        {
            self.finish_word_selection(outcome, now);
            outcome.repaint = true;
            return;
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) && self.selection.is_some() {
            self.stop_selection_autoscroll();
            let copied = self
                .selection
                .as_mut()
                .is_some_and(shepr_vt::selection::Selection::finish);
            if copied && self.config.copy_on_select {
                self.request_selection_copy(outcome);
                self.selection = None;
            } else if self
                .selection
                .as_ref()
                .is_some_and(shepr_vt::selection::Selection::is_just_click)
            {
                self.selection = None;
            }
            if copied {
                self.last_pane_click = None;
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
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.inner_rect, point))
                    .cloned();
                if let Some(hit) = pane_hit {
                    let pane_owns_right_click = self
                        .snapshot
                        .as_deref()
                        .and_then(|snapshot| {
                            snapshot
                                .panes
                                .iter()
                                .find(|pane| pane.pane_id == hit.pane_id)
                        })
                        .is_some_and(|pane| pane.right_click_passthrough)
                        && mouse.modifiers.is_empty();
                    let configured_modifiers = self
                        .config
                        .right_click_passthrough_modifiers
                        .filter(|modifiers| *modifiers == mouse.modifiers);
                    if hit.mouse_reporting
                        && (pane_owns_right_click || configured_modifiers.is_some())
                    {
                        let stripped_modifiers =
                            configured_modifiers.unwrap_or(crossterm::event::KeyModifiers::empty());
                        self.push_pane_mouse_event(
                            &hit,
                            mouse,
                            mouse.modifiers.difference(stripped_modifiers),
                            outcome,
                            accounting,
                        );
                        self.push_endpoint_command(
                            shepr_protocol::command::EndpointCommand::PaneFocus(
                                shepr_protocol::command::PaneTarget {
                                    pane_id: hit.pane_id.clone(),
                                },
                            ),
                            outcome,
                        );
                        self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                            last_position: self.pane_mouse_position(&hit, mouse),
                            hit,
                            button: MouseButton::Right,
                            stripped_modifiers,
                            last_event: mouse,
                        });
                        return;
                    }
                }
                if !self.config.mouse_capture {
                    return;
                }
                let workspace_id = (!self.sidebar_collapsed)
                    .then(|| self.active_endpoint_workspace_at(point))
                    .flatten();
                if let Some(workspace_id) = workspace_id {
                    self.open_workspace_context_menu(workspace_id, mouse.column, mouse.row);
                    outcome.repaint = true;
                    return;
                }
                let pane_id = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.rect, point))
                    .map(|hit| hit.pane_id.clone());
                if let Some(pane_id) = pane_id {
                    self.open_pane_context_menu(pane_id, mouse.column, mouse.row);
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp if super::contains(self.hits.agent_body, point) => {
                let next = self.agent_scroll.saturating_sub(1);
                if next != self.agent_scroll {
                    self.agent_scroll = next;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollDown if super::contains(self.hits.agent_body, point) => {
                let next = self
                    .agent_scroll
                    .saturating_add(1)
                    .min(self.hits.agent_max_scroll);
                if next != self.agent_scroll {
                    self.agent_scroll = next;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp if super::contains(self.hits.workspace_body, point) => {
                let next = self.workspace_scroll.saturating_sub(1);
                if next != self.workspace_scroll {
                    self.workspace_scroll = next;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollDown if super::contains(self.hits.workspace_body, point) => {
                let next = self
                    .workspace_scroll
                    .saturating_add(1)
                    .min(self.hits.workspace_max_scroll);
                if next != self.workspace_scroll {
                    self.workspace_scroll = next;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.selection.take().is_some() {
                    outcome.repaint = true;
                }
                self.selection_focus_pending = None;
                self.stop_selection_autoscroll();
                self.selection_highlight_clear_deadline = None;
                self.word_selection_gesture = None;
                let previous_pane_click = self.last_pane_click.take();
                self.workspace_press = None;
                // A width drag whose release never arrived (the button went up outside the
                // terminal) still owes the endpoint its resize.
                if let Some(ClientChromeDrag::SidebarWidth {
                    resize_pending: true,
                }) = self.chrome_drag.take()
                {
                    outcome.resize = true;
                }
                if super::contains(self.hits.sidebar_divider, point)
                    && !super::contains(self.hits.sidebar_toggle, point)
                {
                    let double_click = self.last_sidebar_divider_click.is_some_and(|last| {
                        now.duration_since(last) <= crate::limits::DOUBLE_CLICK_WINDOW
                    });
                    self.last_sidebar_divider_click = Some(now);
                    if double_click {
                        self.sidebar_width = self.config.sidebar_width;
                        self.sidebar_width_manual = false;
                        outcome.repaint = true;
                        outcome.resize = true;
                        self.persist_chrome_preferences(outcome);
                    } else {
                        self.chrome_drag = Some(ClientChromeDrag::SidebarWidth {
                            resize_pending: false,
                        });
                        self.set_sidebar_width_from_column(mouse.column, outcome);
                    }
                    return;
                }
                if super::contains(self.hits.sidebar_section_divider, point) {
                    self.chrome_drag = Some(ClientChromeDrag::SidebarSection);
                    self.set_sidebar_section_from_row(mouse.row, outcome);
                    return;
                }
                if super::contains(self.hits.workspace_scrollbar, point) {
                    if let Some(metrics) = self.hits.workspace_scroll_metrics {
                        if let Some(grab_row_offset) =
                            shepr_termio::scroll::scrollbar_thumb_grab_offset(
                                metrics,
                                self.hits.workspace_scrollbar,
                                mouse.row,
                            )
                        {
                            self.chrome_drag =
                                Some(ClientChromeDrag::WorkspaceScrollbar { grab_row_offset });
                        } else {
                            let offset = shepr_termio::scroll::scrollbar_offset_from_row(
                                metrics,
                                self.hits.workspace_scrollbar,
                                mouse.row,
                            );
                            let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                            if next != self.workspace_scroll {
                                self.workspace_scroll = next;
                                outcome.repaint = true;
                            }
                        }
                    }
                    return;
                }
                if super::contains(self.hits.agent_scrollbar, point) {
                    if let Some(metrics) = self.hits.agent_scroll_metrics {
                        if let Some(grab_row_offset) =
                            shepr_termio::scroll::scrollbar_thumb_grab_offset(
                                metrics,
                                self.hits.agent_scrollbar,
                                mouse.row,
                            )
                        {
                            self.chrome_drag =
                                Some(ClientChromeDrag::AgentScrollbar { grab_row_offset });
                        } else {
                            let offset = shepr_termio::scroll::scrollbar_offset_from_row(
                                metrics,
                                self.hits.agent_scrollbar,
                                mouse.row,
                            );
                            let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                            if next != self.agent_scroll {
                                self.agent_scroll = next;
                                outcome.repaint = true;
                            }
                        }
                    }
                    return;
                }
                if super::contains(self.hits.agent_sort_toggle, point) {
                    let sort = match self.config.agent_panel_sort {
                        shepr_config::AgentPanelSortConfig::Spaces => {
                            shepr_config::AgentPanelSortConfig::Priority
                        }
                        shepr_config::AgentPanelSortConfig::Priority => {
                            shepr_config::AgentPanelSortConfig::Spaces
                        }
                    };
                    self.config.agent_panel_sort = sort;
                    self.agent_panel_sort_manual = true;
                    self.agent_scroll = 0;
                    self.persist_chrome_preferences(outcome);
                    outcome.repaint = true;
                    return;
                }
                if self.handle_endpoint_machine_click(point, outcome) {
                    return;
                }
                if super::contains(self.hits.global_launcher, point) {
                    self.toggle_global_menu();
                    outcome.repaint = true;
                    return;
                }
                if super::contains(self.hits.new_workspace, point) {
                    self.record_binding(
                        &shepr_termio::input::KeybindMatch::Action(
                            shepr_termio::input::KeybindAction::NewWorkspace,
                        ),
                        outcome,
                    );
                    return;
                }
                if super::contains(self.hits.sidebar_toggle, point) {
                    self.sidebar_collapsed = !self.sidebar_collapsed;
                    self.sidebar_collapsed_manual = true;
                    outcome.repaint = true;
                    outcome.resize = true;
                    self.persist_chrome_preferences(outcome);
                    return;
                }
                let workspace_press = self
                    .hits
                    .workspaces
                    .iter()
                    .find(|hit| super::contains(hit.rect, point))
                    .map(|hit| ClientWorkspacePress {
                        endpoint_id: hit.endpoint_id.clone(),
                        workspace_id: hit.workspace_id.clone(),
                        start_column: mouse.column,
                        start_row: mouse.row,
                    });
                if let Some(workspace_press) = workspace_press {
                    self.workspace_press = Some(workspace_press);
                    return;
                }
                if self.handle_endpoint_agent_click(point, outcome) {
                    return;
                }
                let agent_pane_id = self
                    .hits
                    .agents
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, pane_id)| pane_id.clone());
                if let Some(pane_id) = agent_pane_id {
                    self.push_endpoint_command(
                        shepr_protocol::command::EndpointCommand::PaneFocus(
                            shepr_protocol::command::PaneTarget {
                                pane_id: pane_id.clone(),
                            },
                        ),
                        outcome,
                    );
                    return;
                }
                let scrollbar_hit = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| {
                        hit.scrollbar_rect
                            .is_some_and(|rect| super::contains(rect, point))
                            && hit
                                .scroll
                                .is_some_and(|metrics| metrics.max_offset_from_bottom > 0)
                    })
                    .cloned();
                if let Some(hit) = scrollbar_hit {
                    self.mode = ClientShellMode::Terminal;
                    self.push_endpoint_command(
                        shepr_protocol::command::EndpointCommand::PaneFocus(
                            shepr_protocol::command::PaneTarget {
                                pane_id: hit.pane_id.clone(),
                            },
                        ),
                        outcome,
                    );
                    let (Some(track), Some(metrics)) = (hit.scrollbar_rect, hit.scroll) else {
                        return;
                    };
                    if let Some(grab_row_offset) =
                        shepr_termio::scroll::scrollbar_thumb_grab_offset(metrics, track, mouse.row)
                    {
                        self.chrome_drag = Some(ClientChromeDrag::PaneScrollbar {
                            hit,
                            grab_row_offset,
                            last_sent_offset: None,
                            throttle: Throttle::new(crate::limits::MOUSE_DRAG_SEND_INTERVAL),
                        });
                    } else if let Some(offset) = Self::pane_scrollbar_offset(&hit, mouse.row, None)
                    {
                        self.push_pane_scroll_offset(hit.pane_id, offset, outcome);
                    }
                    return;
                }
                let split_hit = self
                    .hits
                    .pane_splits
                    .iter()
                    .find(|hit| super::contains(hit.hit_rect, point))
                    .cloned();
                if let Some(hit) = split_hit {
                    let Some(workspace_id) = self
                        .snapshot
                        .as_deref()
                        .and_then(|snapshot| snapshot.focused_workspace_id.clone())
                    else {
                        return;
                    };
                    let pointer = match hit.direction {
                        shepr_protocol::PaneSurfaceSplitDirection::Horizontal => mouse.column,
                        shepr_protocol::PaneSurfaceSplitDirection::Vertical => mouse.row,
                    };
                    self.chrome_drag = Some(ClientChromeDrag::PaneSplit {
                        grab_offset: i32::from(hit.pos) - i32::from(pointer),
                        last_sent_ratio: None,
                        throttle: Throttle::new(crate::limits::MOUSE_DRAG_SEND_INTERVAL),
                        hit,
                        workspace_id,
                    });
                    return;
                }
                let pane_hit = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.rect, point))
                    .cloned();
                if let Some(hit) = pane_hit {
                    if hit.mouse_reporting && super::contains(hit.inner_rect, point) {
                        self.push_pane_mouse_event(
                            &hit,
                            mouse,
                            mouse.modifiers,
                            outcome,
                            accounting,
                        );
                        self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                            last_position: self.pane_mouse_position(&hit, mouse),
                            hit: hit.clone(),
                            button: MouseButton::Left,
                            stripped_modifiers: crossterm::event::KeyModifiers::empty(),
                            last_event: mouse,
                        });
                    } else if super::contains(hit.inner_rect, point) {
                        let click = ClientPaneClick {
                            pane_id: hit.pane_id.clone(),
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
                                    self.last_pane_click = Some(click);
                                }
                                self.selection_focus_pending = (self.focused_pane_id().as_deref()
                                    != Some(hit.pane_id.as_str()))
                                .then(|| hit.pane_id.clone());
                                let (viewport_row, col) =
                                    selection_cell(mouse.column, mouse.row, hit.inner_rect);
                                let absolute_row = metrics.absolute_row_at_viewport(viewport_row);
                                self.selection = Some(shepr_vt::selection::Selection::anchor(
                                    hit.pane_id.clone(),
                                    shepr_vt::Point::new(absolute_row, col),
                                ));
                            }
                        } else {
                            // Selections hold absolute rows. Without the scroll
                            // origin this click cannot be anchored in them; it
                            // only clears the old selection.
                            self.selection = None;
                        }
                    }
                    self.push_endpoint_command(
                        shepr_protocol::command::EndpointCommand::PaneFocus(
                            shepr_protocol::command::PaneTarget {
                                pane_id: hit.pane_id.clone(),
                            },
                        ),
                        outcome,
                    );
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                if let Some(hit) = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.inner_rect, point) && hit.mouse_reporting)
                    .cloned()
                {
                    self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome, accounting);
                    self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                        last_position: self.pane_mouse_position(&hit, mouse),
                        hit,
                        button: MouseButton::Middle,
                        stripped_modifiers: crossterm::event::KeyModifiers::empty(),
                        last_event: mouse,
                    });
                }
            }
            MouseEventKind::Moved => {
                if let Some(hit) = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.inner_rect, point) && hit.mouse_reporting)
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
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.inner_rect, point))
                    .cloned()
                {
                    if self.focused_pane_id().as_deref() != Some(hit.pane_id.as_str()) {
                        self.push_endpoint_command(
                            shepr_protocol::command::EndpointCommand::PaneFocus(
                                shepr_protocol::command::PaneTarget {
                                    pane_id: hit.pane_id.clone(),
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
        let cell = ClientMousePosition::Cell {
            column: mouse.column.saturating_sub(hit.inner_rect.x),
            row: mouse.row.saturating_sub(hit.inner_rect.y),
        };
        if hit.sgr_pixel_mouse && hit.pixel_width > 0 && hit.pixel_height > 0 {
            self.host_mouse_pixels
                .and_then(|pixels| {
                    pixels
                        .pane_position(hit.inner_rect, hit.pixel_width, hit.pixel_height)
                        .and_then(|position| match position {
                            shepr_termio::input::mouse::Position::Pixels { x, y } => {
                                Some(ClientMousePosition::Pixels {
                                    x,
                                    y,
                                    column: mouse.column.saturating_sub(hit.inner_rect.x),
                                    row: mouse.row.saturating_sub(hit.inner_rect.y),
                                })
                            }
                            shepr_termio::input::mouse::Position::Cell { .. } => None,
                        })
                })
                .unwrap_or(cell)
        } else {
            cell
        }
    }

    pub(super) fn push_pane_mouse_event(
        &self,
        hit: &PaneHit,
        mouse: MouseEvent,
        modifiers: crossterm::event::KeyModifiers,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        let kind = shepr_protocol::ClientMouseKind::from_host(mouse.kind);
        let position = self.pane_mouse_position(hit, mouse);
        let geometry = matches!(position, ClientMousePosition::Pixels { .. }).then_some(
            shepr_protocol::ClientMouseGeometry {
                cols: hit.inner_rect.width,
                rows: hit.inner_rect.height,
                width_px: hit.pixel_width,
                height_px: hit.pixel_height,
            },
        );
        push_target_event(
            hit.pane_id.clone(),
            ClientPaneInputEvent::Mouse {
                kind,
                position,
                geometry,
                modifiers: shepr_protocol::WireModifiers::from_host(modifiers),
                lines: self.config.mouse_scroll_lines,
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
    pub(super) fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        now: Instant,
        outcome: &mut ClientShellInput,
    ) {
        let mut accounting = PaneInputBatchAccounting::default();
        self.handle_mouse_with_accounting(mouse, now, outcome, &mut accounting);
    }
}
