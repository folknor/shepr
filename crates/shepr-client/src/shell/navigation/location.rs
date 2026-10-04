use crate::endpoint::ClientEndpointId;

/// What a shell destination names inside its endpoint. `Machine` is the endpoint itself:
/// selecting it navigates nowhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocationTarget {
    Machine,
    Workspace(shepr_protocol::WorkspaceId),
    Pane(shepr_protocol::PublicPaneId),
}

/// A shell destination always names the endpoint that owns it. It is also the one value an
/// endpoint pick carries to the endpoint hub and its choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub(crate) endpoint: ClientEndpointId,
    pub(crate) target: LocationTarget,
}

impl Location {
    pub(crate) fn machine(endpoint: ClientEndpointId) -> Self {
        Self {
            endpoint,
            target: LocationTarget::Machine,
        }
    }

    pub(crate) fn workspace(
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotIdentity {
    boot_id: shepr_protocol::BootId,
    generation: shepr_protocol::ConnectionGeneration,
}

/// A location captured from one snapshot. It is valid only while both snapshot identity
/// components still match the endpoint's current presentation. A configured machine's
/// state entry (its Connect or Restart) is pinned to no snapshot: the machine shows it
/// only while it has none to present, and it stays valid while the entry offers its
/// action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct PinnedLocation {
    pub(in crate::shell) location: Location,
    snapshot: Option<SnapshotIdentity>,
}

impl PinnedLocation {
    pub(in crate::shell) fn new(
        location: Location,
        boot_id: shepr_protocol::BootId,
        generation: shepr_protocol::ConnectionGeneration,
    ) -> Self {
        Self {
            location,
            snapshot: Some(SnapshotIdentity {
                boot_id,
                generation,
            }),
        }
    }

    /// The state entry of the configured machine `endpoint`.
    pub(in crate::shell) fn machine_entry(endpoint: ClientEndpointId) -> Self {
        Self {
            location: Location::machine(endpoint),
            snapshot: None,
        }
    }

    pub(in crate::shell) fn is_machine_entry(&self) -> bool {
        self.snapshot.is_none()
    }

    pub(super) fn matches(&self, endpoint: &ClientEndpointId, target: LocationTarget) -> bool {
        self.location.endpoint == *endpoint && self.location.target == target
    }

    pub(super) fn boot_id(&self) -> Option<&shepr_protocol::BootId> {
        self.snapshot.as_ref().map(|snapshot| &snapshot.boot_id)
    }

    pub(super) fn generation(&self) -> Option<shepr_protocol::ConnectionGeneration> {
        self.snapshot.as_ref().map(|snapshot| snapshot.generation)
    }
}
