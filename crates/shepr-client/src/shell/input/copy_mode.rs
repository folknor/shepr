//! Client copy mode. Search queries are typed text and the queued keys are
//! raw input; input content must stay out of logs and error messages here
//! (log lengths or content-free kinds instead).

use crate::shell::ledger::Work;
use crate::shell::overlays::text_editor::TextEditor;
use crate::shell::state::{ClientCopySearch, ClientCopySelection, ClientShellMode};
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;

use crate::shell::input::events::PaneInputBatchAccounting;
use crate::shell::state::{
    ClientCopyModeState, ClientCopyOperation, ClientCopySearchPrompt, ClientCopySearchResult,
    ClientShellEndpointError, ClientShellInput, ClientShellState, PaneHit, TypedText,
};

use std::collections::VecDeque;

/// Copy-mode coordinates are absolute rows. Output leaves retained points in
/// place; surface installation clamps evicted cursor and selection rows and
/// prunes evicted search matches. The viewport is addressed by scroll offsets,
/// so these convert.
impl ClientCopyModeState {
    /// Whether this stored copy session's pane currently owns focus.
    /// The caller still decides whether a focused session is active input mode.
    pub(in crate::shell) fn pane_is_focused(&self, focused_pane_id: Option<&str>) -> bool {
        focused_pane_id == Some(self.pane_id.as_str())
    }

    /// The row at the top of the pane's viewport.
    pub(in crate::shell) fn viewport_top(&self) -> shepr_vt::AbsRow {
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
    pub(in crate::shell) fn retained_row(&self, row: shepr_vt::AbsRow) -> shepr_vt::AbsRow {
        row.clamp(self.history_origin, self.last_row())
    }

    /// The scroll offset that puts `top` at the top of the viewport.
    fn offset_for_top(&self, top: shepr_vt::AbsRow) -> usize {
        let from_origin = top.0.saturating_sub(self.history_origin.0);
        self.max_offset_from_bottom
            .saturating_sub(usize::try_from(from_origin).unwrap_or(usize::MAX))
    }
}

/// Operations only queue behind an awaiting request, except during dispatch. Keys
/// also exist while a completed request's input is replayed.
#[derive(Default)]
pub(in crate::shell) struct CopyPipeline {
    /// The one outstanding copy request. Only its answer applies; dropping it is what
    /// makes a late answer stale.
    awaiting: Option<shepr_protocol::RequestId>,
    ops: VecDeque<ClientCopyOperation>,
    keys: VecDeque<shepr_termio::input::TerminalKey>,
}
impl CopyPipeline {
    pub(in crate::shell) fn in_flight(&self) -> bool {
        self.awaiting.is_some()
    }
    pub(in crate::shell) fn is_awaiting(&self, id: &shepr_protocol::RequestId) -> bool {
        self.awaiting.as_ref() == Some(id)
    }
    pub(in crate::shell) fn awaiting(&self) -> Option<&shepr_protocol::RequestId> {
        self.awaiting.as_ref()
    }
    pub(in crate::shell) fn begin(&mut self, id: shepr_protocol::RequestId) {
        self.awaiting = Some(id);
    }
    pub(in crate::shell) fn finish(&mut self) {
        self.awaiting = None;
    }
    pub(in crate::shell) fn reset(&mut self) {
        self.awaiting = None;
        self.ops.clear();
        self.keys.clear();
    }
    pub(in crate::shell) fn push_op(&mut self, op: ClientCopyOperation) {
        self.ops.push_back(op);
    }
    pub(in crate::shell) fn pop_op(&mut self) -> Option<ClientCopyOperation> {
        self.ops.pop_front()
    }
    pub(in crate::shell) fn clear_ops(&mut self) {
        self.ops.clear();
    }
    pub(in crate::shell) fn has_queued_search(&self) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op, ClientCopyOperation::Search { .. }))
    }
    pub(in crate::shell) fn push_key(&mut self, key: shepr_termio::input::TerminalKey) {
        self.keys.push_back(key);
    }
    pub(in crate::shell) fn pop_key(&mut self) -> Option<shepr_termio::input::TerminalKey> {
        self.keys.pop_front()
    }
    pub(in crate::shell) fn keys_len(&self) -> usize {
        self.keys.len()
    }
    pub(in crate::shell) fn take_keys(&mut self) -> VecDeque<shepr_termio::input::TerminalKey> {
        std::mem::take(&mut self.keys)
    }
    pub(in crate::shell) fn put_keys(&mut self, keys: VecDeque<shepr_termio::input::TerminalKey>) {
        self.keys = keys;
    }
    pub(in crate::shell) fn clear_keys(&mut self) {
        self.keys.clear();
    }
}

impl ClientShellState {
    pub(in crate::shell) fn complete_copy_motion(
        &mut self,
        request: &shepr_protocol::RequestId,
        pane_id: &shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        result: Result<shepr_protocol::command::EndpointReply, ClientShellEndpointError>,
        now: std::time::Instant,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use shepr_protocol::command::EndpointReply;
        if !self.copy_pipeline.is_awaiting(request) {
            return false;
        }
        let (repaint, continue_queue) = match result {
            Ok(EndpointReply::PaneCopyMotion {
                pane_id: returned_pane_id,
                cursor,
            }) if &returned_pane_id == pane_id => (
                self.apply_copy_motion_target(pane_id, origin, cursor, outcome),
                true,
            ),
            Ok(EndpointReply::PaneCopyMotion { .. }) => (false, false),
            Ok(_) => {
                self.set_endpoint_error("endpoint returned an unexpected copy-motion result", now);
                (true, false)
            }
            Err(_) => (true, false),
        };
        // The apply can reset the pipeline (a search with `copy_after_search` exits copy
        // mode); then nothing queued behind this request may replay or dispatch.
        if self.copy_pipeline.is_awaiting(request) {
            self.finish_copy_operation(continue_queue, outcome);
        }
        repaint
    }
    pub(in crate::shell) fn complete_copy_search(
        &mut self,
        request: &shepr_protocol::RequestId,
        pane_id: &shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        generation: u64,
        result: Result<shepr_protocol::command::EndpointReply, ClientShellEndpointError>,
        now: std::time::Instant,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use shepr_protocol::command::EndpointReply;
        if !self.copy_pipeline.is_awaiting(request) {
            return false;
        }
        let (repaint, continue_queue) = match result {
            Ok(EndpointReply::PaneCopySearch {
                pane_id: returned_pane_id,
                matches,
                total,
                current,
                current_global,
            }) if &returned_pane_id == pane_id => {
                let repaint = self.apply_copy_search_result(
                    pane_id,
                    origin,
                    query,
                    direction,
                    repeat,
                    generation,
                    ClientCopySearchResult {
                        matches,
                        total,
                        current: current.and_then(|index| usize::try_from(index).ok()),
                        current_global,
                    },
                    outcome,
                );
                if !repaint {
                    self.cancel_deferred_copy_after_search(generation);
                }
                (repaint, repaint)
            }
            Ok(EndpointReply::PaneCopySearch { .. }) => {
                self.cancel_deferred_copy_after_search(generation);
                (false, false)
            }
            Ok(_) => {
                self.cancel_deferred_copy_after_search(generation);
                self.set_endpoint_error("endpoint returned an unexpected copy-search result", now);
                (true, false)
            }
            Err(_) => {
                self.cancel_deferred_copy_after_search(generation);
                (true, false)
            }
        };
        // The apply can reset the pipeline (a search with `copy_after_search` exits copy
        // mode); then nothing queued behind this request may replay or dispatch.
        if self.copy_pipeline.is_awaiting(request) {
            self.finish_copy_operation(continue_queue, outcome);
        }
        repaint
    }
    pub(in crate::shell) fn drop_copy_operation(
        &mut self,
        request: &shepr_protocol::RequestId,
    ) -> bool {
        if !self.copy_pipeline.is_awaiting(request) {
            return false;
        }
        // Buffered keys depend on a result that will never be applied. Discard them rather
        // than replaying exits, new motions or pane input into a frozen presentation.
        self.copy_pipeline.reset();
        if let Some(search) = self
            .copy_mode
            .as_mut()
            .and_then(|copy_mode| copy_mode.search.as_mut())
        {
            search.copy_after_result = false;
        }
        true
    }

    /// Copy accepts input only when its explicit mode is active, no overlay
    /// intercepts it, and the stored session still belongs to the focused pane.
    pub(in crate::shell) fn copy_mode_owns_input(&self) -> bool {
        self.mode == ClientShellMode::Copy
            && self.overlay.is_none()
            && self.copy_mode.as_ref().is_some_and(|copy_mode| {
                copy_mode.pane_is_focused(self.focused_pane_id().as_deref())
            })
    }

    /// Keys that leave copy mode or hand input to the prefix: Esc always, and outside
    /// the search prompt the prefix and `q`. While a copy operation is in flight they
    /// queue in order like every other key; they only act out of order when the queue
    /// is full, as the way out of a request that stopped answering.
    pub(in crate::shell) fn copy_mode_interrupt_key(
        &self,
        key: &shepr_termio::input::TerminalKey,
    ) -> bool {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return false;
        }
        if key.code == KeyCode::Esc {
            return true;
        }
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return false;
        };
        if copy_mode
            .search
            .as_ref()
            .is_some_and(|search| search.prompt.is_some())
        {
            return false;
        }
        shepr_config::terminal_key_matches_combo(key, self.config.keybinds.prefix)
            || shepr_termio::copy_mode::copy_mode_command_char(key) == Some('q')
    }

    /// Gives up on the in-flight copy operation and every key queued behind it. The
    /// request stays in the ledger; its answer, if one ever comes, belongs to an older
    /// copy session and is ignored.
    pub(in crate::shell) fn abandon_copy_operation(&mut self) {
        self.reset_copy_pipeline();
        if let Some(copy_mode) = self.copy_mode.as_mut()
            && let Some(search) = copy_mode.search.as_mut()
        {
            search.copy_after_result = false;
        }
    }

    pub(in crate::shell) fn reset_copy_pipeline(&mut self) {
        self.copy_pipeline.reset();
    }

    pub(in crate::shell) fn enter_copy_mode(&mut self, outcome: &mut ClientShellInput) -> bool {
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
            .pane_surface()
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
        self.mouse_selection.clear();
        self.reset_copy_pipeline();
        let alternate_screen_active = self
            .pane_surface()
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
            search: None,
            operation_generation: 0,
        });
        self.mode = ClientShellMode::Copy;
        true
    }

    pub(in crate::shell) fn route_copy_mode_key(
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
                        || copy_mode.search.as_ref().is_some_and(|search| {
                            !search.query.is_empty()
                                || !search.matches.is_empty()
                                || search.direction.is_some()
                        })
                });
                if should_clear {
                    if let Some(copy_mode) = self.copy_mode.as_mut() {
                        copy_mode.selection = None;
                        copy_mode.operation_generation =
                            copy_mode.operation_generation.saturating_add(1);
                        copy_mode.search = None;
                    }
                    self.mouse_selection.clear();
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
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Line(
                        shepr_protocol::command::PaneLineMotion::End,
                    ),
                    outcome,
                );
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
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Line(
                        shepr_protocol::command::PaneLineMotion::End,
                    ),
                    outcome,
                );
            }
            '^' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Line(
                        shepr_protocol::command::PaneLineMotion::FirstNonBlank,
                    ),
                    outcome,
                );
            }
            '/' => self.open_copy_search(shepr_protocol::command::PaneCopySearchDirection::Forward),
            '?' => {
                self.open_copy_search(shepr_protocol::command::PaneCopySearchDirection::Backward);
            }
            'n' => self.repeat_copy_search(false, outcome),
            'N' => self.repeat_copy_search(true, outcome),
            'w' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::NextStart,
                    ),
                    outcome,
                );
            }
            'b' => self.request_copy_motion(
                shepr_protocol::command::PaneCopyMotion::Word(
                    shepr_protocol::command::PaneWordMotion::PreviousStart,
                ),
                outcome,
            ),
            'e' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::NextEnd,
                    ),
                    outcome,
                );
            }
            'W' => self.request_copy_motion(
                shepr_protocol::command::PaneCopyMotion::Word(
                    shepr_protocol::command::PaneWordMotion::NextBigStart,
                ),
                outcome,
            ),
            'B' => self.request_copy_motion(
                shepr_protocol::command::PaneCopyMotion::Word(
                    shepr_protocol::command::PaneWordMotion::PreviousBigStart,
                ),
                outcome,
            ),
            'E' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::NextBigEnd,
                    ),
                    outcome,
                );
            }
            '{' => self.request_copy_motion(
                shepr_protocol::command::PaneCopyMotion::Paragraph(
                    shepr_protocol::command::PaneParagraphMotion::Previous,
                ),
                outcome,
            ),
            '}' => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Paragraph(
                        shepr_protocol::command::PaneParagraphMotion::Next,
                    ),
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
            .and_then(|copy_mode| copy_mode.search.as_ref())
            .and_then(|search| search.prompt.as_ref())
        else {
            return false;
        };
        let mut submit = None;
        match key.code {
            KeyCode::Esc => {
                if let Some(copy_mode) = self.copy_mode.as_mut() {
                    let discard_search = if let Some(search) = copy_mode.search.as_mut() {
                        search.prompt = None;
                        search.query.is_empty()
                            && search.direction.is_none()
                            && search.matches.is_empty()
                    } else {
                        false
                    };
                    if discard_search {
                        copy_mode.search = None;
                    }
                }
            }
            KeyCode::Enter => {
                submit = Some((prompt.query.to_string(), prompt.direction));
                if let Some(copy_mode) = self.copy_mode.as_mut()
                    && let Some(search) = copy_mode.search.as_mut()
                {
                    search.prompt = None;
                }
            }
            _ => {
                if let Some(prompt) = self
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.search.as_mut())
                    .and_then(|search| search.prompt.as_mut())
                {
                    prompt.query.handle_key(key);
                }
            }
        }
        if let Some((query, direction)) = submit {
            self.request_copy_search(query.into(), direction, false, outcome);
        }
        outcome.repaint = true;
        true
    }

    pub(in crate::shell) fn insert_copy_search_text(&mut self, text: &str) -> bool {
        if !self.copy_mode_owns_input() {
            return false;
        }
        let Some(prompt) = self
            .copy_mode
            .as_mut()
            .and_then(|copy_mode| copy_mode.search.as_mut())
            .and_then(|search| search.prompt.as_mut())
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
        copy_mode
            .search
            .get_or_insert_with(ClientCopySearch::default)
            .prompt = Some(ClientCopySearchPrompt {
            direction,
            query: TextEditor::default(),
        });
    }

    fn repeat_copy_search(&mut self, reverse: bool, outcome: &mut ClientShellInput) {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return;
        };
        let Some(search) = copy_mode.search.as_ref() else {
            return;
        };
        if search.query.is_empty() {
            return;
        }
        let Some(direction) = search.direction else {
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
        let query = search.query.clone();
        self.request_copy_search(query, direction, true, outcome);
    }

    fn defer_copy_until_search_result(&mut self) -> bool {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return false;
        };
        let pane_id = copy_mode.pane_id.clone();
        let generation = copy_mode.operation_generation;
        // Only the awaited request counts: a search an earlier session abandoned is still
        // in the ledger, but its answer will be ignored.
        let pending = self
            .copy_pipeline
            .awaiting()
            .and_then(|id| self.ledger.work(id))
            .is_some_and(|work| {
                matches!(
                    work,
                    Work::CopySearch {
                        pane_id: pending_pane,
                        generation: pending_generation,
                        ..
                    } if pending_pane == &pane_id && *pending_generation == generation
                )
            })
            || self.copy_pipeline.has_queued_search();
        if pending
            && let Some(search) = self
                .copy_mode
                .as_mut()
                .and_then(|copy_mode| copy_mode.search.as_mut())
        {
            search.copy_after_result = true;
        }
        pending
    }

    fn request_copy_search(
        &mut self,
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        outcome: &mut ClientShellInput,
    ) {
        if query.is_empty() || self.copy_mode.is_none() {
            return;
        }
        if let Some(copy_mode) = self.copy_mode.as_mut() {
            copy_mode
                .search
                .get_or_insert_with(ClientCopySearch::default);
        }
        self.copy_pipeline.push_op(ClientCopyOperation::Search {
            query,
            direction,
            repeat,
        });
        self.dispatch_next_copy_operation(outcome);
    }

    pub(in crate::shell) fn apply_copy_search_result(
        &mut self,
        pane_id: &str,
        origin: shepr_protocol::command::PaneTextPoint,
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        generation: u64,
        result: ClientCopySearchResult,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let search_queued = self.copy_pipeline.has_queued_search();
        let Some(copy_mode) = self.copy_mode.as_mut() else {
            return false;
        };
        if copy_mode.pane_id != pane_id
            || copy_mode.cursor != origin
            || copy_mode.operation_generation != generation
        {
            return false;
        }
        let Some(search) = copy_mode.search.as_mut() else {
            return false;
        };
        let current = result.current.filter(|index| *index < result.matches.len());
        search.query = query;
        if !repeat {
            search.direction = Some(direction);
        }
        search.matches = result.matches;
        search.total = result.total;
        search.current = current;
        search.current_global = result.current_global;
        let target = current.and_then(|index| search.matches.get(index).copied());
        let copy_after_search = if search_queued {
            false
        } else {
            std::mem::take(&mut search.copy_after_result)
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

    pub(in crate::shell) fn finish_copy_operation(
        &mut self,
        continue_queue: bool,
        outcome: &mut ClientShellInput,
    ) {
        self.copy_pipeline.finish();
        if continue_queue && self.copy_mode_owns_input() {
            self.dispatch_next_copy_operation(outcome);
            let mut accounting = PaneInputBatchAccounting::default();
            self.dispatch_queued_copy_input(outcome, &mut accounting);
        } else {
            self.copy_pipeline.clear_ops();
            if self.copy_mode_owns_input() {
                // Failed copy requests still release the buffered input. Replaying it here
                // keeps local copy actions and later remote motions in order.
                let mut accounting = PaneInputBatchAccounting::default();
                self.dispatch_queued_copy_input(outcome, &mut accounting);
            } else {
                // These keys belonged to the copy pane. Do not send them into a pane that
                // gained focus while the request was outstanding.
                self.copy_pipeline.clear_keys();
            }
        }
    }

    fn dispatch_queued_copy_input(
        &mut self,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        while !self.copy_pipeline.in_flight() {
            let Some(key) = self.copy_pipeline.pop_key() else {
                return;
            };
            // A replayed `q`, `y` or Enter leaves copy mode, and leaving clears
            // the queue (`reset_copy_pipeline`). The keys behind it were typed
            // after that exit and belong to the pane, so hold them aside and
            // put them back: they then route in whatever mode the key left.
            let mut later = self.copy_pipeline.take_keys();
            self.handle_key(key, outcome, accounting);
            later.extend(self.copy_pipeline.take_keys());
            self.copy_pipeline.put_keys(later);
        }
    }

    pub(in crate::shell) fn cancel_deferred_copy_after_search(&mut self, generation: u64) {
        if let Some(search) = self
            .copy_mode
            .as_mut()
            .filter(|copy_mode| copy_mode.operation_generation == generation)
            .and_then(|copy_mode| copy_mode.search.as_mut())
        {
            search.copy_after_result = false;
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

    /// Whether the copy-mode bar is drawn over the copy pane's bottom row: true when the pane
    /// reaches the pane area's bottom row, which the bar takes. Unknown geometry counts as
    /// covered.
    fn mode_bar_covers_copy_pane(&self) -> bool {
        let (Some(hit), Some((cols, rows))) = (self.copy_hit(), self.last_composed_size) else {
            return true;
        };
        let layout = self.layout(cols, rows);
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
            self.mouse_selection.selection = Some(shepr_vt::selection::Selection::line_range(
                copy_mode.pane_id.clone(),
                row,
                row,
                end_col,
            ));
        } else {
            copy_mode.selection = Some(ClientCopySelection::Character {
                anchor: shepr_vt::Point::new(row, copy_mode.cursor.col),
            });
            self.mouse_selection.selection = Some(shepr_vt::selection::Selection::anchor(
                copy_mode.pane_id.clone(),
                shepr_vt::Point::new(row, copy_mode.cursor.col),
            ));
        }
    }

    /// Project the copy selection's anchor and shape onto its current cursor range.
    /// The VT selection is the visible range; the copy state retains the anchor.
    pub(in crate::shell) fn sync_copy_selection(&mut self) {
        let Some(copy_mode) = self.copy_mode.as_ref() else {
            return;
        };
        let Some(selection) = copy_mode.selection else {
            return;
        };
        self.mouse_selection.selection = Some(match selection {
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
        self.copy_pipeline
            .push_op(ClientCopyOperation::Motion(motion));
        self.dispatch_next_copy_operation(outcome);
    }

    pub(in crate::shell) fn dispatch_next_copy_operation(
        &mut self,
        outcome: &mut ClientShellInput,
    ) {
        if self.copy_pipeline.in_flight() {
            return;
        }
        while let Some(operation) = self.copy_pipeline.pop_op() {
            let Some(copy_mode) = self.copy_mode.as_mut() else {
                self.copy_pipeline.reset();
                return;
            };
            let pane_id = copy_mode.pane_id.clone();
            let origin = copy_mode.cursor;
            let (command, kind) = match operation {
                ClientCopyOperation::Motion(motion) => (
                    shepr_protocol::command::EndpointCommand::PaneCopyMotion(
                        shepr_protocol::command::PaneCopyMotionParams {
                            pane_id: pane_id.clone(),
                            cursor: origin,
                            motion,
                        },
                    ),
                    Work::CopyMotion { pane_id, origin },
                ),
                ClientCopyOperation::Search {
                    query,
                    direction,
                    repeat,
                } => {
                    if query.is_empty() {
                        continue;
                    }
                    copy_mode.operation_generation =
                        copy_mode.operation_generation.saturating_add(1);
                    let generation = copy_mode.operation_generation;
                    let search = copy_mode
                        .search
                        .get_or_insert_with(ClientCopySearch::default);
                    let previous = repeat
                        .then(|| {
                            search
                                .current
                                .and_then(|index| search.matches.get(index).copied())
                                .filter(|text_match| text_match.start == copy_mode.cursor)
                        })
                        .flatten();
                    (
                        shepr_protocol::command::EndpointCommand::PaneCopySearch(
                            shepr_protocol::command::PaneCopySearchParams {
                                pane_id: pane_id.clone(),
                                query: query.as_str().to_owned(),
                                direction,
                                cursor: origin,
                                previous,
                            },
                        ),
                        Work::CopySearch {
                            pane_id,
                            origin,
                            query,
                            direction,
                            repeat,
                            generation,
                        },
                    )
                }
            };
            if let Some(id) = self.submit(command, kind, outcome) {
                self.copy_pipeline.begin(id);
            } else {
                self.copy_pipeline.clear_ops();
                if let Some(search) = self
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.search.as_mut())
                {
                    search.copy_after_result = false;
                }
            }
            return;
        }
    }

    pub(in crate::shell) fn apply_copy_motion_target(
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

    pub(in crate::shell) fn exit_copy_mode(&mut self, copy: bool, outcome: &mut ClientShellInput) {
        let live_selection = self
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_vt::selection::Selection::is_visible);
        if copy
            && !live_selection
            && let Some((pane_id, text_match)) = self.copy_mode.as_ref().and_then(|copy_mode| {
                copy_mode
                    .search
                    .as_ref()
                    .and_then(|search| {
                        search
                            .current
                            .and_then(|index| search.matches.get(index).copied())
                    })
                    .map(|text_match| (copy_mode.pane_id.clone(), text_match))
            })
        {
            self.mouse_selection.selection = Some(shepr_vt::selection::Selection::range(
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
                .mouse_selection
                .selection
                .as_ref()
                .is_some_and(shepr_vt::selection::Selection::is_visible)
        {
            self.request_selection_copy(outcome);
        }
        self.mouse_selection.clear();
        self.push_pane_scroll_offset(
            copy_mode.pane_id,
            copy_mode.entry_offset_from_bottom,
            outcome,
        );
        self.mode = ClientShellMode::Terminal;
        outcome.repaint = true;
    }
}

#[cfg(test)]
impl CopyPipeline {
    pub(in crate::shell) fn keys_is_empty(&self) -> bool {
        self.keys.is_empty()
    }
    pub(in crate::shell) fn ops_is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}
#[cfg(test)]
mod pipeline_tests {
    use crossterm::event::KeyCode;

    use crate::shell::input::copy_mode::CopyPipeline;
    use crate::shell::state::ClientCopyOperation;

    #[test]
    fn reset_clears_the_request_and_everything_queued() {
        let mut p = CopyPipeline::default();
        p.begin("request".into());
        p.push_op(ClientCopyOperation::Motion(
            shepr_protocol::command::PaneCopyMotion::Word(
                shepr_protocol::command::PaneWordMotion::NextStart,
            ),
        ));
        p.push_key(shepr_termio::input::TerminalKey::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::empty(),
        ));
        p.reset();
        assert!(!p.in_flight());
        assert!(p.keys_is_empty());
        assert!(p.ops_is_empty());
    }
    #[test]
    fn a_finished_request_keeps_its_queued_keys_for_the_replay() {
        let mut p = CopyPipeline::default();
        p.begin("request".into());
        let key = shepr_termio::input::TerminalKey::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::empty(),
        );
        p.push_key(key.clone());
        p.finish();
        assert!(!p.in_flight());
        assert_eq!(p.pop_key(), Some(key));
    }
}
