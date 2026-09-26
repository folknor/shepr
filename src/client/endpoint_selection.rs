//! Tracks the client's desired endpoint selection across handoffs.
//!
//! Selecting an endpoint updates the in-memory choice immediately, because the
//! automatic activation path reads it to know which endpoint should own the pane
//! surface. A handoff can still fail and roll back to its source, so the choice is
//! only persisted once the target actually owns the surface. A failed handoff
//! restores the previous choice and remembers the failed connection, so automatic
//! activation does not retry it on every snapshot; a fresh connection generation or
//! an explicit request clears that memory.

use tracing::warn;

use super::endpoint::{ClientEndpointId, EndpointCatalog, ProfileId};

struct SelectionAttempt {
    endpoint_id: ClientEndpointId,
    generation: Option<u64>,
    previous: Option<ProfileId>,
}

pub(super) struct EndpointSelectionTracker {
    persisted: Option<ProfileId>,
    attempt: Option<SelectionAttempt>,
    failed: Option<(ClientEndpointId, Option<u64>)>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SelectionOutcome {
    /// No attempt was outstanding, or it is still in flight.
    Unsettled,
    /// The target owns the surface; the selection should be persisted if it changed.
    Committed { persist: bool },
    /// The handoff did not reach the target; the previous selection was restored.
    Reverted,
}

impl EndpointSelectionTracker {
    pub(super) fn new(catalog: &EndpointCatalog) -> Self {
        Self {
            persisted: catalog.selected_profile.clone(),
            attempt: None,
            failed: None,
        }
    }

    /// Selects `endpoint_id` for an activation request. Returns false for an endpoint
    /// the catalog does not contain.
    pub(super) fn begin(
        &mut self,
        catalog: &mut EndpointCatalog,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
    ) -> bool {
        let before = catalog.selected_profile.clone();
        if !catalog.select_endpoint(endpoint_id) {
            return false;
        }
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
            generation,
            previous,
        });
        true
    }

    /// Resolves the outstanding attempt once no handoff work remains in flight.
    /// `busy` covers a pending activation, a deferred Local activation and a queued
    /// activation event; `active_surface` is whether `active_id` owns the surface.
    pub(super) fn settle(
        &mut self,
        catalog: &mut EndpointCatalog,
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
            let persist = catalog.selected_profile != self.persisted;
            return SelectionOutcome::Committed { persist };
        }
        // The catalog may have lost the previous machine while the handoff ran; a removed
        // selection falls back to Local.
        catalog.selected_profile = attempt
            .previous
            .filter(|previous| catalog.is_selectable(previous));
        self.failed = Some((attempt.endpoint_id, attempt.generation));
        SelectionOutcome::Reverted
    }

    /// Records that the current selection has been written to disk.
    pub(super) fn mark_persisted(&mut self, catalog: &EndpointCatalog) {
        self.persisted = catalog.selected_profile.clone();
    }

    /// Whether automatic activation of `endpoint_id` on this connection generation
    /// already failed and must wait for a new connection or an explicit request.
    pub(super) fn suppresses(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
    ) -> bool {
        self.failed
            .as_ref()
            .is_some_and(|(failed, failed_generation)| {
                failed == endpoint_id && *failed_generation == generation
            })
    }

    /// Settles the outstanding attempt and persists a committed selection change.
    pub(super) fn settle_and_persist(
        &mut self,
        catalog: &mut EndpointCatalog,
        busy: bool,
        active_id: &ClientEndpointId,
        active_surface: bool,
    ) {
        if let SelectionOutcome::Committed { persist: true } =
            self.settle(catalog, busy, active_id, active_surface)
        {
            match catalog.store_selection() {
                Ok(()) => self.mark_persisted(catalog),
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
            .add_ssh("Build", "build", "agents")
            .expect("test precondition");
        (catalog, id)
    }

    #[test]
    fn failed_handoff_restores_previous_selection_and_suppresses_retry() {
        let (mut catalog, id) = catalog_with_machine();
        let mut tracker = EndpointSelectionTracker::new(&catalog);
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&mut catalog, &target, Some(7)));
        assert!(catalog.selected_profile.is_some());
        assert_eq!(
            tracker.settle(&mut catalog, true, &ClientEndpointId::Local, true),
            SelectionOutcome::Unsettled
        );
        assert_eq!(
            tracker.settle(&mut catalog, false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(catalog.selected_profile, None);
        assert!(tracker.suppresses(&target, Some(7)));
        assert!(!tracker.suppresses(&target, Some(8)));
    }

    #[test]
    fn automatic_retry_of_the_startup_selection_is_suppressed_after_failure() {
        let (mut catalog, id) = catalog_with_machine();
        assert!(catalog.select_ssh(&id));
        let mut tracker = EndpointSelectionTracker::new(&catalog);
        let target = ClientEndpointId::Ssh(id.clone());
        assert!(tracker.begin(&mut catalog, &target, Some(3)));
        assert_eq!(
            tracker.settle(&mut catalog, false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        // The preference survives for the next connection, but not for this one.
        assert_eq!(catalog.selected_profile, Some(id));
        assert!(tracker.suppresses(&target, Some(3)));
    }

    #[test]
    fn committed_handoff_persists_only_changes() {
        let (mut catalog, id) = catalog_with_machine();
        let mut tracker = EndpointSelectionTracker::new(&catalog);
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&mut catalog, &target, Some(2)));
        assert_eq!(
            tracker.settle(&mut catalog, false, &target, true),
            SelectionOutcome::Committed { persist: true }
        );
        tracker.mark_persisted(&catalog);
        assert!(tracker.begin(&mut catalog, &target, Some(2)));
        assert_eq!(
            tracker.settle(&mut catalog, false, &target, true),
            SelectionOutcome::Committed { persist: false }
        );
    }

    #[test]
    fn superseded_request_keeps_the_original_restore_point() {
        let (mut catalog, first) = catalog_with_machine();
        let second = catalog
            .add_ssh("Other", "other", "agents")
            .expect("test precondition");
        let mut tracker = EndpointSelectionTracker::new(&catalog);
        assert!(tracker.begin(&mut catalog, &ClientEndpointId::Ssh(first), Some(2)));
        assert!(tracker.begin(&mut catalog, &ClientEndpointId::Ssh(second), Some(3)));
        assert_eq!(
            tracker.settle(&mut catalog, false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(catalog.selected_profile, None);
    }

    #[test]
    fn a_revert_never_restores_a_machine_the_catalog_lost_meanwhile() {
        let (mut catalog, first) = catalog_with_machine();
        let second = catalog
            .add_ssh("Other", "other", "agents")
            .expect("test precondition");
        assert!(catalog.select_ssh(&first));
        let mut tracker = EndpointSelectionTracker::new(&catalog);
        assert!(tracker.begin(&mut catalog, &ClientEndpointId::Ssh(second), Some(3)));
        // The first machine is removed while the handoff runs.
        catalog.replace_profiles(vec![catalog.ssh[1].clone()]);
        assert_eq!(
            tracker.settle(&mut catalog, false, &ClientEndpointId::Local, true),
            SelectionOutcome::Reverted
        );
        assert_eq!(catalog.selected_profile, None);
    }

    #[test]
    fn explicit_request_clears_failure_memory() {
        let (mut catalog, id) = catalog_with_machine();
        let mut tracker = EndpointSelectionTracker::new(&catalog);
        let target = ClientEndpointId::Ssh(id);
        assert!(tracker.begin(&mut catalog, &target, Some(4)));
        tracker.settle(&mut catalog, false, &ClientEndpointId::Local, true);
        assert!(tracker.suppresses(&target, Some(4)));
        assert!(tracker.begin(&mut catalog, &target, Some(4)));
        assert!(!tracker.suppresses(&target, Some(4)));
    }
}
