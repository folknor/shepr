//! The scrollback policy: a byte budget per pane, and the line count it buys.

pub use crate::limits::{ESTIMATED_CELL_BYTES, MAX_HISTORY_LINES, MIN_HISTORY_LINES};

/// A count of scrollback lines: the history capacity a [`ScrollbackBudget`]
/// buys at some width. Zero means no history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HistoryLines(usize);

impl HistoryLines {
    pub const NONE: Self = Self(0);

    pub fn get(self) -> usize {
        self.0
    }

    pub fn is_none(self) -> bool {
        self.0 == 0
    }

    /// This capacity raised to hold `held` lines of content already retained.
    pub fn at_least(self, held: usize) -> Self {
        Self(self.0.max(held))
    }
}

/// The approximate scrollback budget of one pane, in bytes.
///
/// The one policy: zero disables scrollback; any non-zero budget keeps at least
/// [`MIN_HISTORY_LINES`] and at most [`MAX_HISTORY_LINES`], and between them
/// the line count is the budget divided by an estimated line size, so a
/// narrower pane keeps more lines. It is not a hard memory cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackBudget(usize);

impl ScrollbackBudget {
    /// A budget that keeps no history.
    pub const DISABLED: Self = Self(0);

    pub fn new(bytes: usize) -> Self {
        Self(bytes)
    }

    pub fn bytes(self) -> usize {
        self.0
    }

    /// The history lines this budget buys for a pane `columns` wide.
    pub fn lines_at(self, columns: usize) -> HistoryLines {
        if self.0 == 0 {
            return HistoryLines::NONE;
        }
        let bytes_per_line = columns.max(1).saturating_mul(ESTIMATED_CELL_BYTES);
        HistoryLines((self.0 / bytes_per_line).clamp(MIN_HISTORY_LINES, MAX_HISTORY_LINES))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_converts_to_bounded_line_counts() {
        assert_eq!(ScrollbackBudget::DISABLED.lines_at(80), HistoryLines::NONE);
        assert_eq!(
            ScrollbackBudget::new(1).lines_at(80).get(),
            MIN_HISTORY_LINES
        );
        let per_line = 80 * ESTIMATED_CELL_BYTES;
        assert_eq!(
            ScrollbackBudget::new(per_line * 5_000).lines_at(80).get(),
            5_000
        );
        assert_eq!(
            ScrollbackBudget::new(usize::MAX).lines_at(80).get(),
            MAX_HISTORY_LINES
        );
        let budget = ScrollbackBudget::new(per_line * 5_000);
        assert!(budget.lines_at(40) > budget.lines_at(80));
        assert_eq!(
            ScrollbackBudget::new(1).lines_at(0).get(),
            MIN_HISTORY_LINES
        );
    }
}
