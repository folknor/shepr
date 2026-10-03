use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use shepr_core::layout::PaneId;

#[derive(Debug, Default)]
pub struct RenderRequest {
    pub generic: bool,
    pub pty_sources: HashSet<PaneId>,
    pub terminal_title_sources: HashSet<PaneId>,
}

/// Coalesces render requests while retaining enough origin information for the
/// headless server to discard PTY-only updates hidden from every client.
#[derive(Debug, Default)]
pub struct RenderSignal {
    pending: AtomicBool,
    state: Mutex<RenderSignalState>,
}

#[derive(Debug, Default)]
struct RenderSignalState {
    request: RenderRequest,
    immediate_pty_sources: HashSet<PaneId>,
    queued_pty_flags: Vec<Arc<AtomicBool>>,
}

impl RenderSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub fn request_generic(&self) {
        let mut state = shepr_vt::lock_auxiliary(&self.state);
        state.request.generic = true;
        self.pending.store(true, Ordering::Release);
    }

    /// Repeated reads of an already queued pane need only an atomic exchange.
    /// The collector clears enrolled flags under the same lock that drains the
    /// request, so a concurrent producer either joins this batch or the next.
    pub(crate) fn request_pty_coalesced(&self, pane_id: PaneId, queued: &Arc<AtomicBool>) -> bool {
        if queued.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut state = shepr_vt::lock_auxiliary(&self.state);
        state.queued_pty_flags.push(Arc::clone(queued));
        let source_added = state.request.pty_sources.insert(pane_id);
        let wake_for_source = source_added && state.immediate_pty_sources.contains(&pane_id);
        let became_pending = !self.pending.swap(true, Ordering::AcqRel);
        became_pending || wake_for_source
    }

    pub fn set_immediate_pty_sources(&self, sources: HashSet<PaneId>) {
        // The headless loop refreshes this classification before checking
        // pending presentation work in that same iteration, so no extra wake
        // is needed when queued hidden work becomes immediately actionable.
        shepr_vt::lock_auxiliary(&self.state).immediate_pty_sources = sources;
    }

    pub fn has_immediate_work(&self) -> bool {
        let state = shepr_vt::lock_auxiliary(&self.state);
        state.request.generic
            || !state.request.terminal_title_sources.is_empty()
            || state
                .request
                .pty_sources
                .iter()
                .any(|pane_id| state.immediate_pty_sources.contains(pane_id))
    }

    /// Coalesces terminal-title changes separately from ordinary PTY damage so
    /// consumers can update metadata without inspecting every pane. The first
    /// title source makes hidden-only pending PTY work immediately actionable;
    /// later title sources join that already queued work.
    pub fn request_terminal_title(&self, pane_id: PaneId) -> bool {
        let mut state = shepr_vt::lock_auxiliary(&self.state);
        let first_title_source = state.request.terminal_title_sources.is_empty();
        let source_added = state.request.terminal_title_sources.insert(pane_id);
        let became_pending = !self.pending.swap(true, Ordering::AcqRel);
        became_pending || (source_added && first_title_source)
    }

    pub fn pending_terminal_title_sources(&self) -> HashSet<PaneId> {
        shepr_vt::lock_auxiliary(&self.state)
            .request
            .terminal_title_sources
            .clone()
    }

    pub fn take(&self) -> RenderRequest {
        let mut state = shepr_vt::lock_auxiliary(&self.state);
        for queued in state.queued_pty_flags.drain(..) {
            queued.store(false, Ordering::Release);
        }
        self.pending.store(false, Ordering::Release);
        std::mem::take(&mut state.request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn enqueue_pty(
        signal: &RenderSignal,
        pane_id: PaneId,
        queued_flags: &mut HashMap<PaneId, Arc<AtomicBool>>,
    ) -> bool {
        let queued = queued_flags
            .entry(pane_id)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        signal.request_pty_coalesced(pane_id, queued)
    }

    #[test]
    fn repeated_pty_reads_do_not_lock_and_collection_rearms_the_pane() {
        let signal = RenderSignal::new();
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        let queued = Arc::new(AtomicBool::new(false));
        assert!(signal.request_pty_coalesced(pane_id, &queued));
        {
            let _guard = shepr_vt::lock_auxiliary(&signal.state);
            assert!(!signal.request_pty_coalesced(pane_id, &queued));
        }
        assert_eq!(signal.take().pty_sources, HashSet::from([pane_id]));
        assert!(!queued.load(Ordering::Acquire));
        assert!(signal.request_pty_coalesced(pane_id, &queued));
        assert_eq!(signal.take().pty_sources, HashSet::from([pane_id]));
    }

    #[test]
    fn queued_hidden_pane_does_not_suppress_a_new_visible_source() {
        let signal = RenderSignal::new();
        let hidden = shepr_test_fixtures::fixed_pane_id(1);
        let visible = shepr_test_fixtures::fixed_pane_id(2);
        signal.set_immediate_pty_sources(HashSet::from([visible]));
        let hidden_queued = Arc::new(AtomicBool::new(false));
        let visible_queued = Arc::new(AtomicBool::new(false));
        assert!(signal.request_pty_coalesced(hidden, &hidden_queued));
        assert!(!signal.request_pty_coalesced(hidden, &hidden_queued));
        assert!(signal.request_pty_coalesced(visible, &visible_queued));
        assert_eq!(signal.take().pty_sources, HashSet::from([hidden, visible]));
    }

    #[test]
    fn coalesces_pty_sources_until_taken() {
        let signal = RenderSignal::new();
        let mut queued_flags = HashMap::new();
        let first = shepr_test_fixtures::fixed_pane_id(10);
        let second = shepr_test_fixtures::fixed_pane_id(20);

        assert!(enqueue_pty(&signal, first, &mut queued_flags));
        assert!(!enqueue_pty(&signal, first, &mut queued_flags));
        assert!(!enqueue_pty(&signal, second, &mut queued_flags));

        let request = signal.take();
        assert!(!request.generic);
        assert_eq!(request.pty_sources, HashSet::from([first, second]));
        assert!(request.terminal_title_sources.is_empty());
        assert!(!signal.is_pending());
    }

    #[test]
    fn hidden_pty_sources_coalesce_to_one_wake() {
        let signal = RenderSignal::new();
        let mut queued_flags = HashMap::new();
        signal.set_immediate_pty_sources(HashSet::from([shepr_test_fixtures::fixed_pane_id(100)]));

        let wakes = (1..=50)
            .filter(|pane_id| {
                enqueue_pty(
                    &signal,
                    shepr_test_fixtures::fixed_pane_id(*pane_id),
                    &mut queued_flags,
                )
            })
            .count();

        assert_eq!(wakes, 1);
    }

    #[test]
    fn immediate_pty_source_wakes_pending_hidden_work() {
        let signal = RenderSignal::new();
        let mut queued_flags = HashMap::new();
        let hidden = shepr_test_fixtures::fixed_pane_id(10);
        let visible = shepr_test_fixtures::fixed_pane_id(20);
        signal.set_immediate_pty_sources(HashSet::from([visible]));

        assert!(enqueue_pty(&signal, hidden, &mut queued_flags));
        assert!(!enqueue_pty(
            &signal,
            shepr_test_fixtures::fixed_pane_id(30),
            &mut queued_flags
        ));
        assert!(enqueue_pty(&signal, visible, &mut queued_flags));
        assert!(!enqueue_pty(&signal, visible, &mut queued_flags));
    }

    #[test]
    fn newly_visible_queued_pty_work_is_immediate_before_the_loop_checks_it() {
        let signal = RenderSignal::new();
        let mut queued_flags = HashMap::new();
        let pane_id = shepr_test_fixtures::fixed_pane_id(10);

        signal.set_immediate_pty_sources(HashSet::new());
        assert!(enqueue_pty(&signal, pane_id, &mut queued_flags));
        assert!(!signal.has_immediate_work());

        signal.set_immediate_pty_sources(HashSet::from([pane_id]));
        assert!(signal.has_immediate_work());
        assert!(signal.is_pending());
    }

    #[test]
    fn terminal_title_source_wakes_pending_pty_work() {
        let signal = RenderSignal::new();
        let mut queued_flags = HashMap::new();
        let hidden = shepr_test_fixtures::fixed_pane_id(10);
        let first_title = shepr_test_fixtures::fixed_pane_id(20);
        let second_title = shepr_test_fixtures::fixed_pane_id(30);

        assert!(enqueue_pty(&signal, hidden, &mut queued_flags));
        assert!(signal.request_terminal_title(first_title));
        assert!(!signal.request_terminal_title(second_title));
        assert_eq!(
            signal.pending_terminal_title_sources(),
            HashSet::from([first_title, second_title])
        );
    }

    #[test]
    fn coalesces_terminal_title_sources_without_making_them_pty_damage() {
        let signal = RenderSignal::new();
        let pane_id = shepr_test_fixtures::fixed_pane_id(10);

        assert!(signal.request_terminal_title(pane_id));
        assert!(!signal.request_terminal_title(pane_id));
        assert_eq!(
            signal.pending_terminal_title_sources(),
            HashSet::from([pane_id])
        );

        let request = signal.take();
        assert!(request.pty_sources.is_empty());
        assert_eq!(request.terminal_title_sources, HashSet::from([pane_id]));
    }

    #[test]
    fn keeps_generic_and_pty_requests_distinct() {
        let signal = RenderSignal::new();
        let mut queued_flags = HashMap::new();
        let pane_id = shepr_test_fixtures::fixed_pane_id(10);

        signal.request_generic();
        assert!(!enqueue_pty(&signal, pane_id, &mut queued_flags));

        let request = signal.take();
        assert!(request.generic);
        assert_eq!(request.pty_sources, HashSet::from([pane_id]));
    }
}
