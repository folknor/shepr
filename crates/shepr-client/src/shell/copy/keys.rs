//! Client copy mode. Search queries are typed text and the queued keys are
//! raw input; input content must stay out of logs and error messages here
//! (log lengths or content-free kinds instead).

use crate::shell::copy::pipeline::CopyPipeline;
use crate::shell::copy::{
    ClientCopyOperation, ClientCopySearch, ClientCopySearchPrompt, ClientCopySearchResult,
    ClientCopySelection, CopyEntry, CopySession,
};
use crate::shell::input::events::PaneInputBatchAccounting;
use crate::shell::ledger::{Submitted, Ticket, Work};
use crate::shell::state::{
    ClientShellEndpointError, ClientShellInput, ClientShellMode, ClientShellState, Repaint,
    TypedText,
};
use crate::shell::view::PaneHit;
use crossterm::event::KeyCode;
use shepr_termio::text_editor::TextEditor;

fn abandon_session_operation(copy: &mut Option<CopySession>) {
    if let Some(session) = copy.as_mut() {
        session.pipeline_mut().reset();
        if let Some(search) = session.search.as_mut() {
            search.copy_after_result = false;
        }
    }
}

/// The rollback of a dropped copy request. Its queued keys depend on a result that will
/// never be applied: they are discarded, with the deferred copy, rather than replayed
/// into a frozen presentation. A no-op unless the session's pipeline holds `flight`.
pub(in crate::shell) fn drop_copy_flight(
    copy: &mut Option<CopySession>,
    flight: Ticket,
) -> Repaint {
    if !copy
        .as_ref()
        .is_some_and(|session| session.pipeline().holds(flight))
    {
        return Repaint::Unchanged;
    }
    abandon_session_operation(copy);
    Repaint::Needed
}

impl ClientShellState {
    /// The live session's pipeline. There is none without a session: ending a session
    /// discards everything queued against it.
    fn copy_pipeline_mut(&mut self) -> Option<&mut CopyPipeline> {
        self.copy.as_mut().map(CopySession::pipeline_mut)
    }

    /// Whether `flight` is the one the live session awaits.
    fn copy_awaits(&self, flight: Ticket) -> bool {
        self.copy
            .as_ref()
            .is_some_and(|session| session.pipeline().holds(flight))
    }

    pub(in crate::shell) fn complete_copy_motion(
        &mut self,
        flight: Ticket,
        pane_id: &shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        result: &Result<shepr_protocol::command::PaneCopyMotionReply, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) -> Repaint {
        if !self.copy_awaits(flight) {
            return Repaint::Unchanged;
        }
        let (repaint, continue_queue) = match result {
            Ok(shepr_protocol::command::PaneCopyMotionReply {
                pane_id: returned_pane_id,
                cursor,
            }) if returned_pane_id == pane_id => {
                let applied = self.apply_copy_motion_target(pane_id, origin, *cursor, outcome);
                (
                    if applied {
                        Repaint::Needed
                    } else {
                        Repaint::Unchanged
                    },
                    true,
                )
            }
            Ok(shepr_protocol::command::PaneCopyMotionReply { .. }) => (Repaint::Unchanged, false),
            Err(_) => (Repaint::Needed, false),
        };
        // The apply can reset the pipeline (a search with `copy_after_search` exits copy
        // mode); then nothing queued behind this request may replay or dispatch.
        if self.copy_awaits(flight) {
            self.finish_copy_operation(continue_queue, outcome);
        }
        repaint
    }
    pub(in crate::shell) fn complete_copy_search(
        &mut self,
        flight: Ticket,
        rows: Ticket,
        pane_id: &shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        result: Result<shepr_protocol::command::PaneCopySearchReply, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) -> Repaint {
        if !self.copy_awaits(flight) {
            return Repaint::Unchanged;
        }
        let (repaint, continue_queue) = match result {
            Ok(shepr_protocol::command::PaneCopySearchReply {
                pane_id: returned_pane_id,
                search,
            }) if &returned_pane_id == pane_id => {
                let current = search.current;
                let applied = self.apply_copy_search_result(
                    pane_id,
                    origin,
                    query,
                    direction,
                    repeat,
                    rows,
                    ClientCopySearchResult {
                        matches: search.matches,
                        total: search.total,
                        current,
                    },
                    outcome,
                );
                if applied {
                    (Repaint::Needed, true)
                } else {
                    self.cancel_deferred_copy_after_search(rows);
                    (Repaint::Unchanged, false)
                }
            }
            Ok(shepr_protocol::command::PaneCopySearchReply { .. }) => {
                self.cancel_deferred_copy_after_search(rows);
                (Repaint::Unchanged, false)
            }
            Err(_) => {
                self.cancel_deferred_copy_after_search(rows);
                (Repaint::Needed, false)
            }
        };
        // The apply can reset the pipeline (a search with `copy_after_search` exits copy
        // mode); then nothing queued behind this request may replay or dispatch.
        if self.copy_awaits(flight) {
            self.finish_copy_operation(continue_queue, outcome);
        }
        repaint
    }

    /// Copy accepts input only when its explicit mode is active, no overlay
    /// intercepts it, and the stored session still belongs to the focused pane.
    pub(in crate::shell) fn copy_mode_owns_input(&self) -> bool {
        self.mode.is(ClientShellMode::Copy)
            && self.overlay.is_none()
            && self
                .copy
                .as_ref()
                .is_some_and(|copy_mode| copy_mode.pane_is_focused(self.focused_pane_id().as_ref()))
    }

    /// Keys that leave copy mode or hand input to the prefix: Esc always, and outside
    /// the search prompt the prefix and `q`. While a copy operation is in flight they
    /// queue in order like every other key; they only act out of order when the queue
    /// is full, as the way out of a request that stopped answering.
    pub(in crate::shell) fn copy_mode_interrupt_key(
        &self,
        key: &shepr_term::key::TerminalKey,
    ) -> bool {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return false;
        }
        if key.code == KeyCode::Esc {
            return true;
        }
        let Some(copy_mode) = self.copy.as_ref() else {
            return false;
        };
        if copy_mode
            .search
            .as_ref()
            .is_some_and(|search| search.prompt.is_some())
        {
            return false;
        }
        self.config.keybinds.prefix.matches(key)
            || shepr_termio::copy_mode::copy_mode_command(key)
                == Some(shepr_termio::copy_mode::CopyModeCommand::Exit)
    }

    /// Gives up on the in-flight copy operation and every key queued behind it. The
    /// request stays in the ledger; its answer, if one ever comes, finds no flight
    /// holding its ticket.
    pub(in crate::shell) fn abandon_copy_operation(&mut self) {
        abandon_session_operation(&mut self.copy);
    }

    pub(in crate::shell) fn enter_copy_mode(&mut self, outcome: &mut ClientShellInput) -> bool {
        let pane_id = match self.focused_pane_id() {
            Some(pane_id) => pane_id,
            None => return false,
        };
        if self
            .copy
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.pane_id == pane_id)
        {
            self.mode.set(ClientShellMode::Copy);
            return true;
        }
        if self.copy.is_some() {
            self.exit_copy_mode(false, outcome);
        }
        let Some(hit) = self
            .presentation
            .pane_hits()
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
                let cursor = surface.frame.cursor().filter(|cursor| cursor.visible)?;
                let inner = pane.inner_rect;
                (cursor.x >= inner.x
                    && cursor.x < inner.x.saturating_add(inner.width)
                    && cursor.y >= inner.y
                    && cursor.y < inner.y.saturating_add(inner.height))
                // Lazily: outside the pane the subtractions would underflow.
                .then(|| shepr_protocol::command::PaneTextPoint {
                    row: metrics
                        .absolute_row_at_viewport(shepr_term::ViewportRow(cursor.y - inner.y)),
                    col: cursor.x - inner.x,
                })
            })
            .unwrap_or(shepr_protocol::command::PaneTextPoint {
                row: metrics.absolute_row_at_viewport(shepr_term::ViewportRow(
                    hit.inner_rect.height.saturating_sub(1),
                )),
                col: 0,
            });
        self.mouse_selection.clear();
        let alternate_screen_active = self
            .pane_surface()
            .and_then(|surface| surface.panes.iter().find(|pane| pane.pane_id == pane_id))
            .is_some_and(|pane| pane.alternate_screen_active);
        self.copy = Some(CopySession::start(CopyEntry {
            pane_id,
            scroll: metrics,
            geometry: (hit.inner_rect.width, hit.inner_rect.height),
            alternate_screen_active,
            cursor,
            rows: self.ledger.ticket(),
        }));
        self.mode.set(ClientShellMode::Copy);
        true
    }

    pub(in crate::shell) fn route_copy_mode_key(
        &mut self,
        key: &shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if self.route_copy_search_prompt_key(key, outcome) {
            return;
        }
        let Some(command) = shepr_termio::copy_mode::copy_mode_command(key) else {
            return;
        };
        match command {
            shepr_termio::copy_mode::CopyModeCommand::CancelOrClear => {
                let should_clear = self.copy.as_ref().is_some_and(|copy_mode| {
                    copy_mode.selection.is_some()
                        || copy_mode.search.as_ref().is_some_and(|search| {
                            !search.query.is_empty()
                                || !search.results.matches.is_empty()
                                || search.direction.is_some()
                        })
                });
                if should_clear {
                    if let Some(copy_mode) = self.copy.as_mut() {
                        copy_mode.selection = None;
                        copy_mode.rows = self.ledger.ticket();
                        copy_mode.search = None;
                    }
                    self.mouse_selection.clear();
                } else {
                    self.exit_copy_mode(false, outcome);
                }
                outcome.repaint = true;
            }
            shepr_termio::copy_mode::CopyModeCommand::Exit => {
                self.exit_copy_mode(false, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::Copy => {
                if !self.defer_copy_until_search_result() {
                    self.exit_copy_mode(true, outcome);
                }
            }
            shepr_termio::copy_mode::CopyModeCommand::MoveLeft => {
                self.move_copy_cursor(0, -1, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::MoveDown => {
                self.move_copy_cursor(1, 0, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::MoveUp => {
                self.move_copy_cursor(-1, 0, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::MoveRight => {
                self.move_copy_cursor(0, 1, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::PageUp => {
                self.move_copy_page(-1, shepr_termio::copy_mode::CopyPage::Full, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::PageDown => {
                self.move_copy_page(1, shepr_termio::copy_mode::CopyPage::Full, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::HalfPageUp => {
                self.move_copy_page(-1, shepr_termio::copy_mode::CopyPage::Half, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::HalfPageDown => {
                self.move_copy_page(1, shepr_termio::copy_mode::CopyPage::Half, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::LineStart => {
                self.set_copy_cursor_col(0);
                self.sync_copy_selection();
            }
            shepr_termio::copy_mode::CopyModeCommand::LineEnd => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Line(
                        shepr_protocol::command::PaneLineMotion::End,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::HistoryStart => {
                self.move_copy_history(true, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::HistoryEnd => {
                self.move_copy_history(false, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::FirstNonBlank => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Line(
                        shepr_protocol::command::PaneLineMotion::FirstNonBlank,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::SearchForward => {
                self.open_copy_search(shepr_protocol::command::PaneCopySearchDirection::Forward);
            }
            shepr_termio::copy_mode::CopyModeCommand::SearchBackward => {
                self.open_copy_search(shepr_protocol::command::PaneCopySearchDirection::Backward);
            }
            shepr_termio::copy_mode::CopyModeCommand::RepeatSearchForward => {
                self.repeat_copy_search(false, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::RepeatSearchBackward => {
                self.repeat_copy_search(true, outcome);
            }
            shepr_termio::copy_mode::CopyModeCommand::BeginSelection => {
                self.begin_copy_selection(false);
            }
            shepr_termio::copy_mode::CopyModeCommand::BeginLineSelection => {
                self.begin_copy_selection(true);
            }
            shepr_termio::copy_mode::CopyModeCommand::WordNextStart => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::NextStart,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::WordPreviousStart => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::PreviousStart,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::WordNextEnd => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::NextEnd,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::BigWordNextStart => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::NextBigStart,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::BigWordPreviousStart => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::PreviousBigStart,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::BigWordNextEnd => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::NextBigEnd,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::ParagraphPrevious => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Paragraph(
                        shepr_protocol::command::PaneParagraphMotion::Previous,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::ParagraphNext => {
                self.request_copy_motion(
                    shepr_protocol::command::PaneCopyMotion::Paragraph(
                        shepr_protocol::command::PaneParagraphMotion::Next,
                    ),
                    outcome,
                );
            }
            shepr_termio::copy_mode::CopyModeCommand::SubmitSearch
            | shepr_termio::copy_mode::CopyModeCommand::CancelSearch => return,
        }
        outcome.repaint = true;
    }

    fn route_copy_search_prompt_key(
        &mut self,
        key: &shepr_term::key::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(prompt) = self
            .copy
            .as_ref()
            .and_then(|copy_mode| copy_mode.search.as_ref())
            .and_then(|search| search.prompt.as_ref())
        else {
            return false;
        };
        let mut submit = None;
        match shepr_termio::copy_mode::copy_mode_prompt_command(key) {
            Some(shepr_termio::copy_mode::CopyModeCommand::CancelSearch) => {
                if let Some(copy_mode) = self.copy.as_mut() {
                    let discard_search = if let Some(search) = copy_mode.search.as_mut() {
                        search.prompt = None;
                        search.query.is_empty()
                            && search.direction.is_none()
                            && search.results.matches.is_empty()
                    } else {
                        false
                    };
                    if discard_search {
                        copy_mode.search = None;
                    }
                }
            }
            Some(shepr_termio::copy_mode::CopyModeCommand::SubmitSearch) => {
                submit = Some((prompt.query.to_string(), prompt.direction));
                if let Some(copy_mode) = self.copy.as_mut()
                    && let Some(search) = copy_mode.search.as_mut()
                {
                    search.prompt = None;
                }
            }
            _ => {
                if let Some(prompt) = self
                    .copy
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
            .copy
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
        let Some(copy_mode) = self.copy.as_mut() else {
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
        let Some(copy_mode) = self.copy.as_ref() else {
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
        let Some(copy_mode) = self.copy.as_ref() else {
            return false;
        };
        // Only the awaited search counts, and only against the rows the session still
        // has: every session change resets the pipeline, so a held flight is always the
        // current session's.
        let pending = copy_mode.pipeline().awaited_search_rows() == Some(copy_mode.rows)
            || copy_mode.pipeline().has_queued_search();
        if pending
            && let Some(search) = self
                .copy
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
        if query.is_empty() || self.copy.is_none() {
            return;
        }
        if let Some(copy_mode) = self.copy.as_mut() {
            copy_mode
                .search
                .get_or_insert_with(ClientCopySearch::default);
        }
        if let Some(pipeline) = self.copy_pipeline_mut() {
            pipeline.push_op(ClientCopyOperation::Search {
                query,
                direction,
                repeat,
            });
        }
        self.dispatch_next_copy_operation(outcome);
    }

    pub(in crate::shell) fn apply_copy_search_result(
        &mut self,
        pane_id: &shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        rows: Ticket,
        result: ClientCopySearchResult,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(copy_mode) = self.copy.as_mut() else {
            return false;
        };
        let search_queued = copy_mode.pipeline().has_queued_search();
        if copy_mode.pane_id != *pane_id || copy_mode.cursor != origin || copy_mode.rows != rows {
            return false;
        }
        let Some(search) = copy_mode.search.as_mut() else {
            return false;
        };
        let current = result
            .current
            .filter(|position| position.window_index < result.matches.len());
        search.query = query;
        if !repeat {
            search.direction = Some(direction);
        }
        search.results = ClientCopySearchResult { current, ..result };
        let target =
            current.and_then(|position| search.results.matches.get(position.window_index).copied());
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
        if let Some(pipeline) = self.copy_pipeline_mut() {
            pipeline.finish();
        }
        if continue_queue && self.copy_mode_owns_input() {
            self.dispatch_next_copy_operation(outcome);
            let mut accounting = PaneInputBatchAccounting::default();
            self.dispatch_queued_copy_input(outcome, &mut accounting);
        } else {
            if let Some(pipeline) = self.copy_pipeline_mut() {
                pipeline.clear_ops();
            }
            if self.copy_mode_owns_input() {
                // Failed copy requests still release the buffered input. Replaying it here
                // keeps local copy actions and later remote motions in order.
                let mut accounting = PaneInputBatchAccounting::default();
                self.dispatch_queued_copy_input(outcome, &mut accounting);
            } else {
                // These keys belonged to the copy pane. Do not send them into a pane that
                // gained focus while the request was outstanding.
                if let Some(pipeline) = self.copy_pipeline_mut() {
                    pipeline.clear_keys();
                }
            }
        }
    }

    fn dispatch_queued_copy_input(
        &mut self,
        outcome: &mut ClientShellInput,
        accounting: &mut PaneInputBatchAccounting,
    ) {
        let Some(pipeline) = self.copy_pipeline_mut() else {
            return;
        };
        // A replayed `q`, `y` or Enter leaves copy mode, and ending the session discards
        // its pipeline. The keys behind it were typed after that exit and belong to the
        // pane, so the replay holds them here, outside the session, and they route in
        // whatever mode the key left. Keys the handler queues behind a request it starts
        // go after them.
        let mut held = pipeline.take_keys();
        loop {
            if let Some(session) = self.copy.as_mut()
                && session.pipeline().in_flight()
            {
                let queued = session.pipeline_mut().take_keys();
                held.extend(queued);
                session.pipeline_mut().put_keys(held);
                return;
            }
            let Some(key) = held.pop_front() else {
                return;
            };
            self.handle_key(key, outcome, accounting);
            if let Some(pipeline) = self.copy_pipeline_mut() {
                let queued = pipeline.take_keys();
                held.extend(queued);
            }
        }
    }

    pub(in crate::shell) fn cancel_deferred_copy_after_search(&mut self, rows: Ticket) {
        if let Some(search) = self
            .copy
            .as_mut()
            .filter(|copy_mode| copy_mode.rows == rows)
            .and_then(|copy_mode| copy_mode.search.as_mut())
        {
            search.copy_after_result = false;
        }
    }

    pub(in crate::shell) fn copy_hit(&self) -> Option<PaneHit> {
        let pane_id = &self.copy.as_ref()?.pane_id;
        self.presentation
            .pane_hits()
            .iter()
            .find(|hit| hit.pane_id == *pane_id)
            .cloned()
    }

    fn move_copy_cursor(&mut self, row_delta: i16, col_delta: i16, outcome: &mut ClientShellInput) {
        let Some(copy_mode) = self.copy.as_mut() else {
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

    fn move_copy_page(
        &mut self,
        direction: i8,
        page: shepr_termio::copy_mode::CopyPage,
        outcome: &mut ClientShellInput,
    ) {
        let Some(hit) = self.copy_hit() else {
            return;
        };
        let lines = shepr_termio::copy_mode::copy_mode_page_lines(hit.inner_rect.height, page);
        let Some((pane_id, next_offset)) = self.copy.as_mut().map(|copy_mode| {
            let rows = u64::try_from(lines).unwrap_or(u64::MAX);
            if direction < 0 {
                copy_mode.cursor.row =
                    copy_mode.retained_row(copy_mode.cursor.row.saturating_sub(rows));
                copy_mode.scroll = copy_mode
                    .scroll
                    .with_offset(copy_mode.scroll.offset_from_bottom.saturating_add(lines));
            } else {
                copy_mode.cursor.row =
                    copy_mode.retained_row(copy_mode.cursor.row.saturating_add(rows));
                copy_mode.scroll = copy_mode
                    .scroll
                    .with_offset(copy_mode.scroll.offset_from_bottom.saturating_sub(lines));
            }
            (copy_mode.pane_id, copy_mode.scroll.offset_from_bottom)
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
        let Some((pane_id, offset_from_bottom)) = self.copy.as_mut().map(|copy_mode| {
            if top {
                copy_mode.cursor.row = copy_mode.scroll.history_origin;
                copy_mode.scroll = copy_mode
                    .scroll
                    .with_offset(copy_mode.scroll.max_offset_from_bottom);
            } else {
                copy_mode.cursor.row = copy_mode.last_row();
                copy_mode.scroll = copy_mode.scroll.with_offset(0);
            }
            (copy_mode.pane_id, copy_mode.scroll.offset_from_bottom)
        }) else {
            return;
        };
        self.push_pane_scroll_offset(pane_id, offset_from_bottom, outcome);
        self.sync_copy_selection();
        outcome.repaint = true;
    }

    fn set_copy_cursor_col(&mut self, col: u16) {
        if let Some(copy_mode) = self.copy.as_mut() {
            copy_mode.cursor.col = col;
        }
    }

    /// Whether the copy-mode bar is drawn over the copy pane's bottom row: true when the pane
    /// reaches the pane area's bottom row, which the bar takes. Unknown geometry counts as
    /// covered.
    fn mode_bar_covers_copy_pane(&self) -> bool {
        let (Some(hit), Some(view)) = (self.copy_hit(), self.view()) else {
            return true;
        };
        hit.inner_rect.bottom() >= view.layout.pane_surface.bottom()
    }

    /// Scrolls the copy pane so the cursor is on screen, keeping it off the row the mode bar
    /// covers. Motions and search results both go through here. On the very last line of
    /// history no scroll can lift it; `compose` then moves the bar to the top row instead.
    fn reveal_copy_cursor(&mut self, outcome: &mut ClientShellInput) {
        let reserve_mode_bar_row = self.mode_bar_covers_copy_pane();
        let request = self.copy.as_mut().and_then(|copy_mode| {
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
            if offset == copy_mode.scroll.offset_from_bottom {
                return None;
            }
            copy_mode.scroll = copy_mode.scroll.with_offset(offset);
            Some((copy_mode.pane_id, offset))
        });
        if let Some((pane_id, offset)) = request {
            self.push_pane_scroll_offset(pane_id, offset, outcome);
        }
    }

    fn begin_copy_selection(&mut self, linewise: bool) {
        let Some(copy_mode) = self.copy.as_mut() else {
            return;
        };
        let row = copy_mode.cursor.row;
        if linewise {
            copy_mode.selection = Some(ClientCopySelection::Linewise { anchor_row: row });
            self.mouse_selection.selection = Some(shepr_term::selection::Selection::line_range(
                copy_mode.pane_id,
                row,
                row,
            ));
        } else {
            copy_mode.selection = Some(ClientCopySelection::Character {
                anchor: shepr_term::Point::new(row, copy_mode.cursor.col),
            });
            self.mouse_selection.selection = Some(shepr_term::selection::Selection::anchor(
                copy_mode.pane_id,
                shepr_term::Point::new(row, copy_mode.cursor.col),
            ));
        }
    }

    /// Project the copy selection's anchor and shape onto its current cursor range.
    /// The VT selection is the visible range; the copy state retains the anchor.
    pub(in crate::shell) fn sync_copy_selection(&mut self) {
        if let Some(session) = self.copy.as_ref() {
            crate::shell::copy::project_selection(session, &mut self.mouse_selection);
        }
    }

    fn request_copy_motion(
        &mut self,
        motion: shepr_protocol::command::PaneCopyMotion,
        outcome: &mut ClientShellInput,
    ) {
        if self.copy.is_none() {
            return;
        }
        if let Some(pipeline) = self.copy_pipeline_mut() {
            pipeline.push_op(ClientCopyOperation::Motion(motion));
        }
        self.dispatch_next_copy_operation(outcome);
    }

    pub(in crate::shell) fn dispatch_next_copy_operation(
        &mut self,
        outcome: &mut ClientShellInput,
    ) {
        if self
            .copy
            .as_ref()
            .is_none_or(|session| session.pipeline().in_flight())
        {
            return;
        }
        loop {
            let Some(copy_mode) = self.copy.as_mut() else {
                return;
            };
            let Some(operation) = copy_mode.pipeline_mut().pop_op() else {
                return;
            };
            let pane_id = copy_mode.pane_id;
            let origin = copy_mode.cursor;
            let flight = self.ledger.ticket();
            let mut search_rows = None;
            let (command, kind) = match operation {
                ClientCopyOperation::Motion(motion) => (
                    shepr_protocol::command::EndpointCommand::PaneCopyMotion(
                        shepr_protocol::command::PaneCopyMotionParams {
                            pane_id,
                            cursor: origin,
                            motion,
                        },
                    ),
                    Work::CopyMotion {
                        pane_id,
                        origin,
                        flight,
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
                    let rows = self.ledger.ticket();
                    copy_mode.rows = rows;
                    search_rows = Some(rows);
                    let search = copy_mode
                        .search
                        .get_or_insert_with(ClientCopySearch::default);
                    let previous = repeat
                        .then(|| {
                            search
                                .results
                                .current
                                .and_then(|position| {
                                    search.results.matches.get(position.window_index).copied()
                                })
                                .filter(|text_match| text_match.start == copy_mode.cursor)
                        })
                        .flatten();
                    (
                        shepr_protocol::command::EndpointCommand::PaneCopySearch(
                            shepr_protocol::command::PaneCopySearchParams {
                                pane_id,
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
                            flight,
                            rows,
                        },
                    )
                }
            };
            let submitted = self.submit(command, kind, outcome);
            if let Some(session) = self.copy.as_mut() {
                if submitted == Submitted::Opened {
                    session.pipeline_mut().begin(flight, search_rows);
                } else {
                    session.pipeline_mut().clear_ops();
                    if let Some(search) = session.search.as_mut() {
                        search.copy_after_result = false;
                    }
                }
            }
            return;
        }
    }

    pub(in crate::shell) fn apply_copy_motion_target(
        &mut self,
        pane_id: &shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        cursor: shepr_protocol::command::PaneTextPoint,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(copy_mode) = self.copy.as_mut() else {
            return false;
        };
        if copy_mode.pane_id != *pane_id || copy_mode.cursor != origin {
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
            .is_some_and(shepr_term::selection::Selection::is_visible);
        if copy
            && !live_selection
            && let Some((pane_id, text_match)) = self.copy.as_ref().and_then(|copy_mode| {
                copy_mode
                    .search
                    .as_ref()
                    .and_then(|search| {
                        search.results.current.and_then(|position| {
                            search.results.matches.get(position.window_index).copied()
                        })
                    })
                    .map(|text_match| (copy_mode.pane_id, text_match))
            })
        {
            self.mouse_selection.selection = Some(shepr_term::selection::Selection::range(
                pane_id,
                shepr_term::Point::new(text_match.start.row, text_match.start.col),
                shepr_term::Point::new(text_match.end.row, text_match.end.col),
            ));
        }
        if self.copy.is_none() {
            return;
        }
        if copy
            && self
                .mouse_selection
                .selection
                .as_ref()
                .is_some_and(shepr_term::selection::Selection::is_visible)
        {
            self.request_selection_copy(outcome);
        }
        let Some(copy_mode) = self.copy.take() else {
            return;
        };
        self.mouse_selection.clear();
        self.push_pane_scroll_offset(
            copy_mode.pane_id,
            copy_mode.entry_offset_from_bottom,
            outcome,
        );
        self.mode.set(ClientShellMode::Terminal);
        outcome.repaint = true;
    }
}
