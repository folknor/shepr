use std::time::Instant;

use super::super::ClientEndpointId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EndpointLease {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) generation: u64,
    pub(super) boot_id: shepr_protocol::BootId,
    /// The endpoint cache may already be newer than the first activation event. Never let an
    /// activation prove coherence with a revision that the monotonic cache has discarded.
    pub(super) minimum_revision: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ActivationEvidence {
    pub(super) snapshot_revision: Option<u64>,
    pub(super) focused_workspace_id: Option<shepr_protocol::WorkspaceId>,
    pub(super) focused_pane_id: Option<shepr_protocol::PublicPaneId>,
    pub(super) surface: Option<shepr_protocol::PaneSurfaceFrame>,
}

impl ActivationEvidence {
    pub(super) fn record_snapshot(&mut self, snapshot: &shepr_protocol::ClientShellSnapshot) {
        if self
            .snapshot_revision
            .is_none_or(|current| snapshot.revision >= current)
        {
            self.snapshot_revision = Some(snapshot.revision.get());
            self.focused_workspace_id = snapshot.focused_workspace_id.clone();
            self.focused_pane_id = snapshot.focused_pane_id.clone();
        }
    }

    pub(super) fn record_surface(&mut self, surface: shepr_protocol::PaneSurfaceFrame) {
        let replace = self.surface.as_ref().is_none_or(|current| {
            surface.projection_revision > current.projection_revision
                || (surface.projection_revision == current.projection_revision
                    && surface.surface_revision >= current.surface_revision)
        });
        if replace {
            self.surface = Some(surface);
        }
    }

    pub(super) fn invalidate_surface(&mut self) {
        self.surface = None;
    }

    pub(super) fn coherent_surface(
        &self,
        minimum_revision: u64,
        geometry: shepr_protocol::ClientSurfaceSize,
    ) -> Option<&shepr_protocol::PaneSurfaceFrame> {
        self.surface.as_ref().filter(|surface| {
            self.snapshot_revision == Some(surface.projection_revision.get())
                && surface.projection_revision.get() >= minimum_revision
                && super::surface_matches_geometry(surface, geometry)
        })
    }
}

#[derive(Clone, Debug)]
pub(super) enum ActivationPhase {
    ReleasingSource {
        request_id: shepr_protocol::RequestId,
    },
    ActivatingTarget {
        request_id: shepr_protocol::RequestId,
        acknowledged_revision: Option<u64>,
        /// At most one focus request may be in flight. Retargets only replace `focus` until
        /// this response arrives, at which point the latest target is sent.
        focus_request_id: Option<shepr_protocol::RequestId>,
        focus_request_target: Option<crate::shell::ClientEndpointFocusTarget>,
        focus_acknowledged: bool,
        evidence: ActivationEvidence,
    },
    ReleasingTargetForRollback {
        request_id: shepr_protocol::RequestId,
    },
    RestoringSource {
        request_id: shepr_protocol::RequestId,
        acknowledged_revision: Option<u64>,
        evidence: ActivationEvidence,
    },
    SynchronizingPresentation {
        lease: EndpointLease,
        request_id: shepr_protocol::RequestId,
        acknowledged_revision: Option<u64>,
        evidence: ActivationEvidence,
        completion: Box<ActivationCompletion>,
    },
    AwaitingPresentationEffects {
        lease: EndpointLease,
        token: String,
        ready: bool,
        completion: Box<ActivationCompletion>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceActivationProgress {
    Pending,
    Ready,
    Rejected {
        message: String,
        source_release_rejected: bool,
    },
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActivationRollback {
    /// A correlated rollback request is in flight; retain the frozen frame.
    Pending,
    /// Neither endpoint can be made safe to present. Keep pane input frozen while presenting
    /// client chrome and this error.
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationCompletion {
    AwaitingPresentationSync {
        previous: ClientEndpointId,
        endpoint: ClientEndpointId,
    },
    AwaitingPresentationEffects,
    Activated,
    RestoredSource {
        error: String,
        /// A newer endpoint-qualified selection arrived while this handoff was frozen. It is
        /// started only after the original source has been coherently restored.
        successor: Option<EndpointActivationIntent>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointActivationIntent {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) target: Option<crate::shell::ClientEndpointFocusTarget>,
}

/// Begin failures are separated by whether the transport may already have observed a lifecycle
/// write. A partial transaction must be kept and rolled back under a frozen presentation.
#[derive(Debug)]
pub enum ActivationBeginError {
    Preflight(String),
    Partial {
        activation: Box<PendingEndpointActivation>,
        error: String,
    },
}

/// The only owner of an endpoint handoff. The registry's active endpoint remains the committed
/// endpoint until the target commits, while this object owns the uncommitted lifecycle lane.
#[derive(Debug)]
pub struct PendingEndpointActivation {
    pub(super) source: EndpointLease,
    pub(super) source_available: bool,
    pub(super) target: EndpointLease,
    pub(super) focus: Option<crate::shell::ClientEndpointFocusTarget>,
    pub(super) host_focused: bool,
    pub(super) resize: shepr_protocol::ClientMessage,
    /// The surface size carried by `resize`, kept typed so it never has to be re-extracted
    /// from the message. Every assignment of `resize` assigns this too.
    pub(super) geometry: shepr_protocol::ClientSurfaceSize,
    pub(super) phase: ActivationPhase,
    pub(super) deadline: Instant,
    pub(super) epoch: u64,
    pub(super) next_focus_serial: u64,
    pub(super) rollback_error: Option<String>,
    /// A different endpoint was selected while this source-off-first transaction was in flight.
    /// Keep only the latest intent until source restoration commits.
    pub(super) successor: Option<EndpointActivationIntent>,
}
