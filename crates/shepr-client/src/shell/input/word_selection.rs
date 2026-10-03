//! Double-click word selection. Cached rows hold pane text read back from the
//! endpoint; that content must stay out of logs and error messages here.

use crate::shell::ledger::Work;

use crate::shell::state::{
    ClientShellEndpointError, ClientShellInput, ClientShellState, PaneHit, Repaint,
};

use super::word_bounds::word_bounds_at_column;

/// Held second press. Keep only one row read in flight and use the latest
/// pointer position when it returns, so remote latency cannot queue up motion.
#[derive(Debug)]
pub(in crate::shell) struct ClientWordSelection {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) focus_confirmed: bool,
    anchor: shepr_vt::Point<shepr_vt::AbsRow>,
    anchor_bounds: Option<(u16, u16)>,
    cursor: shepr_vt::Point<shepr_vt::AbsRow>,
    end_col: u16,
    cached_row: Option<(shepr_vt::AbsRow, String)>,
    pending: Option<shepr_protocol::RequestId>,
    pub(in crate::shell) dragged: bool,
    pub(in crate::shell) released: bool,
}

impl ClientShellState {
    pub(in crate::shell) fn request_word_selection(
        &mut self,
        hit: &PaneHit,
        metrics: shepr_termio::ScrollMetrics,
        viewport_row: u16,
        col: u16,
        outcome: &mut ClientShellInput,
    ) {
        let row = metrics.absolute_row_at_viewport(shepr_vt::ViewportRow(viewport_row));
        self.mouse_selection.word_gesture = Some(ClientWordSelection {
            pane_id: hit.pane_id,
            focus_confirmed: self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_pane_id.as_ref())
                == Some(&hit.pane_id),
            anchor: shepr_vt::Point::new(row, col),
            anchor_bounds: None,
            cursor: shepr_vt::Point::new(row, col),
            end_col: hit.inner_rect.width.saturating_sub(1),
            cached_row: None,
            pending: None,
            dragged: false,
            released: false,
        });
        self.request_word_selection_row(row, outcome);
    }

    fn cancel_word_selection(&mut self) {
        self.mouse_selection.clear_range();
    }

    fn request_word_selection_row(
        &mut self,
        row: shepr_vt::AbsRow,
        outcome: &mut ClientShellInput,
    ) {
        let Some(gesture) = self.mouse_selection.word_gesture.as_mut() else {
            return;
        };
        if gesture.pending.is_some() {
            return;
        }
        let pane_id = gesture.pane_id;
        let params = shepr_protocol::command::PaneSelectionReadParams {
            pane_id,
            anchor: shepr_protocol::command::PaneTextPoint { row, col: 0 },
            cursor: shepr_protocol::command::PaneTextPoint {
                row,
                col: gesture.end_col,
            },
        };
        if let Some(id) = self.submit(
            shepr_protocol::command::EndpointCommand::PaneSelectionRead(params),
            Work::WordSelection { pane_id, row },
            outcome,
        ) {
            if let Some(gesture) = self.mouse_selection.word_gesture.as_mut() {
                gesture.pending = Some(id);
            }
        } else {
            self.cancel_word_selection();
        }
    }

    pub(in crate::shell) fn drag_word_selection(
        &mut self,
        cursor: (shepr_vt::AbsRow, u16),
        outcome: &mut ClientShellInput,
        now: std::time::Instant,
    ) {
        let Some(gesture) = self.mouse_selection.word_gesture.as_mut() else {
            return;
        };
        let cursor = shepr_vt::Point::new(cursor.0, cursor.1);
        if gesture.released || gesture.cursor == cursor {
            return;
        }
        gesture.cursor = cursor;
        gesture.dragged = true;
        self.update_word_selection(outcome, now);
    }

    pub(in crate::shell) fn finish_word_selection(
        &mut self,
        outcome: &mut ClientShellInput,
        now: std::time::Instant,
    ) {
        self.stop_selection_autoscroll();
        if let Some(gesture) = self.mouse_selection.word_gesture.as_mut() {
            gesture.released = true;
        }
        // A pending row reply will finish the selection if its bounds are not ready yet.
        self.update_word_selection(outcome, now);
    }

    fn update_word_selection(&mut self, outcome: &mut ClientShellInput, now: std::time::Instant) {
        let Some(gesture) = self.mouse_selection.word_gesture.as_ref() else {
            return;
        };
        let Some((anchor_start, anchor_end)) = gesture.anchor_bounds else {
            return;
        };
        let Some((_, text)) = gesture
            .cached_row
            .as_ref()
            .filter(|(row, _)| *row == gesture.cursor.row)
        else {
            self.request_word_selection_row(gesture.cursor.row, outcome);
            return;
        };
        let (start_col, end_col) = word_bounds_at_column(text, gesture.cursor.col)
            .unwrap_or((gesture.cursor.col, gesture.cursor.col));
        let start = shepr_vt::Point::new(gesture.anchor.row, anchor_start)
            .min(shepr_vt::Point::new(gesture.cursor.row, start_col));
        let end = shepr_vt::Point::new(gesture.anchor.row, anchor_end)
            .max(shepr_vt::Point::new(gesture.cursor.row, end_col));
        self.mouse_selection.selection = Some(shepr_vt::selection::Selection::range(
            gesture.pane_id,
            start,
            end,
        ));
        if gesture.released {
            let dragged = gesture.dragged;
            if let Some(selection) = self.mouse_selection.selection.as_mut() {
                selection.finish();
            }
            self.mouse_selection.word_gesture = None;
            if self.config.copy_on_select {
                self.request_selection_copy(outcome);
                if dragged {
                    self.mouse_selection.clear_range();
                } else {
                    self.mouse_selection.highlight_clear_deadline = Some(
                        crate::limits::Deadline::after(
                            now,
                            crate::limits::WORD_SELECTION_HIGHLIGHT_TIMEOUT,
                        )
                        .instant(),
                    );
                }
            }
        }
        outcome.repaint = true;
    }

    pub(in crate::shell) fn drop_word_selection(
        &mut self,
        request: &shepr_protocol::RequestId,
    ) -> Repaint {
        if self
            .mouse_selection
            .word_gesture
            .as_ref()
            .is_some_and(|g| g.pending.as_ref() == Some(request))
        {
            self.cancel_word_selection();
            Repaint::Needed
        } else {
            Repaint::Unchanged
        }
    }

    pub(in crate::shell) fn complete_word_selection_row(
        &mut self,
        request: &shepr_protocol::RequestId,
        pane_id: &shepr_protocol::PublicPaneId,
        absolute_row: shepr_vt::AbsRow,
        result: Result<shepr_protocol::command::EndpointReply, ClientShellEndpointError>,
        now: std::time::Instant,
        outcome: &mut ClientShellInput,
    ) -> Repaint {
        if self
            .mouse_selection
            .word_gesture
            .as_ref()
            .is_none_or(|g| g.pending.as_ref() != Some(request))
        {
            return Repaint::Unchanged;
        }
        if self
            .snapshot
            .as_deref()
            .is_none_or(|snapshot| !snapshot.panes.iter().any(|pane| pane.pane_id == *pane_id))
        {
            self.cancel_word_selection();
            return Repaint::Needed;
        }
        let text = match result {
            Ok(shepr_protocol::command::EndpointReply::PaneSelection {
                pane_id: returned_pane_id,
                text,
            }) if returned_pane_id == *pane_id => text,
            other => {
                if matches!(other, Ok(value) if !matches!(value, shepr_protocol::command::EndpointReply::PaneSelection { .. }))
                {
                    self.set_endpoint_error(
                        "endpoint returned an unexpected word-selection result",
                        now,
                    );
                }
                self.cancel_word_selection();
                return Repaint::Needed;
            }
        };
        let Some(gesture) = self.mouse_selection.word_gesture.as_mut() else {
            return Repaint::Unchanged;
        };
        gesture.pending = None;
        if gesture.anchor_bounds.is_none() {
            gesture.anchor_bounds = word_bounds_at_column(&text, gesture.anchor.col);
            if gesture.anchor_bounds.is_none() {
                self.cancel_word_selection();
                return Repaint::Needed;
            }
        }
        gesture.cached_row = Some((absolute_row, text));
        // The update records its own repaint in `outcome`.
        self.update_word_selection(outcome, now);
        Repaint::Unchanged
    }
}
