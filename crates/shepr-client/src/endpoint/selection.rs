//! The client's endpoint selection: its one owner, tracked across handoffs.
//!
//! Selecting an endpoint updates the in-memory choice immediately, because the
//! automatic activation path reads it to know which endpoint should own the pane
//! surface. A handoff can still fail and roll back to its source, so a failed
//! handoff restores the previous choice and remembers the failed connection, so
//! automatic activation does not retry it on every loop turn; a fresh connection
//! generation or an explicit request clears that memory. The selection is never
//! written to disk: every client starts on Local.
//!
//! The tracker knows the launch-time machine labels, which never change; the
//! selected machine itself lives here.

use super::ClientEndpointId;
use shepr_config::MachineLabel;

struct SelectionAttempt {
    endpoint_id: ClientEndpointId,
    generation: Option<shepr_protocol::ConnectionGeneration>,
    previous: Option<MachineLabel>,
}

pub(crate) struct EndpointSelectionTracker {
    /// The configured machines a selection may name.
    machines: Vec<MachineLabel>,
    /// The selected machine; `None` is Local.
    selected: Option<MachineLabel>,
    attempt: Option<SelectionAttempt>,
    failed: Option<(
        ClientEndpointId,
        Option<shepr_protocol::ConnectionGeneration>,
    )>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SelectionOutcome {
    /// No attempt was outstanding, or it is still in flight.
    Unsettled,
    /// The target owns the surface.
    Committed,
    /// The handoff did not reach the target; the previous selection was restored.
    Reverted,
}

impl EndpointSelectionTracker {
    /// Starts on Local, with `machines` as the selectable machines.
    pub(crate) fn new(machines: Vec<MachineLabel>) -> Self {
        Self {
            machines,
            selected: None,
            attempt: None,
            failed: None,
        }
    }

    /// The endpoint this client wants to own the pane surface.
    pub(crate) fn selected_endpoint(&self) -> ClientEndpointId {
        self.selected
            .as_ref()
            .map_or(ClientEndpointId::Local, |label| {
                ClientEndpointId::Ssh(label.clone())
            })
    }

    /// Selects `endpoint_id` for an activation request. Returns false for a machine
    /// that is not configured.
    pub(crate) fn begin(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
    ) -> bool {
        let next = match endpoint_id {
            ClientEndpointId::Local => None,
            ClientEndpointId::Ssh(label) if self.machines.contains(label) => Some(label.clone()),
            ClientEndpointId::Ssh(_) => return false,
        };
        let before = std::mem::replace(&mut self.selected, next);
        // A superseded request never owned the surface, so the restore point stays the
        // selection from before the first outstanding request.
        let previous = self
            .attempt
            .take()
            .map_or(before, |attempt| attempt.previous);
        if self
            .failed
            .as_ref()
            .is_some_and(|(failed, _)| failed == endpoint_id)
        {
            self.failed = None;
        }
        self.attempt = Some(SelectionAttempt {
            endpoint_id: endpoint_id.clone(),
            generation: generation.map(Into::into),
            previous,
        });
        true
    }

    /// Resolves the outstanding attempt once no handoff work remains in flight.
    /// `busy` covers a handoff in flight, a deferred Local selection and a queued
    /// activation event; `active_owns_presentation` is whether `active_id` owns the
    /// presentation, which a connection that merely kept its surface does not.
    pub(crate) fn settle(
        &mut self,
        busy: bool,
        active_id: &ClientEndpointId,
        active_owns_presentation: bool,
    ) -> SelectionOutcome {
        if busy {
            return SelectionOutcome::Unsettled;
        }
        let Some(attempt) = self.attempt.take() else {
            return SelectionOutcome::Unsettled;
        };
        if active_owns_presentation && active_id == &attempt.endpoint_id {
            return SelectionOutcome::Committed;
        }
        self.selected = attempt.previous;
        self.failed = Some((attempt.endpoint_id, attempt.generation));
        SelectionOutcome::Reverted
    }

    /// Whether automatic activation of `endpoint_id` on this connection generation
    /// already failed and must wait for a new connection or an explicit request.
    pub(crate) fn suppresses(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
    ) -> bool {
        self.failed
            .as_ref()
            .is_some_and(|(failed, failed_generation)| {
                failed == endpoint_id && *failed_generation == generation.map(Into::into)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(name: &str) -> MachineLabel {
        MachineLabel::parse(name).expect("test precondition")
    }

    fn tracker() -> (EndpointSelectionTracker, MachineLabel) {
        let id = label("Build");
        (EndpointSelectionTracker::new(vec![id.clone()]), id)
    }

    #[test]
    fn a_new_tracker_starts_on_local() {
        let (tracker, _) = tracker();
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
    }

    #[test]
    fn failed_handoff_restores_previous_selection_and_suppresses_retry() {
        let (mut tracker, id) = tracker();
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&target, Some(7)));
        assert_eq!(tracker.selected_endpoint(), target);
        assert_eq!(
            tracker.settle(true, &ClientEndpointId::Local, true),
            SelectionOutcome::Unsettled
        );
        assert_eq!(
            tracker.settle(false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
        assert!(tracker.suppresses(&target, Some(7)));
        assert!(!tracker.suppresses(&target, Some(8)));
    }

    #[test]
    fn a_committed_handoff_keeps_the_selection() {
        let (mut tracker, id) = tracker();
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&target, Some(2)));
        assert_eq!(
            tracker.settle(false, &target, true),
            SelectionOutcome::Committed
        );
        assert_eq!(tracker.selected_endpoint(), target);
    }

    #[test]
    fn a_failed_handoff_back_to_a_selected_machine_restores_it() {
        let (mut tracker, id) = tracker();
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&target, Some(2)));
        assert_eq!(
            tracker.settle(false, &target, true),
            SelectionOutcome::Committed
        );
        assert!(tracker.begin(&ClientEndpointId::Local, Some(5)));
        assert_eq!(
            tracker.settle(false, &target, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(tracker.selected_endpoint(), target);
    }

    #[test]
    fn unknown_machines_cannot_be_selected() {
        let (mut tracker, _) = tracker();
        assert!(!tracker.begin(&ClientEndpointId::Ssh(label("Other")), Some(1)));
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
    }

    #[test]
    fn superseded_request_keeps_the_original_restore_point() {
        let first = label("Build");
        let second = label("Other");
        let mut tracker = EndpointSelectionTracker::new(vec![first.clone(), second.clone()]);
        assert!(tracker.begin(&ClientEndpointId::Ssh(first), Some(2)));
        assert!(tracker.begin(&ClientEndpointId::Ssh(second), Some(3)));
        assert_eq!(
            tracker.settle(false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
    }

    #[test]
    fn explicit_request_clears_failure_memory() {
        let (mut tracker, id) = tracker();
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&target, Some(4)));
        tracker.settle(false, &ClientEndpointId::Local, true);
        assert!(tracker.suppresses(&target, Some(4)));
        assert!(tracker.begin(&target, Some(4)));
        assert!(!tracker.suppresses(&target, Some(4)));
    }
}
