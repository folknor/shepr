use alacritty_terminal::{event::EventListener, grid::Dimensions, term::Term};
use shepr_core::scrollback::{HistoryLines, ScrollbackBudget};

use crate::effects::Effects;
use crate::{primary_screen_active, term_config};

/// Owns the rule for changing the terminal's history capacity and suppressing
/// the title event that `Term::set_options` emits while doing so.
pub(super) struct HistoryCapacity {
    budget: ScrollbackBudget,
    /// History capacity. It grows with the resize budget, decreases only after
    /// a primary-screen width change (never below the content held) or an
    /// explicit primary-history purge, and never on a height change.
    lines: HistoryLines,
}

impl HistoryCapacity {
    pub(super) fn new(budget: ScrollbackBudget, columns: usize) -> Self {
        Self {
            budget,
            lines: budget.lines_at(columns),
        }
    }

    pub(super) fn lines(&self) -> HistoryLines {
        self.lines
    }

    /// The lines the byte budget buys at `columns` wide.
    pub(super) fn budget_lines(&self, columns: usize) -> HistoryLines {
        self.budget.lines_at(columns)
    }

    pub(super) fn set<T: EventListener>(
        &mut self,
        term: &mut Term<T>,
        effects: &Effects,
        lines: HistoryLines,
    ) {
        if lines == self.lines {
            return;
        }
        let queued_events = effects.queued_len();
        term.set_options(term_config(lines.get()));
        // `set_options` synchronously announces the current title. No child
        // event can arrive during this exclusive terminal mutation, so remove
        // only the synthetic tail event before updating the shared limit.
        effects.truncate_queue(queued_events);
        self.lines = lines;
    }

    /// A purge leaves no retained history whose old capacity must be
    /// preserved, so restore the byte-budget limit at the current width.
    pub(super) fn restore_after_purge<T: EventListener>(
        &mut self,
        term: &mut Term<T>,
        effects: &Effects,
    ) {
        if !primary_screen_active(term) || term.history_size() != 0 {
            return;
        }
        let lines = self.budget_lines(term.columns());
        self.set(term, effects, lines);
    }
}
