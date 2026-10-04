//! The one outstanding request of a copy session and the keys queued behind it.

use crate::shell::ledger::Ticket;
use std::collections::VecDeque;

/// A copy session runs one server request (a motion or a search) at a time. Every key
/// typed while it is outstanding queues here, so no copy operation starts behind it;
/// the keys replay, in order, once it completes. Keys also exist while a completed
/// request's input is replayed.
#[derive(Default)]
pub(in crate::shell) struct CopyPipeline {
    /// The one outstanding copy request. Only an answer carrying its ticket applies;
    /// a reset discards it, which is what makes a late answer stale.
    flight: Option<Ticket>,
    keys: VecDeque<shepr_term::key::TerminalKey>,
}
impl CopyPipeline {
    pub(in crate::shell) fn in_flight(&self) -> bool {
        self.flight.is_some()
    }
    pub(super) fn holds(&self, flight: Ticket) -> bool {
        self.flight == Some(flight)
    }
    pub(super) fn begin(&mut self, flight: Ticket) {
        self.flight = Some(flight);
    }
    pub(super) fn finish(&mut self) {
        self.flight = None;
    }
    pub(super) fn reset(&mut self) {
        self.flight = None;
        self.keys.clear();
    }
    pub(in crate::shell) fn push_key(&mut self, key: shepr_term::key::TerminalKey) {
        self.keys.push_back(key);
    }
    pub(in crate::shell) fn keys_len(&self) -> usize {
        self.keys.len()
    }
    pub(super) fn take_keys(&mut self) -> VecDeque<shepr_term::key::TerminalKey> {
        std::mem::take(&mut self.keys)
    }
    pub(super) fn put_keys(&mut self, keys: VecDeque<shepr_term::key::TerminalKey>) {
        self.keys = keys;
    }
    pub(in crate::shell) fn clear_keys(&mut self) {
        self.keys.clear();
    }
}

#[cfg(test)]
impl CopyPipeline {
    pub(super) fn pop_key(&mut self) -> Option<shepr_term::key::TerminalKey> {
        self.keys.pop_front()
    }
    pub(super) fn keys_is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}
#[cfg(test)]
mod pipeline_tests {
    use crossterm::event::KeyCode;

    use crate::shell::copy::pipeline::CopyPipeline;
    use crate::shell::ledger::Ticket;

    #[test]
    fn reset_clears_the_request_and_everything_queued() {
        let mut p = CopyPipeline::default();
        p.begin(Ticket::fixture(1));
        p.push_key(shepr_term::key::TerminalKey::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::empty(),
        ));
        p.reset();
        assert!(!p.in_flight());
        assert!(p.keys_is_empty());
    }
    #[test]
    fn a_finished_request_keeps_its_queued_keys_for_the_replay() {
        let mut p = CopyPipeline::default();
        p.begin(Ticket::fixture(1));
        let key = shepr_term::key::TerminalKey::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::empty(),
        );
        p.push_key(key.clone());
        p.finish();
        assert!(!p.in_flight());
        assert_eq!(p.pop_key(), Some(key));
    }
}
