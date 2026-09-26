//! Text selection over stable terminal row identities.
//!
//! Mouse and copy-mode callers convert their input to absolute rows before
//! constructing or extending a selection. This keeps a selection attached to
//! its text as the viewport moves and lets readers reject rows evicted from
//! terminal history.

use crate::{
    layout::PaneId,
    terminal::{AbsRow, Point},
};

/// Current phase of a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Mouse is down but hasn't moved yet. If released without moving, this
    /// was just a click - no selection created.
    Anchored,
    /// Mouse has moved from the anchor point. Cells are being highlighted.
    Dragging,
    /// Mouse released after dragging. Selection is visible and complete.
    Done,
}

/// A text selection within a terminal pane.
#[derive(Debug, Clone)]
pub struct Selection<P = PaneId> {
    /// Which pane the selection belongs to.
    pub pane_id: P,
    anchor: Point<AbsRow>,
    cursor: Point<AbsRow>,
    phase: Phase,
}

impl<P> Selection<P> {
    /// Start a potential selection at a stable terminal position.
    pub fn anchor(pane_id: P, position: Point<AbsRow>) -> Self {
        Self {
            pane_id,
            anchor: position,
            cursor: position,
            phase: Phase::Anchored,
        }
    }

    /// Create a selection over an existing stable range.
    pub(crate) fn range(pane_id: P, anchor: Point<AbsRow>, cursor: Point<AbsRow>) -> Self {
        Self {
            pane_id,
            anchor,
            cursor,
            phase: Phase::Dragging,
        }
    }

    /// Select whole rows in reading order, including their trailing cells.
    pub(crate) fn line_range(
        pane_id: P,
        anchor_row: AbsRow,
        cursor_row: AbsRow,
        end_col: u16,
    ) -> Self {
        let (anchor_col, cursor_col) = if anchor_row <= cursor_row {
            (0, end_col)
        } else {
            (end_col, 0)
        };
        Self::range(
            pane_id,
            Point::new(anchor_row, anchor_col),
            Point::new(cursor_row, cursor_col),
        )
    }

    /// The original anchor position.
    pub(crate) fn anchor_position(&self) -> Point<AbsRow> {
        self.anchor
    }

    /// Extend the selection to a stable terminal position.
    pub fn drag(&mut self, position: Point<AbsRow>) {
        self.cursor = position;
        if self.cursor != self.anchor {
            self.phase = Phase::Dragging;
        }
    }

    /// Finalize the selection. Returns false for a plain click.
    pub fn finish(&mut self) -> bool {
        if self.phase == Phase::Dragging {
            self.phase = Phase::Done;
            true
        } else {
            false
        }
    }

    /// Whether this selection should be rendered.
    pub fn is_visible(&self) -> bool {
        self.phase == Phase::Dragging || self.phase == Phase::Done
    }

    /// Whether this selection was already finalized.
    #[cfg(test)]
    pub fn is_finalized(&self) -> bool {
        self.phase == Phase::Done
    }

    /// Whether the user just clicked without dragging.
    pub fn is_just_click(&self) -> bool {
        self.phase == Phase::Anchored
    }

    /// Force the selection into Dragging phase when pointer movement was
    /// clamped to the same cell as the anchor.
    pub fn force_dragging(&mut self) {
        if self.phase == Phase::Anchored {
            self.phase = Phase::Dragging;
        }
    }

    /// Whether the pointer is still down and the selection can keep extending.
    pub fn is_in_progress(&self) -> bool {
        matches!(self.phase, Phase::Anchored | Phase::Dragging)
    }

    /// Whether the user is actively dragging.
    pub fn is_dragging(&self) -> bool {
        self.phase == Phase::Dragging
    }

    /// Returns the selected range in reading order, with absolute row IDs.
    pub(crate) fn ordered_rows(&self) -> (Point<AbsRow>, Point<AbsRow>) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Return the range as row and column pairs, preserving absolute row IDs.
    pub(crate) fn ordered_cells(&self) -> ((AbsRow, u16), (AbsRow, u16)) {
        let (start, end) = self.ordered_rows();
        ((start.row, start.col), (end.row, end.col))
    }

    /// Check whether a stable terminal cell is inside the visible selection.
    pub fn contains(&self, cell: Point<AbsRow>) -> bool {
        if !self.is_visible() {
            return false;
        }
        let (start, end) = self.ordered_rows();
        if cell.row < start.row || cell.row > end.row {
            return false;
        }
        if start.row == end.row {
            cell.col >= start.col && cell.col <= end.col
        } else if cell.row == start.row {
            cell.col >= start.col
        } else if cell.row == end.row {
            cell.col <= end.col
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(row: u64, col: u16) -> Point<AbsRow> {
        Point::new(AbsRow(row), col)
    }

    fn selection(sr: u64, sc: u16, er: u64, ec: u16) -> Selection {
        Selection::range(PaneId::from_raw(0), point(sr, sc), point(er, ec))
    }

    #[test]
    fn orders_ranges_forward_and_backward() {
        assert_eq!(
            selection(2, 5, 4, 10).ordered_rows(),
            (point(2, 5), point(4, 10))
        );
        assert_eq!(
            selection(4, 10, 2, 5).ordered_rows(),
            (point(2, 5), point(4, 10))
        );
    }

    #[test]
    fn contains_cells_on_single_and_multiple_rows() {
        let one = selection(2, 5, 2, 15);
        assert!(!one.contains(point(2, 4)));
        assert!(one.contains(point(2, 5)));
        assert!(one.contains(point(2, 15)));
        assert!(!one.contains(point(3, 10)));

        let multiple = selection(2, 5, 4, 10);
        assert!(!multiple.contains(point(2, 4)));
        assert!(multiple.contains(point(2, 79)));
        assert!(multiple.contains(point(3, 0)));
        assert!(multiple.contains(point(4, 10)));
        assert!(!multiple.contains(point(4, 11)));
    }

    #[test]
    fn anchor_is_hidden_until_dragged_and_finish_rejects_clicks() {
        let mut selection = Selection::anchor(PaneId::from_raw(0), point(5, 10));
        assert!(!selection.is_visible());
        assert!(!selection.contains(point(5, 10)));
        assert!(selection.is_just_click());
        assert!(!selection.finish());

        selection.drag(point(5, 11));
        assert!(selection.is_visible());
        assert!(selection.finish());
        assert!(selection.is_finalized());
    }

    #[test]
    fn absolute_rows_above_u32_are_preserved() {
        let row = u64::from(u32::MAX) + 10;
        let selection = selection(row, 0, row + 1, 3);
        assert_eq!(selection.ordered_rows(), (point(row, 0), point(row + 1, 3)));
        assert_eq!(
            selection.ordered_cells(),
            ((AbsRow(row), 0), (AbsRow(row + 1), 3))
        );
    }
}
