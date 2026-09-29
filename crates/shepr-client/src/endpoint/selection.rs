//! The client's endpoint selection: its one owner, tracked across handoffs.
//!
//! Selecting an endpoint updates the in-memory choice immediately, because the
//! automatic activation path reads it to know which endpoint should own the pane
//! surface. A handoff can still fail and roll back to its source, so the choice is
//! only persisted once the target actually owns the surface. A failed handoff
//! restores the previous choice and remembers the failed connection, so automatic
//! activation does not retry it on every snapshot; a fresh connection generation or
//! an explicit request clears that memory.
//!
//! The catalog only supplies the saved machines and the selection file; the selected
//! machine itself lives here.

use tracing::warn;

use super::{ClientEndpointId, EndpointCatalog, ProfileId};

struct SelectionAttempt {
    endpoint_id: ClientEndpointId,
    generation: Option<shepr_protocol::ConnectionGeneration>,
    previous: Option<ProfileId>,
}

pub(crate) struct EndpointSelectionTracker {
    /// The selected saved machine; `None` is Local.
    selected: Option<ProfileId>,
    persisted: Option<ProfileId>,
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
    /// The target owns the surface; the selection should be persisted if it changed.
    Committed { persist: bool },
    /// The handoff did not reach the target; the previous selection was restored.
    Reverted,
}

impl EndpointSelectionTracker {
    /// Starts from the selection the catalog's selection file saved.
    pub(crate) fn new(catalog: &EndpointCatalog) -> Self {
        Self::with_selection(catalog.load_selection())
    }

    fn with_selection(selected: Option<ProfileId>) -> Self {
        Self {
            persisted: selected.clone(),
            selected,
            attempt: None,
            failed: None,
        }
    }

    /// The endpoint this client wants to own the pane surface.
    pub(crate) fn selected_endpoint(&self) -> ClientEndpointId {
        self.selected
            .as_ref()
            .map_or(ClientEndpointId::Local, |profile_id| {
                ClientEndpointId::Ssh(profile_id.clone())
            })
    }

    /// Selects `endpoint_id` for an activation request. Returns false for an endpoint
    /// the catalog does not contain.
    pub(crate) fn begin(
        &mut self,
        catalog: &EndpointCatalog,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
    ) -> bool {
        let next = match endpoint_id {
            ClientEndpointId::Local => None,
            ClientEndpointId::Ssh(profile_id) if catalog.is_selectable(profile_id) => {
                Some(profile_id.clone())
            }
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

    /// Drops a selection that no longer names a saved machine after the catalog
    /// changed, falling back to Local.
    pub(crate) fn catalog_changed(&mut self, catalog: &EndpointCatalog) {
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !catalog.is_selectable(selected))
        {
            self.selected = None;
        }
    }

    /// Resolves the outstanding attempt once no handoff work remains in flight.
    /// `busy` covers a pending activation, a deferred Local activation and a queued
    /// activation event; `active_surface` is whether `active_id` owns the surface.
    pub(crate) fn settle(
        &mut self,
        catalog: &EndpointCatalog,
        busy: bool,
        active_id: &ClientEndpointId,
        active_surface: bool,
    ) -> SelectionOutcome {
        if busy {
            return SelectionOutcome::Unsettled;
        }
        let Some(attempt) = self.attempt.take() else {
            return SelectionOutcome::Unsettled;
        };
        if active_surface && active_id == &attempt.endpoint_id {
            let persist = self.selected != self.persisted;
            return SelectionOutcome::Committed { persist };
        }
        // The catalog may have lost the previous machine while the handoff ran; a removed
        // selection falls back to Local.
        self.selected = attempt
            .previous
            .filter(|previous| catalog.is_selectable(previous));
        self.failed = Some((attempt.endpoint_id, attempt.generation));
        SelectionOutcome::Reverted
    }

    /// Records that the current selection has been written to disk.
    fn mark_persisted(&mut self) {
        self.persisted = self.selected.clone();
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

    /// Settles the outstanding attempt and persists a committed selection change.
    pub(crate) fn settle_and_persist(
        &mut self,
        catalog: &EndpointCatalog,
        busy: bool,
        active_id: &ClientEndpointId,
        active_surface: bool,
    ) {
        if let SelectionOutcome::Committed { persist: true } =
            self.settle(catalog, busy, active_id, active_surface)
        {
            match catalog.store_selection(self.selected.as_ref()) {
                Ok(()) => self.mark_persisted(),
                Err(error) => warn!(%error, "failed to persist endpoint selection"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_with_machine() -> (EndpointCatalog, ProfileId) {
        let mut catalog = EndpointCatalog::default();
        let id = catalog
            .add_ssh("Build", "build")
            .expect("test precondition");
        (catalog, id)
    }

    #[test]
    fn failed_handoff_restores_previous_selection_and_suppresses_retry() {
        let (catalog, id) = catalog_with_machine();
        let mut tracker = EndpointSelectionTracker::with_selection(None);
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&catalog, &target, Some(7)));
        assert_eq!(tracker.selected_endpoint(), target);
        assert_eq!(
            tracker.settle(&catalog, true, &ClientEndpointId::Local, true),
            SelectionOutcome::Unsettled
        );
        assert_eq!(
            tracker.settle(&catalog, false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
        assert!(tracker.suppresses(&target, Some(7)));
        assert!(!tracker.suppresses(&target, Some(8)));
    }

    #[test]
    fn automatic_retry_of_the_startup_selection_is_suppressed_after_failure() {
        let (catalog, id) = catalog_with_machine();
        let mut tracker = EndpointSelectionTracker::with_selection(Some(id.clone()));
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&catalog, &target, Some(3)));
        assert_eq!(
            tracker.settle(&catalog, false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        // The preference survives for the next connection, but not for this one.
        assert_eq!(tracker.selected_endpoint(), target);
        assert!(tracker.suppresses(&target, Some(3)));
    }

    #[test]
    fn committed_handoff_persists_only_changes() {
        let (catalog, id) = catalog_with_machine();
        let mut tracker = EndpointSelectionTracker::with_selection(None);
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&catalog, &target, Some(2)));
        assert_eq!(
            tracker.settle(&catalog, false, &target, true),
            SelectionOutcome::Committed { persist: true }
        );
        tracker.mark_persisted();
        assert!(tracker.begin(&catalog, &target, Some(2)));
        assert_eq!(
            tracker.settle(&catalog, false, &target, true),
            SelectionOutcome::Committed { persist: false }
        );
    }

    #[test]
    fn unknown_machines_cannot_be_selected() {
        let (catalog, _) = catalog_with_machine();
        let mut other = EndpointCatalog::default();
        let unknown = other.add_ssh("Other", "other").expect("test precondition");
        let mut tracker = EndpointSelectionTracker::with_selection(None);
        assert!(!tracker.begin(&catalog, &ClientEndpointId::Ssh(unknown), Some(1)));
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
    }

    #[test]
    fn superseded_request_keeps_the_original_restore_point() {
        let (mut catalog, first) = catalog_with_machine();
        let second = catalog
            .add_ssh("Other", "other")
            .expect("test precondition");
        let mut tracker = EndpointSelectionTracker::with_selection(None);
        assert!(tracker.begin(&catalog, &ClientEndpointId::Ssh(first), Some(2)));
        assert!(tracker.begin(&catalog, &ClientEndpointId::Ssh(second), Some(3)));
        assert_eq!(
            tracker.settle(&catalog, false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
    }

    #[test]
    fn a_revert_never_restores_a_machine_the_catalog_lost_meanwhile() {
        let (mut catalog, first) = catalog_with_machine();
        let second = catalog
            .add_ssh("Other", "other")
            .expect("test precondition");
        let mut tracker = EndpointSelectionTracker::with_selection(Some(first));
        assert!(tracker.begin(&catalog, &ClientEndpointId::Ssh(second), Some(3)));
        // The first machine is removed while the handoff runs.
        catalog.replace_profiles(vec![catalog.ssh[1].clone()]);
        assert_eq!(
            tracker.settle(&catalog, false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
    }

    #[test]
    fn a_catalog_change_drops_a_selection_that_was_removed() {
        let (mut catalog, id) = catalog_with_machine();
        let mut tracker = EndpointSelectionTracker::with_selection(Some(id.clone()));

        catalog.replace_profiles(catalog.ssh.clone());
        tracker.catalog_changed(&catalog);
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Ssh(id));

        catalog.replace_profiles(Vec::new());
        tracker.catalog_changed(&catalog);
        assert_eq!(tracker.selected_endpoint(), ClientEndpointId::Local);
    }

    #[test]
    fn explicit_request_clears_failure_memory() {
        let (catalog, id) = catalog_with_machine();
        let mut tracker = EndpointSelectionTracker::with_selection(None);
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&catalog, &target, Some(4)));
        tracker.settle(&catalog, false, &ClientEndpointId::Local, true);
        assert!(tracker.suppresses(&target, Some(4)));
        assert!(tracker.begin(&catalog, &target, Some(4)));
        assert!(!tracker.suppresses(&target, Some(4)));
    }
}
