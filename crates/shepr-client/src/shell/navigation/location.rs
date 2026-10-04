use crate::endpoint::ClientEndpointId;

/// What a shell destination names inside its endpoint. `Machine` is the endpoint itself:
/// selecting it navigates nowhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationTarget {
    Machine,
    Workspace(shepr_protocol::WorkspaceId),
    Pane(shepr_protocol::PublicPaneId),
}

/// A shell destination always names the endpoint that owns it. It is also the one value an
/// endpoint pick carries to the endpoint hub and its choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub endpoint: ClientEndpointId,
    pub target: LocationTarget,
}

impl Location {
    pub fn machine(endpoint: ClientEndpointId) -> Self {
        Self {
            endpoint,
            target: LocationTarget::Machine,
        }
    }

    pub fn workspace(
        endpoint: ClientEndpointId,
        workspace_id: shepr_protocol::WorkspaceId,
    ) -> Self {
        Self {
            endpoint,
            target: LocationTarget::Workspace(workspace_id),
        }
    }

    pub fn pane(endpoint: ClientEndpointId, pane_id: shepr_protocol::PublicPaneId) -> Self {
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct SnapshotIdentity {
    boot_id: shepr_protocol::BootId,
    generation: shepr_protocol::ConnectionGeneration,
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
        generation: shepr_protocol::ConnectionGeneration,
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

    pub(in crate::shell) fn generation(&self) -> shepr_protocol::ConnectionGeneration {
        self.snapshot.generation
    }
}
