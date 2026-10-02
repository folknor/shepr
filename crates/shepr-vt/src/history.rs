use std::sync::Mutex;

use alacritty_terminal::{event::EventListener, grid::Dimensions, term::Term};

use crate::{TerminalEvent, lock_auxiliary, primary_screen_active, scrollback_lines, term_config};

/// Owns the rule for changing the terminal's history capacity and suppressing
/// the title event that `Term::set_options` emits while doing so.
pub(super) struct HistoryCapacity<'a, T: EventListener> {
    term: &'a mut Term<T>,
    events: &'a Mutex<Vec<TerminalEvent>>,
    history_lines: &'a mut usize,
    max_scrollback: usize,
}

impl<'a, T: EventListener> HistoryCapacity<'a, T> {
    pub(super) fn new(
        term: &'a mut Term<T>,
        events: &'a Mutex<Vec<TerminalEvent>>,
        history_lines: &'a mut usize,
        max_scrollback: usize,
    ) -> Self {
        Self {
            term,
            events,
            history_lines,
            max_scrollback,
        }
    }

    pub(super) fn set(&mut self, history_lines: usize) {
        if history_lines == *self.history_lines {
            return;
        }
        let queued_events = lock_auxiliary(self.events).len();
        self.term.set_options(term_config(history_lines));
        // `set_options` synchronously announces the current title. No child
        // event can arrive during this exclusive terminal mutation, so remove
        // only the synthetic tail event before updating the shared limit.
        lock_auxiliary(self.events).truncate(queued_events);
        *self.history_lines = history_lines;
    }

    pub(super) fn restore_after_history_purge(&mut self) {
        if !primary_screen_active(self.term) || self.term.history_size() != 0 {
            return;
        }
        let history_lines = scrollback_lines(self.max_scrollback, self.term.columns());
        self.set(history_lines);
    }
}
