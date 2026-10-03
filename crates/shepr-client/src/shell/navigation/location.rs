use crate::endpoint::ClientEndpointId;
use crate::shell::endpoints::ClientEndpointFocusTarget;

/// A shell destination always names the endpoint that owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum LocationTarget {
    Machine,
    Workspace(shepr_protocol::WorkspaceId),
    Pane(shepr_protocol::PublicPaneId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct Location {
    pub(in crate::shell) endpoint: ClientEndpointId,
    pub(in crate::shell) target: LocationTarget,
}

impl Location {
    pub(in crate::shell) fn machine(endpoint: ClientEndpointId) -> Self {
        Self {
            endpoint,
            target: LocationTarget::Machine,
        }
    }

    pub(in crate::shell) fn workspace(
        endpoint: ClientEndpointId,
        workspace_id: shepr_protocol::WorkspaceId,
    ) -> Self {
        Self {
            endpoint,
            target: LocationTarget::Workspace(workspace_id),
        }
    }

    pub(in crate::shell) fn pane(
        endpoint: ClientEndpointId,
        pane_id: shepr_protocol::PublicPaneId,
    ) -> Self {
        Self {
            endpoint,
            target: LocationTarget::Pane(pane_id),
        }
    }

    pub(in crate::shell) fn workspace_id(&self) -> Option<shepr_protocol::WorkspaceId> {
        match self.target {
            LocationTarget::Workspace(workspace_id) => Some(workspace_id),
            LocationTarget::Machine | LocationTarget::Pane(_) => None,
        }
    }

    pub(in crate::shell) fn pane_id(&self) -> Option<shepr_protocol::PublicPaneId> {
        match self.target {
            LocationTarget::Pane(pane_id) => Some(pane_id),
            LocationTarget::Machine | LocationTarget::Workspace(_) => None,
        }
    }

    /// The shell action carries this operand alongside `endpoint`, then the choice stores it
    /// under that endpoint's move. Shell-facing destinations retain the complete Location.
    pub(in crate::shell) fn focus_target(&self) -> Option<ClientEndpointFocusTarget> {
        match self.target {
            LocationTarget::Machine => None,
            LocationTarget::Workspace(workspace_id) => {
                Some(ClientEndpointFocusTarget::Workspace(workspace_id))
            }
            LocationTarget::Pane(pane_id) => Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct SnapshotIdentity {
    boot_id: shepr_protocol::BootId,
    generation: u64,
}

/// A location captured from one snapshot. It is valid only while both snapshot identity
/// components still match the endpoint's current presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct PinnedLocation {
    pub(in crate::shell) location: Location,
    snapshot: SnapshotIdentity,
}

impl PinnedLocation {
    pub(in crate::shell) fn new(
        location: Location,
        boot_id: shepr_protocol::BootId,
        generation: u64,
    ) -> Self {
        Self {
            location,
            snapshot: SnapshotIdentity {
                boot_id,
                generation,
            },
        }
    }

    pub(in crate::shell) fn matches(
        &self,
        endpoint: &ClientEndpointId,
        target: LocationTarget,
    ) -> bool {
        self.location.endpoint == *endpoint && self.location.target == target
    }

    pub(in crate::shell) fn boot_id(&self) -> &shepr_protocol::BootId {
        &self.snapshot.boot_id
    }

    pub(in crate::shell) fn generation(&self) -> u64 {
        self.snapshot.generation
    }
}
