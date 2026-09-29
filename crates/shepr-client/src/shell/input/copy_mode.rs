//! Client copy mode. Search queries are typed text and the queued keys are
//! raw input; input content must stay out of logs and error messages here
//! (log lengths or content-free kinds instead).

use super::*;
use crossterm::event::{KeyCode, KeyModifiers};

/// Copy-mode rows are absolute: output that evicts history does not move
/// the line the cursor, a selection anchor or a search match names. The
/// viewport is still addressed by scroll offsets, so these convert.
impl ClientCopyModeState {
    /// The row at the top of the pane's viewport.
    pub(super) fn viewport_top(&self) -> shepr_vt::AbsRow {
        let from_origin = self
            .max_offset_from_bottom
            .saturating_sub(self.offset_from_bottom);
        self.history_origin
            .saturating_add(u64::try_from(from_origin).unwrap_or(u64::MAX))
    }

    /// The newest row the pane retains.
    fn last_row(&self) -> shepr_vt::AbsRow {
        let rows = self
            .max_offset_from_bottom
            .saturating_add(usize::from(self.geometry.1.max(1)))
            .saturating_sub(1);
        self.history_origin
            .saturating_add(u64::try_from(rows).unwrap_or(u64::MAX))
    }

    /// `row` clamped to the rows the pane retains.
    fn retained_row(&self, row: shepr_vt::AbsRow) -> shepr_vt::AbsRow {
        row.clamp(self.history_origin, self.last_row())
    }

    /// The scroll offset that puts `top` at the top of the viewport.
    fn offset_for_top(&self, top: shepr_vt::AbsRow) -> usize {
        let from_origin = top.0.saturating_sub(self.history_origin.0);
        self.max_offset_from_bottom
            .saturating_sub(usize::try_from(from_origin).unwrap_or(usize::MAX))
    }
}

impl ClientShellState {
    pub(super) fn reset_copy_pipeline(&mut self) {
        self.copy_session_generation = self.copy_session_generation.saturating_add(1);
        self.copy_operation_in_flight = false;
        self.copy_operation_queue.clear();
        self.copy_input_queue.clear();
    }

    pub(super) fn enter_copy_mode(&mut self, outcome: &mut ClientShellInput) -> bool {
        let pane_id = match self.focused_pane_id() {
            Some(pane_id) => pane_id,
            None => return false,
        };
        if self
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.pane_id == pane_id)
        {
            self.mode = ClientShellMode::Copy;
            return true;
        }
        if self.copy_mode.is_some() {
            self.exit_copy_mode(false, outcome);
        }
        let Some(hit) = self
            .hits
            .panes
            .iter()
            .find(|hit| hit.pane_id == pane_id)
            .cloned()
        else {
            return false;
        };
        let Some(metrics) = hit.scroll else {
            return false;
        };
        let cursor = self
            .pane_surface
            .as_ref()
            .and_then(|surface| {
                let pane = surface.panes.iter().find(|pane| pane.pane_id == pane_id)?;
                let cursor = surface
                    .frame
                    .cursor
                    .as_ref()
                    .filter(|cursor| cursor.visible)?;
                let inner = pane.inner_rect;
                (cursor.x >= inner.x
                    && cursor.x < inner.x.saturating_add(inner.width)
                    && cursor.y >= inner.y
                    && cursor.y < inner.y.saturating_add(inner.height))
                // Lazily: outside the pane the subtractions would underflow.
                .then(|| shepr_protocol::command::PaneTextPoint {
                    row: metrics
                        .absolute_row_at_viewport(shepr_vt::ViewportRow(cursor.y - inner.y)),
                    col: cursor.x - inner.x,
                })
            })
            .unwrap_or(shepr_protocol::command::PaneTextPoint {
                row: metrics.absolute_row_at_viewport(shepr_vt::ViewportRow(
                    hit.inner_rect.height.saturating_sub(1),
                )),
                col: 0,
            });
        self.selection = None;
        self.stop_selection_autoscroll();
        self.selection_highlight_clear_deadline = None;
        self.reset_copy_pipeline();
        let alternate_screen_active = self
            .pane_surface
            .as_ref()
            .and_then(|surface| surface.panes.iter().find(|pane| pane.pane_id == pane_id))
            .is_some_and(|pane| pane.alternate_screen_active);
        self.copy_mode = Some(ClientCopyModeState {
            pane_id,
            geometry: (hit.inner_rect.width, hit.inner_rect.height),
            alternate_screen_active,
            cursor,
            history_origin: metrics.history_origin,
            offset_from_bottom: metrics.offset_from_bottom,
            max_offset_from_bottom: metrics.max_offset_from_bottom,
            entry_offset_from_bottom: metrics.offset_from_bottom,
            selection: None,
            search_prompt: None,
            search_query: String::new(),
            search_direction: None,
            search_matches: Vec::new(),
            search_total: 0,
            search_current: None,
            search_current_global: None,
            search_generation: 0,
            copy_after_search: false,
        });
        self.mode = ClientShellMode::Copy;
        true
    }

    pub(super) fn route_copy_mode_key(
        &mut self,
        key: &shepr_termio::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if self.route_copy_search_prompt_key(key, outcome) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                let should_clear = self.copy_mode.as_ref().is_some_and(|copy_mode| {
                    copy_mode.selection.is_some()
                        || !copy_mode.search_query.is_empty()
                        || !copy_mode.search_matches.is_empty()
                        || copy_mode.search_direction.is_some()
                });
                if should_clear {
                    if let Some(copy_mode) = self.copy_mode.as_mut() {
                        copy_mode.selection = None;
                        copy_mode.search_query.clear();
                        copy_mode.search_direction = None;
                        copy_mode.search_matches.clear();
                        copy_mode.search_total = 0;
                        copy_mode.search_current = None;
                        copy_mode.search_current_global = None;
                        copy_mode.search_generation = copy_mode.search_generation.saturating_add(1);
                        copy_mode.copy_after_search = false;
                    }
                    self.selection = None;
                } else {
                    self.exit_copy_mode(false, outcome);
                }
                outcome.repaint = true;
                return;
            }
            KeyCode::Enter => {
                if !self.defer_copy_until_search_result() {
                    self.exit_copy_mode(true, outcome);
                }
                return;
            }
            KeyCode::Left => {
                self.move_copy_cursor(0, -1, outcome);
                return;
            }
            KeyCode::Down => {
                self.move_copy_cursor(1, 0, outcome);
                return;
            }
            KeyCode::Up => {
                self.move_copy_cursor(-1, 0, outcome);
                return;
            }
            KeyCode::Right => {
                self.move_copy_cursor(0, 1, outcome);
                return;
            }
            KeyCode::PageUp => {
                self.move_copy_page(-1, false, outcome);
                return;
            }
            KeyCode::PageDown => {
                self.move_copy_page(1, false, outcome);
                return;
            }
            KeyCode::Home => {
                self.set_copy_cursor_col(0);
                self.sync_copy_selection();
                outcome.repaint = true;
                return;
            }
            KeyCode::End => {
                self.request_copy_motion(shepr_protocol::command::PaneCopyMotion::LineEnd, outcome);
                return;
            }
            _ => {}
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('b'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_copy_page(-1, false, outcome);
                return;
            }
            (KeyCode::Char('f'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_copy_page(1, false, outcome);
                return;
            }
            (KeyCode::Char('u'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_copy_page(-1, true, outcome);
                return;
            }
            (KeyCode::Char('d'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_copy_page(1, true, outcome);
                return;
            }
            _ => {}
        }

        let Some(command) = shepr_termio::copy_mode::copy_mode_command_char(key) else {
            return;
        };
        match command {
            'q' => self.exit_copy_mode(false, outcome),
            'y' => {
                if !self.defer_copy_until_search_result() {
                    self.exit_copy_mode(true, outcome);
                }
            }
            'v' | ' ' => self.begin_copy_selection(false),
            'V' => self.begin_copy_selection(true),
            'h' => self.move_copy_cursor(0, -1, outcome),
            'j' => self.move_copy_cursor(1, 0, outcome),
            'k' => self.move_copy_cursor(-1, 0, outcome),
            'l' => self.move_copy_cursor(0, 1, outcome),
            'g' => self.move_copy_history(true, outcome),
            'G' => self.move_copy_history(false, outcome),
            '0' => {
                self.set_copy_cursor_col(0);
                self.sync_copy_selection();
                outcome.repaint = true;
            }
            '$' => {
                self.request_copy_motion(shepr_protocol::command::PaneCopyMotion::LineEnd, outcome)
            }
            '^' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::FirstNonBlank,
                    outcome,
                );
            }
            '/' => self.open_copy_search(shepr_protocol::command::PaneCopySearchDirection::Forward),
            '?' => {
                self.open_copy_search(shepr_protocol::command::PaneCopySearchDirection::Backward)
            }
            'n' => self.repeat_copy_search(false, outcome),
            'N' => self.repeat_copy_search(true, outcome),
            'w' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::NextWordStart,
                    outcome,
                );
            }
            'b' => self.request_copy_motion(
                shepr_protocol::command::PaneCopyMotion::PreviousWordStart,
                outcome,
            ),
            'e' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::NextWordEnd,
                    outcome,
                );
            }
            'W' => self.request_copy_motion(
                shepr_protocol::command::PaneCopyMotion::NextBigWordStart,
                outcome,
            ),
            'B' => self.request_copy_motion(
                shepr_protocol::command::PaneCopyMotion::PreviousBigWordStart,
                outcome,
            ),
            'E' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::NextBigWordEnd,
                    outcome,
                );
            }
            '{' => self.request_copy_motion(
                shepr_protocol::command::PaneCopyMotion::PreviousParagraph,
                outcome,
            ),
            '}' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::NextParagraph,
                    outcome,
                );
            }
            _ => return,
        }
        outcome.repaint = true;
    }

    fn route_copy_search_prompt_key(
        &mut self,
        key: &shepr_termio::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(prompt) = self
            .copy_mode
            .as_ref()
            .and_then(|copy_mode| copy_mode.search_prompt.as_ref())
        else {
            return false;
        };
        let mut submit = None;
        match key.code {
            KeyCode::Esc => {
                if let Some(copy_mode) = self.copy_mode.as_mut() {
                    copy_mode.search_prompt = None;
                }
            }
            KeyCode::Enter => {
                submit = Some((prompt.query.to_string(), prompt.direction));
                if let Some(copy_mode) = self.copy_mode.as_mut() {
                    copy_mode.search_prompt = None;
                }
            }
            _ => {
                if let Some(prompt) = self
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.search_prompt.as_mut())
                {
                    prompt.query.handle_key(key);
                }
            }
        }
        if let Some((query, direction)) = submit {
            self.request_copy_search(query, direction, false, outcome);
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn insert_copy_search_text(&mut self, text: &str) -> bool {
        if self.mode != ClientShellMode::Copy || self.overlay.is_some() {
            return false;
        }
        let Some(prompt) = self
            .copy_mode
            .as_mut()
            .and_then(|copy_mode| copy_mode.search_prompt.as_mut())
        else {
            return false;
        };
        prompt.query.insert(text);
        true
    }

    fn open_copy_search(&mut self, direction: shepr_protocol::command::PaneCopySearchDirection) {
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return;
        };
        copy_mode.search_prompt = Some(ClientCopySearchPrompt {
            direction,
            query: TextEditor::default(),
        });
    }

    fn repeat_copy_search(&mut self, reverse: bool, outcome: &mut ClientShellInput) {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return;
        };
        if copy_mode.search_query.is_empty() {
            return;
        }
        let Some(direction) = copy_mode.search_direction else {
            return;
        };
        let direction = if reverse {
            match direction {
                shepr_protocol::command::PaneCopySearchDirection::Forward => {
                    shepr_protocol::command::PaneCopySearchDirection::Backward
                }
                shepr_protocol::command::PaneCopySearchDirection::Backward => {
                    shepr_protocol::command::PaneCopySearchDirection::Forward
                }
            }
        } else {
            direction
        };
        self.request_copy_search(copy_mode.search_query.clone(), direction, true, outcome);
    }

    fn defer_copy_until_search_result(&mut self) -> bool {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return false;
        };
        let pane_id = copy_mode.pane_id.clone();
        let generation = copy_mode.search_generation;
        let pending = self.pending_requests.values().any(|pending| {
            matches!(
                &pending.kind,
                PendingEndpointKind::CopySearch {
                    pane_id: pending_pane,
                    generation: pending_generation,
                    ..
                } if pending_pane == &pane_id && *pending_generation == generation
            )
        }) || self
            .copy_operation_queue
            .iter()
            .any(|operation| matches!(operation, ClientCopyOperation::Search { .. }));
        if pending && let Some(copy_mode) = self.copy_mode.as_mut() {
            copy_mode.copy_after_search = true;
        }
        pending
    }

    fn request_copy_search(
        &mut self,
        query: String,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        outcome: &mut ClientShellInput,
    ) {
        if query.is_empty() || self.copy_mode.is_none() {
            return;
        }
        self.copy_operation_queue
            .push_back(ClientCopyOperation::Search {
                query,
                direction,
                repeat,
            });
        self.dispatch_next_copy_operation(outcome);
    }

    pub(super) fn apply_copy_search_result(
        &mut self,
        pane_id: &str,
        origin: shepr_protocol::command::PaneTextPoint,
        query: String,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        generation: u64,
        result: ClientCopySearchResult,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let search_queued = self
            .copy_operation_queue
            .iter()
            .any(|operation| matches!(operation, ClientCopyOperation::Search { .. }));
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return false;
        };
        if copy_mode.pane_id != pane_id
            || copy_mode.cursor != origin
            || copy_mode.search_generation != generation
        {
            return false;
        }
        let current = result.current.filter(|index| *index < result.matches.len());
        copy_mode.search_query = query;
        if !repeat {
            copy_mode.search_direction = Some(direction);
        }
        copy_mode.search_matches = result.matches;
        copy_mode.search_total = result.total;
        copy_mode.search_current = current;
        copy_mode.search_current_global = result.current_global;
        let target = current.and_then(|index| copy_mode.search_matches.get(index).copied());
        let copy_after_search = if search_queued {
            false
        } else {
            std::mem::take(&mut copy_mode.copy_after_search)
        };
        if let Some(target) = target {
            copy_mode.cursor = target.start;
            self.reveal_copy_cursor(outcome);
            self.sync_copy_selection();
        }
        if copy_after_search {
            self.exit_copy_mode(true, outcome);
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn complete_copy_operation(
        &mut self,
        session_generation: u64,
        continue_queue: bool,
        now: std::time::Instant,
        outcome: &mut ClientShellInput,
    ) {
        if self.copy_session_generation != session_generation {
            return;
        }
        self.copy_operation_in_flight = false;
        if continue_queue && self.copy_mode.is_some() {
            self.dispatch_next_copy_operation(outcome);
            self.dispatch_queued_copy_input(now, outcome);
        } else {
            self.copy_operation_queue.clear();
            self.copy_input_queue.clear();
        }
    }

    fn dispatch_queued_copy_input(
        &mut self,
        now: std::time::Instant,
        outcome: &mut ClientShellInput,
    ) {
        while !self.copy_operation_in_flight {
            let Some(key) = self.copy_input_queue.pop_front() else {
                return;
            };
            // A replayed `q`, `y` or Enter leaves copy mode, and leaving clears
            // the queue (`reset_copy_pipeline`). The keys behind it were typed
            // after that exit and belong to the pane, so hold them aside and
            // put them back: they then route in whatever mode the key left.
            let mut later = std::mem::take(&mut self.copy_input_queue);
            self.handle_key(key, now, outcome);
            later.extend(self.copy_input_queue.drain(..));
            self.copy_input_queue = later;
        }
    }

    pub(super) fn cancel_deferred_copy_after_search(&mut self, generation: u64) {
        if let Some(copy_mode) = self
            .copy_mode
            .as_mut()
            .filter(|copy_mode| copy_mode.search_generation == generation)
        {
            copy_mode.copy_after_search = false;
        }
    }

    fn copy_hit(&self) -> Option<PaneHit> {
        let pane_id = self.copy_mode.as_ref()?.pane_id.as_str();
        self.hits
            .panes
            .iter()
            .find(|hit| hit.pane_id == pane_id)
            .cloned()
    }

    fn move_copy_cursor(&mut self, row_delta: i16, col_delta: i16, outcome: &mut ClientShellInput) {
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return;
        };
        let width = copy_mode.geometry.0;
        if col_delta < 0 {
            copy_mode.cursor.col = copy_mode
                .cursor
                .col
                .saturating_sub(col_delta.unsigned_abs());
        } else if col_delta > 0 {
            copy_mode.cursor.col = copy_mode
                .cursor
                .col
                .saturating_add(col_delta.unsigned_abs())
                .min(width.saturating_sub(1));
        }
        let rows = u64::from(row_delta.unsigned_abs());
        if row_delta < 0 {
            copy_mode.cursor.row =
                copy_mode.retained_row(copy_mode.cursor.row.saturating_sub(rows));
        } else if row_delta > 0 {
            copy_mode.cursor.row =
                copy_mode.retained_row(copy_mode.cursor.row.saturating_add(rows));
        }
        self.reveal_copy_cursor(outcome);
        self.sync_copy_selection();
        outcome.repaint = true;
    }

    fn move_copy_page(&mut self, direction: i8, half_page: bool, outcome: &mut ClientShellInput) {
        let Some(hit) = self.copy_hit() else {
            return;
        };
        let lines = shepr_termio::copy_mode::copy_mode_page_lines(hit.inner_rect.height, half_page);
        let Some((pane_id, next_offset)) = self.copy_mode.as_mut().map(|copy_mode| {
            let rows = u64::try_from(lines).unwrap_or(u64::MAX);
            if direction < 0 {
                copy_mode.cursor.row =
                    copy_mode.retained_row(copy_mode.cursor.row.saturating_sub(rows));
                copy_mode.offset_from_bottom = copy_mode
                    .offset_from_bottom
                    .saturating_add(lines)
                    .min(copy_mode.max_offset_from_bottom);
            } else {
                copy_mode.cursor.row =
                    copy_mode.retained_row(copy_mode.cursor.row.saturating_add(rows));
                copy_mode.offset_from_bottom = copy_mode.offset_from_bottom.saturating_sub(lines);
            }
            (copy_mode.pane_id.clone(), copy_mode.offset_from_bottom)
        }) else {
            return;
        };
        self.push_pane_scroll_offset(pane_id, next_offset, outcome);
        self.sync_copy_selection();
        outcome.repaint = true;
    }

    fn move_copy_history(&mut self, top: bool, outcome: &mut ClientShellInput) {
        if self.copy_hit().is_none() {
            return;
        }
        let Some((pane_id, offset_from_bottom)) = self.copy_mode.as_mut().map(|copy_mode| {
            if top {
                copy_mode.cursor.row = copy_mode.history_origin;
                copy_mode.offset_from_bottom = copy_mode.max_offset_from_bottom;
            } else {
                copy_mode.cursor.row = copy_mode.last_row();
                copy_mode.offset_from_bottom = 0;
            }
            (copy_mode.pane_id.clone(), copy_mode.offset_from_bottom)
        }) else {
            return;
        };
        self.push_pane_scroll_offset(pane_id, offset_from_bottom, outcome);
        self.sync_copy_selection();
        outcome.repaint = true;
    }

    fn set_copy_cursor_col(&mut self, col: u16) {
        if let Some(copy_mode) = self.copy_mode.as_mut() {
            copy_mode.cursor.col = col;
        }
    }

    /// Whether the copy-mode bar is drawn over the copy pane's bottom row: true with the tab
    /// bar on top or hidden (the bar then takes the pane area's bottom row) when the pane
    /// reaches that row. Unknown geometry counts as covered.
    fn mode_bar_covers_copy_pane(&self) -> bool {
        let (Some(hit), Some((cols, rows))) = (self.copy_hit(), self.last_composed_size) else {
            return true;
        };
        let layout = self.layout(cols, rows);
        if self.config.tab_bar_position == TabBarPositionConfig::Bottom
            && !layout.tab_bar.is_empty()
        {
            return false;
        }
        hit.inner_rect.bottom() >= layout.pane_surface.bottom()
    }

    /// Scrolls the copy pane so the cursor is on screen, keeping it off the row the mode bar
    /// covers. Motions and search results both go through here. On the very last line of
    /// history no scroll can lift it; `compose` then moves the bar to the top row instead.
    fn reveal_copy_cursor(&mut self, outcome: &mut ClientShellInput) {
        let reserve_mode_bar_row = self.mode_bar_covers_copy_pane();
        let request = self.copy_mode.as_mut().and_then(|copy_mode| {
            let current_top = copy_mode.viewport_top();
            let max_cursor_row = u64::from(
                copy_mode
                    .geometry
                    .1
                    .saturating_sub(if reserve_mode_bar_row { 2 } else { 1 }),
            );
            let bottom = current_top.saturating_add(max_cursor_row);
            let desired_top = if copy_mode.cursor.row < current_top {
                copy_mode.cursor.row
            } else if copy_mode.cursor.row > bottom {
                copy_mode.cursor.row.saturating_sub(max_cursor_row)
            } else {
                current_top
            };
            let offset = copy_mode.offset_for_top(desired_top);
            if offset == copy_mode.offset_from_bottom {
                return None;
            }
            copy_mode.offset_from_bottom = offset;
            Some((copy_mode.pane_id.clone(), offset))
        });
        if let Some((pane_id, offset)) = request {
            self.push_pane_scroll_offset(pane_id, offset, outcome);
        }
    }

    fn begin_copy_selection(&mut self, linewise: bool) {
        let width = self.copy_hit().map(|hit| hit.inner_rect.width);
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return;
        };
        let end_col = width.unwrap_or(copy_mode.geometry.0).saturating_sub(1);
        let row = copy_mode.cursor.row;
        if linewise {
            copy_mode.selection = Some(ClientCopySelection::Linewise { anchor_row: row });
            self.selection = Some(shepr_vt::selection::Selection::line_range(
                copy_mode.pane_id.clone(),
                row,
                row,
                end_col,
            ));
        } else {
            copy_mode.selection = Some(ClientCopySelection::Character {
                anchor: shepr_vt::Point::new(row, copy_mode.cursor.col),
            });
            self.selection = Some(shepr_vt::selection::Selection::anchor(
                copy_mode.pane_id.clone(),
                shepr_vt::Point::new(row, copy_mode.cursor.col),
            ));
        }
    }

    pub(super) fn sync_copy_selection(&mut self) {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return;
        };
        let Some(selection) = copy_mode.selection else {
            return;
        };
        self.selection = Some(match selection {
            ClientCopySelection::Character { anchor } => shepr_vt::selection::Selection::range(
                copy_mode.pane_id.clone(),
                anchor,
                shepr_vt::Point::new(copy_mode.cursor.row, copy_mode.cursor.col),
            ),
            ClientCopySelection::Linewise { anchor_row } => {
                shepr_vt::selection::Selection::line_range(
                    copy_mode.pane_id.clone(),
                    anchor_row,
                    copy_mode.cursor.row,
                    self.copy_hit()
                        .map_or(copy_mode.geometry.0, |hit| hit.inner_rect.width)
                        .saturating_sub(1),
                )
            }
        });
    }

    fn request_copy_motion(
        &mut self,
        motion: shepr_protocol::command::PaneCopyMotion,
        outcome: &mut ClientShellInput,
    ) {
        if self.copy_mode.is_none() {
            return;
        }
        self.copy_operation_queue
            .push_back(ClientCopyOperation::Motion(motion));
        self.dispatch_next_copy_operation(outcome);
    }

    pub(super) fn dispatch_next_copy_operation(&mut self, outcome: &mut ClientShellInput) {
        if self.copy_operation_in_flight {
            return;
        }
        while let Some(operation) = self.copy_operation_queue.pop_front() {
            let Some(copy_mode) = self.copy_mode.as_mut() else {
                self.copy_operation_queue.clear();
                self.copy_input_queue.clear();
                return;
            };
            let session_generation = self.copy_session_generation;
            let pane_id = copy_mode.pane_id.clone();
            let origin = copy_mode.cursor;
            let (command, kind) = match operation {
                ClientCopyOperation::Motion(motion) => (
                    shepr_protocol::command::EndpointCommand::PaneCopyMotion(
                        shepr_protocol::command::PaneCopyMotionParams {
                            pane_id: pane_id.to_string(),
                            cursor: origin,
                            motion,
                        },
                    ),
                    PendingEndpointKind::CopyMotion {
                        pane_id,
                        origin,
                        session_generation,
                    },
                ),
                ClientCopyOperation::Search {
                    query,
                    direction,
                    repeat,
                } => {
                    if query.is_empty() {
                        continue;
                    }
                    copy_mode.search_generation = copy_mode.search_generation.saturating_add(1);
                    let generation = copy_mode.search_generation;
                    let previous = repeat
                        .then(|| {
                            copy_mode
                                .search_current
                                .and_then(|index| copy_mode.search_matches.get(index).copied())
                                .filter(|text_match| text_match.start == copy_mode.cursor)
                        })
                        .flatten();
                    (
                        shepr_protocol::command::EndpointCommand::PaneCopySearch(
                            shepr_protocol::command::PaneCopySearchParams {
                                pane_id: pane_id.to_string(),
                                query: query.clone(),
                                direction,
                                cursor: origin,
                                previous,
                            },
                        ),
                        PendingEndpointKind::CopySearch {
                            pane_id,
                            origin,
                            query,
                            direction,
                            repeat,
                            generation,
                            session_generation,
                        },
                    )
                }
            };
            self.copy_operation_in_flight = true;
            if !self.push_endpoint_command_with_kind(command, kind, outcome) {
                self.copy_operation_in_flight = false;
            }
            return;
        }
    }

    pub(super) fn apply_copy_motion_target(
        &mut self,
        pane_id: &str,
        origin: shepr_protocol::command::PaneTextPoint,
        cursor: shepr_protocol::command::PaneTextPoint,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return false;
        };
        if copy_mode.pane_id != pane_id || copy_mode.cursor != origin {
            return false;
        }
        copy_mode.cursor = cursor;
        self.reveal_copy_cursor(outcome);
        self.sync_copy_selection();
        outcome.repaint = true;
        true
    }

    pub(super) fn exit_copy_mode(&mut self, copy: bool, outcome: &mut ClientShellInput) {
        let live_selection = self
            .selection
            .as_ref()
            .is_some_and(shepr_vt::selection::Selection::is_visible);
        if copy
            && !live_selection
            && let Some((pane_id, text_match)) = self.copy_mode.as_ref().and_then(|copy_mode| {
                copy_mode
                    .search_current
                    .and_then(|index| copy_mode.search_matches.get(index).copied())
                    .map(|text_match| (copy_mode.pane_id.clone(), text_match))
            })
        {
            self.selection = Some(shepr_vt::selection::Selection::range(
                pane_id,
                shepr_vt::Point::new(text_match.start.row, text_match.start.col),
                shepr_vt::Point::new(text_match.end.row, text_match.end.col),
            ));
        }
        let Some(copy_mode) = self.copy_mode.take() else {
            return;
        };
        self.reset_copy_pipeline();
        if copy
            && self
                .selection
                .as_ref()
                .is_some_and(shepr_vt::selection::Selection::is_visible)
        {
            self.request_selection_copy(outcome);
        }
        self.selection = None;
        self.selection_highlight_clear_deadline = None;
        self.push_pane_scroll_offset(
            copy_mode.pane_id,
            copy_mode.entry_offset_from_bottom,
            outcome,
        );
        self.mode = ClientShellMode::Terminal;
        outcome.repaint = true;
    }
}
