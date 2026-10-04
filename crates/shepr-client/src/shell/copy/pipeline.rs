//! The copy operations and keys queued behind one copy session's outstanding request.

use crate::shell::copy::ClientCopyOperation;
use crate::shell::ledger::Ticket;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug)]
struct CopyFlight {
    ticket: Ticket,
    /// The rows a search was dispatched against; `None` for a motion.
    search_rows: Option<Ticket>,
}

/// Operations only queue behind a flight, except during dispatch. Keys also exist
/// while a completed flight's input is replayed.
#[derive(Default)]
pub(in crate::shell) struct CopyPipeline {
    /// The one outstanding copy request. Only an answer carrying its ticket applies;
    /// a reset discards it, which is what makes a late answer stale.
    flight: Option<CopyFlight>,
    ops: VecDeque<ClientCopyOperation>,
    keys: VecDeque<shepr_term::key::TerminalKey>,
}
impl CopyPipeline {
    pub(in crate::shell) fn in_flight(&self) -> bool {
        self.flight.is_some()
    }
    pub(super) fn holds(&self, flight: Ticket) -> bool {
        self.flight.is_some_and(|held| held.ticket == flight)
    }
    pub(in crate::shell) fn begin(&mut self, flight: Ticket, search_rows: Option<Ticket>) {
        self.flight = Some(CopyFlight {
            ticket: flight,
            search_rows,
        });
    }
    /// The rows a search in flight was dispatched against; `None` for a motion or none.
    pub(super) fn awaited_search_rows(&self) -> Option<Ticket> {
        self.flight.and_then(|held| held.search_rows)
    }
    pub(super) fn finish(&mut self) {
        self.flight = None;
    }
    pub(in crate::shell) fn reset(&mut self) {
        self.flight = None;
        self.ops.clear();
        self.keys.clear();
    }
    pub(in crate::shell) fn push_op(&mut self, op: ClientCopyOperation) {
        self.ops.push_back(op);
    }
    pub(super) fn pop_op(&mut self) -> Option<ClientCopyOperation> {
        self.ops.pop_front()
    }
    pub(super) fn clear_ops(&mut self) {
        self.ops.clear();
    }
    pub(super) fn has_queued_search(&self) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op, ClientCopyOperation::Search { .. }))
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
    pub(super) fn ops_is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}
#[cfg(test)]
mod pipeline_tests {
    use crossterm::event::KeyCode;

    use crate::shell::copy::ClientCopyOperation;
    use crate::shell::copy::pipeline::CopyPipeline;
    use crate::shell::ledger::Ticket;

    #[test]
    fn reset_clears_the_request_and_everything_queued() {
        let mut p = CopyPipeline::default();
        p.begin(Ticket::fixture(1), None);
        p.push_op(ClientCopyOperation::Motion(
            shepr_protocol::command::PaneCopyMotion::Word(
                shepr_protocol::command::PaneWordMotion::NextStart,
            ),
        ));
        p.push_key(shepr_term::key::TerminalKey::new(
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
        p.begin(Ticket::fixture(1), None);
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
