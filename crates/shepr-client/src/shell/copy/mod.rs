//! Client copy mode state: one `CopySession` per entered pane, owning the queue of
//! operations and keys behind its outstanding request. Ending a session discards
//! everything queued against it.

pub(in crate::shell) mod keys;
pub(in crate::shell) mod pipeline;

use crate::shell::input::scroll_lanes::ScrollLanes;
use crate::shell::input::selection::MouseSelection;
use crate::shell::ledger::{Ledger, Ticket};
use crate::shell::mode::ModeState;
use crate::shell::state::{ClientShellMode, TypedText};
use pipeline::CopyPipeline;
use shepr_protocol::{ClientShellSnapshot, PaneSurfaceFrame};
use shepr_termio::text_editor::TextEditor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientCopySelection {
    Character {
        anchor: shepr_term::Point<shepr_term::AbsRow>,
    },
    Linewise {
        anchor_row: shepr_term::AbsRow,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct ClientCopySearchPrompt {
    pub(in crate::shell) direction: shepr_protocol::command::PaneCopySearchDirection,
    pub(in crate::shell) query: TextEditor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ClientCopyOperation {
    Motion(shepr_protocol::command::PaneCopyMotion),
    Search {
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::shell) struct ClientCopySearchResult {
    pub(in crate::shell) matches: Vec<shepr_protocol::command::PaneTextRange>,
    pub(in crate::shell) total: usize,
    /// Both indexes identify the same match in the returned window and full result set.
    pub(in crate::shell) current: Option<shepr_protocol::command::PaneCopySearchPosition>,
}

/// One live search lifecycle: prompt, query, result projection and deferred-copy intent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::shell) struct ClientCopySearch {
    pub(in crate::shell) prompt: Option<ClientCopySearchPrompt>,
    pub(in crate::shell) query: TypedText,
    pub(in crate::shell) direction: Option<shepr_protocol::command::PaneCopySearchDirection>,
    pub(in crate::shell) results: ClientCopySearchResult,
    pub(in crate::shell) copy_after_result: bool,
}

impl ClientCopySearch {
    fn clear_results(&mut self) {
        self.results = ClientCopySearchResult::default();
        self.copy_after_result = false;
    }
}

/// Stored copy cursor state; it can survive while the Copy input mode is parked.
/// Copy-mode coordinates are absolute rows. Output leaves retained points in
/// place; surface installation clamps evicted cursor and selection rows and
/// prunes evicted search matches. The viewport is addressed by scroll offsets,
/// so these convert.
pub(in crate::shell) struct CopySession {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) scroll: shepr_term::ScrollMetrics,
    pub(in crate::shell) geometry: (u16, u16),
    alternate_screen_active: bool,
    pub(in crate::shell) cursor: shepr_protocol::command::PaneTextPoint,
    pub(in crate::shell) entry_offset_from_bottom: usize,
    /// The anchor and selection shape drive the projected VT range in `MouseSelection`.
    /// They are not a duplicate range: the projection changes as the copy cursor moves.
    pub(in crate::shell) selection: Option<ClientCopySelection>,
    pub(in crate::shell) search: Option<ClientCopySearch>,
    /// The rows this session's row-addressed answers are checked against. Re-issued at
    /// session entry, when a resize or screen switch moves the rows, when the search is
    /// cleared, and at each search dispatch. A search answer carrying another value is
    /// not applied; its flight still completes, so the keys queued behind it replay.
    rows: Ticket,
    pipeline: CopyPipeline,
}

/// What a session starts from: the pane's geometry and cursor when copy mode is entered.
#[derive(Clone, Copy)]
pub(in crate::shell) struct CopyEntry {
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) scroll: shepr_term::ScrollMetrics,
    pub(in crate::shell) geometry: (u16, u16),
    pub(in crate::shell) alternate_screen_active: bool,
    pub(in crate::shell) cursor: shepr_protocol::command::PaneTextPoint,
    /// The session-entry ticket for `CopySession::rows`.
    pub(in crate::shell) rows: Ticket,
}

impl CopySession {
    /// The only constructor; the entry offset is the scroll's `offset_from_bottom`.
    pub(in crate::shell) fn start(entry: CopyEntry) -> Self {
        Self {
            pane_id: entry.pane_id,
            entry_offset_from_bottom: entry.scroll.offset_from_bottom,
            scroll: entry.scroll,
            geometry: entry.geometry,
            alternate_screen_active: entry.alternate_screen_active,
            cursor: entry.cursor,
            selection: None,
            search: None,
            rows: entry.rows,
            pipeline: CopyPipeline::default(),
        }
    }

    pub(in crate::shell) fn pipeline(&self) -> &CopyPipeline {
        &self.pipeline
    }

    pub(in crate::shell) fn pipeline_mut(&mut self) -> &mut CopyPipeline {
        &mut self.pipeline
    }

    /// The VT selection this session's anchor and cursor project, if it selects.
    fn projected_selection(
        &self,
    ) -> Option<shepr_term::selection::Selection<shepr_protocol::PublicPaneId>> {
        Some(match self.selection? {
            ClientCopySelection::Character { anchor } => shepr_term::selection::Selection::range(
                self.pane_id,
                anchor,
                shepr_term::Point::new(self.cursor.row, self.cursor.col),
            ),
            ClientCopySelection::Linewise { anchor_row } => {
                shepr_term::selection::Selection::line_range(
                    self.pane_id,
                    anchor_row,
                    self.cursor.row,
                )
            }
        })
    }

    /// Whether this stored copy session's pane currently owns focus.
    /// The caller still decides whether a focused session is active input mode.
    pub(in crate::shell) fn pane_is_focused(
        &self,
        focused_pane_id: Option<&shepr_protocol::PublicPaneId>,
    ) -> bool {
        focused_pane_id == Some(&self.pane_id)
    }

    /// The row at the top of the pane's viewport.
    pub(in crate::shell) fn viewport_top(&self) -> shepr_term::AbsRow {
        self.scroll.viewport_top_row()
    }

    /// The newest row the pane retains.
    fn last_row(&self) -> shepr_term::AbsRow {
        let rows = self
            .scroll
            .max_offset_from_bottom
            .saturating_add(usize::from(self.geometry.1.max(1)))
            .saturating_sub(1);
        self.scroll.history_origin.saturating_add(rows)
    }

    /// `row` clamped to the rows the pane retains.
    fn retained_row(&self, row: shepr_term::AbsRow) -> shepr_term::AbsRow {
        row.clamp(self.scroll.history_origin, self.last_row())
    }

    /// The scroll offset that puts `top` at the top of the viewport.
    fn offset_for_top(&self, top: shepr_term::AbsRow) -> usize {
        let from_origin = top.0.saturating_sub(self.scroll.history_origin.0);
        self.scroll
            .max_offset_from_bottom
            .saturating_sub(usize::try_from(from_origin).unwrap_or(usize::MAX))
    }
}

/// Makes the mouse selection show the session's own anchor and cursor projection, when
/// the session selects.
fn project_selection(session: &CopySession, selection: &mut MouseSelection) {
    if let Some(projected) = session.projected_selection() {
        selection.selection = Some(projected);
    }
}

/// Snapshot reconciliation: a removed pane ends the session (and its selection); a focused
/// one re-activates Copy from Terminal and re-projects an absent selection; an unfocused
/// one parks (clears its selection, Copy becomes Terminal). A session can also be parked
/// explicitly while its pane remains focused, so this is not derived from focus alone.
pub(in crate::shell) fn reconcile_snapshot(
    copy: &mut Option<CopySession>,
    mode: &mut ModeState,
    selection: &mut MouseSelection,
    snapshot: &ClientShellSnapshot,
) {
    let Some(session) = copy.as_ref() else {
        return;
    };
    let copy_pane_id = session.pane_id;
    let pane_exists = snapshot
        .panes
        .iter()
        .any(|pane| pane.pane_id == copy_pane_id);
    let pane_focused = session.pane_is_focused(snapshot.focused_pane_id.as_ref());
    if pane_exists && pane_focused {
        if mode.is(ClientShellMode::Terminal) {
            mode.set(ClientShellMode::Copy);
        }
        if selection.selection.is_none() {
            project_selection(session, selection);
        }
        return;
    }
    if !pane_exists {
        // Queued copy-mode keys belonged to this removed pane. Replaying them as input
        // into another pane would be dangerous; ending the session discards its queue.
        *copy = None;
    }
    if selection
        .selection
        .as_ref()
        .is_some_and(|selected| selected.belongs_to(&copy_pane_id))
    {
        selection.clear();
    }
    if mode.is(ClientShellMode::Copy) {
        mode.set(ClientShellMode::Terminal);
    }
}

/// A presented surface or slow-path patch: refreshes geometry, clamps rows, prunes
/// evicted matches, then re-projects or clears the selection. A resize or screen switch
/// re-issues the session's `rows` ticket from `ledger`, which this only mints tickets from.
pub(in crate::shell) fn surface_presented(
    copy: &mut Option<CopySession>,
    selection: &mut MouseSelection,
    lanes: &ScrollLanes,
    ledger: &mut Ledger,
    surface: &PaneSurfaceFrame,
) {
    let Some(session) = copy.as_mut() else {
        return;
    };
    let Some(pane) = surface
        .panes
        .iter()
        .find(|pane| pane.pane_id == session.pane_id)
    else {
        return;
    };
    let mut invalidated = false;
    let mut clamped = false;
    let geometry = (pane.inner_rect.width, pane.inner_rect.height);
    let coordinates_changed = session.geometry != geometry
        || session.alternate_screen_active != pane.alternate_screen_active;
    // Copy-mode points are absolute rows, so output alone moves nothing; a resize or a
    // screen switch reflows or replaces the rows they name.
    if coordinates_changed {
        session.geometry = geometry;
        session.alternate_screen_active = pane.alternate_screen_active;
        session.selection = None;
        invalidated = true;
        if let Some(search) = session.search.as_mut() {
            search.clear_results();
        }
        session.rows = ledger.ticket();
    }
    if let Some(scroll) = pane.scroll {
        let offset = if lanes.target(&pane.pane_id).is_none() {
            scroll.offset_from_bottom
        } else {
            session.scroll.offset_from_bottom
        };
        session.scroll = scroll.with_offset(offset);
        let retained_cursor_row = session.retained_row(session.cursor.row);
        clamped |= retained_cursor_row != session.cursor.row;
        session.cursor.row = retained_cursor_row;
        if let Some(mut selected) = session.selection {
            match &mut selected {
                ClientCopySelection::Character { anchor } => {
                    let retained_row = session.retained_row(anchor.row);
                    clamped |= retained_row != anchor.row;
                    anchor.row = retained_row;
                }
                ClientCopySelection::Linewise { anchor_row } => {
                    let retained_row = session.retained_row(*anchor_row);
                    clamped |= retained_row != *anchor_row;
                    *anchor_row = retained_row;
                }
            }
            session.selection = Some(selected);
        }
        prune_evicted_search_matches(session);
    }
    if clamped {
        project_selection(session, selection);
    }
    if invalidated
        && selection
            .selection
            .as_ref()
            .is_some_and(|selected| selected.belongs_to(&session.pane_id))
    {
        selection.clear();
    }
}

/// Drops search matches whose rows scrolled out of history. They name rows
/// that no longer exist, so they could never render or be copied; the oldest
/// rows go first, so each dropped match was ahead of the current one in the
/// server's global count.
fn prune_evicted_search_matches(session: &mut CopySession) {
    let Some(search) = session.search.as_mut() else {
        return;
    };
    let origin = session.scroll.history_origin;
    let before = search.results.matches.len();
    let current = search.results.current.and_then(|position| {
        search
            .results
            .matches
            .get(position.window_index)
            .copied()
            .map(|found| (found, position.global_index))
    });
    search
        .results
        .matches
        .retain(|found| found.start.row >= origin);
    let removed = before - search.results.matches.len();
    if removed == 0 {
        return;
    }
    search.results.total = search.results.total.saturating_sub(removed);
    search.results.current = current.and_then(|(current, global_index)| {
        search
            .results
            .matches
            .iter()
            .position(|found| *found == current)
            .map(
                |window_index| shepr_protocol::command::PaneCopySearchPosition {
                    window_index,
                    global_index: global_index.saturating_sub(removed),
                },
            )
    });
}

#[cfg(test)]
impl CopySession {
    pub(in crate::shell) fn with_search(mut self, search: ClientCopySearch) -> Self {
        self.search = Some(search);
        self
    }

    pub(in crate::shell) fn with_selection(mut self, selection: ClientCopySelection) -> Self {
        self.selection = Some(selection);
        self
    }

    fn with_cursor(mut self, cursor: shepr_protocol::command::PaneTextPoint) -> Self {
        self.cursor = cursor;
        self
    }
}

/// Assertions about the live session's queue; without a session nothing is queued.
#[cfg(test)]
impl crate::shell::state::ClientShellState {
    pub(in crate::shell) fn copy_in_flight(&self) -> bool {
        self.copy
            .as_ref()
            .is_some_and(|session| session.pipeline().in_flight())
    }

    pub(in crate::shell) fn copy_keys_len(&self) -> usize {
        self.copy
            .as_ref()
            .map_or(0, |session| session.pipeline().keys_len())
    }

    pub(in crate::shell) fn copy_keys_empty(&self) -> bool {
        self.copy_keys_len() == 0
    }

    pub(in crate::shell) fn copy_ops_empty(&self) -> bool {
        self.copy
            .as_ref()
            .is_none_or(|session| session.pipeline().ops_is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientCopyOperation, ClientCopySelection, CopyEntry, CopySession, Ticket};
    use crate::shell::config::ClientShellConfig;
    use crate::shell::state::ClientShellState;
    use crossterm::event::{KeyCode, KeyModifiers};
    use shepr_config::ClientConfig;
    use shepr_term::selection::SelectionShape;

    fn pane_id() -> shepr_protocol::PublicPaneId {
        let workspace =
            shepr_protocol::WorkspaceId::from_number(1).expect("one-based workspace number");
        shepr_protocol::PublicPaneId::new(
            &workspace,
            shepr_protocol::PanePublicNumber::new(1).expect("nonzero literal"),
        )
    }

    fn session() -> CopySession {
        CopySession::start(CopyEntry {
            pane_id: pane_id(),
            scroll: shepr_term::ScrollMetrics::new(0, 0, 2, shepr_term::AbsRow(0)),
            geometry: (10, 2),
            alternate_screen_active: false,
            cursor: shepr_protocol::command::PaneTextPoint {
                row: shepr_term::AbsRow(0),
                col: 0,
            },
            rows: Ticket::fixture(1),
        })
    }

    #[test]
    fn ending_a_session_discards_its_queue() {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        let mut first = session();
        first.pipeline_mut().begin(Ticket::fixture(2), None);
        first.pipeline_mut().push_op(ClientCopyOperation::Motion(
            shepr_protocol::command::PaneCopyMotion::Word(
                shepr_protocol::command::PaneWordMotion::NextStart,
            ),
        ));
        first
            .pipeline_mut()
            .push_key(shepr_term::key::TerminalKey::new(
                KeyCode::Char('j'),
                KeyModifiers::empty(),
            ));
        state.copy = Some(first);

        state.reset_endpoint_projection(crate::shell::endpoints::ProjectionReset::Rebooted);
        assert!(state.copy.is_none());

        let next = session();
        assert!(!next.pipeline().in_flight());
        assert!(next.pipeline().keys_is_empty());
        assert!(next.pipeline().ops_is_empty());
    }

    #[test]
    fn projected_selection_follows_anchor_and_cursor() {
        let plain = session();
        assert!(plain.projected_selection().is_none());

        let anchor = shepr_term::Point::new(shepr_term::AbsRow(0), 1);
        let mut selecting = session().with_selection(ClientCopySelection::Character { anchor });
        selecting.cursor = shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(1),
            col: 3,
        };
        let projected = selecting
            .projected_selection()
            .expect("a character selection projects");
        assert!(projected.belongs_to(&pane_id()));
        assert_eq!(projected.shape(), SelectionShape::Range);
        assert_eq!(
            projected.ordered_rows(),
            (
                shepr_term::Point::new(shepr_term::AbsRow(0), 1),
                shepr_term::Point::new(shepr_term::AbsRow(1), 3)
            )
        );

        let linewise = session()
            .with_selection(ClientCopySelection::Linewise {
                anchor_row: shepr_term::AbsRow(1),
            })
            .with_cursor(shepr_protocol::command::PaneTextPoint {
                row: shepr_term::AbsRow(0),
                col: 4,
            });
        let projected = linewise
            .projected_selection()
            .expect("a linewise selection projects");
        assert_eq!(projected.shape(), SelectionShape::Lines);
        assert_eq!(
            projected.ordered_rows(),
            (
                shepr_term::Point::new(shepr_term::AbsRow(0), 0),
                shepr_term::Point::new(shepr_term::AbsRow(1), 0)
            )
        );
    }
}
