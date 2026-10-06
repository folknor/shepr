mod choice;
pub(crate) mod commands;
pub(crate) mod connection_io;
mod health;
mod hub;
mod local_failure;
mod message_policy;
mod registry;
mod supervisor;
mod view;

pub(crate) use choice::*;
pub(crate) use hub::{Admission, EndpointHub, HubEffect, SnapshotDirty};
pub(crate) use local_failure::*;
pub(crate) use message_policy::*;
pub(crate) use registry::*;
pub(crate) use shepr_config::MachineLabel;
pub(crate) use supervisor::*;
pub(crate) use view::HostBaseline;

/// Token for mutable choice access; only this module can construct it.
pub(crate) struct ChoiceAccess {
    _private: (),
}

impl ChoiceAccess {
    fn new() -> Self {
        Self { _private: () }
    }
}

pub(in crate::endpoint) fn choice_mut(
    shell: &mut crate::shell::ClientShellState,
) -> &mut EndpointChoice {
    shell.endpoints.choice_mut(ChoiceAccess::new())
}

pub(crate) fn mark_local_unavailable(shell: &mut crate::shell::ClientShellState) {
    *choice_mut(shell) = EndpointChoice::initial_local_waiting();
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ClientEndpointId {
    Local,
    Ssh(MachineLabel),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientEndpointBootKey {
    endpoint_id: ClientEndpointId,
    boot_id: shepr_protocol::BootId,
}

impl ClientEndpointBootKey {
    pub(crate) fn new(endpoint_id: &ClientEndpointId, boot_id: &shepr_protocol::BootId) -> Self {
        Self {
            endpoint_id: endpoint_id.clone(),
            boot_id: boot_id.clone(),
        }
    }

    pub(crate) fn endpoint_id(&self) -> &ClientEndpointId {
        &self.endpoint_id
    }
}

impl ClientEndpointId {
    pub(crate) fn policy(&self) -> EndpointPolicy {
        match self {
            Self::Local => EndpointPolicy::Local,
            Self::Ssh(_) => EndpointPolicy::Machine,
        }
    }

    pub(crate) fn is_local(&self) -> bool {
        self.policy().is_local()
    }

    /// The label the client shows for this endpoint: the local server's configured label (or
    /// this host's short name) for the local endpoint, and the configured label for a machine.
    /// A machine entry whose label matches the local label is treated as this host's own entry
    /// and skipped, so it does not create a second endpoint with the same displayed label.
    pub(crate) fn display_label<'a>(&'a self, local: &'a MachineLabel) -> &'a str {
        match self {
            Self::Local => local.as_str(),
            Self::Ssh(label) => label.as_str(),
        }
    }
}

impl std::fmt::Display for ClientEndpointId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => formatter.write_str("local"),
            Self::Ssh(label) => write!(formatter, "ssh:{label}"),
        }
    }
}

/// Behavior shared by an endpoint identity's local or machine role. Values that own transport
/// resources remain in their target and link types; this policy answers only client behavior
/// that varies by role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointPolicy {
    Local,
    Machine,
}

impl EndpointPolicy {
    pub(crate) fn is_local(self) -> bool {
        matches!(self, Self::Local)
    }

    pub(crate) fn uses_ssh_heartbeat(self) -> bool {
        matches!(self, Self::Machine)
    }

    pub(crate) fn handshake_read_timeout(self) -> std::time::Duration {
        match self {
            Self::Local => crate::limits::LOCAL_HANDSHAKE_READ_TIMEOUT,
            Self::Machine => crate::limits::REMOTE_HANDSHAKE_READ_TIMEOUT,
        }
    }

    pub(crate) fn resets_attempts_on_online(self) -> bool {
        self.is_local()
    }

    pub(crate) fn abandons_unconnected_move(self, has_shown_endpoint: bool) -> bool {
        matches!(self, Self::Machine) && has_shown_endpoint
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientEndpointStatus {
    Connecting,
    Online,
    Reconnecting,
    Attention,
}

/// The status a failed attempt or a lost connection leaves, the only status anything outside
/// the endpoint's own connection lifecycle sets. An endpoint becomes Online only when a
/// connection's handshake opens a generation and that generation's snapshot arrives, and
/// starts Connecting only at launch, so neither is a status a caller can assign.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointFailureStatus {
    Reconnecting,
    Attention,
}

impl EndpointFailureStatus {
    /// Attention for a failure that needs a repair outside this client, Reconnecting for one
    /// a later attempt can outlive.
    pub(crate) fn after_failure(failure: &shepr_launch::EndpointFailure) -> Self {
        if failure.disposition().needs_attention() {
            Self::Attention
        } else {
            Self::Reconnecting
        }
    }
}

impl From<EndpointFailureStatus> for ClientEndpointStatus {
    fn from(status: EndpointFailureStatus) -> Self {
        match status {
            EndpointFailureStatus::Reconnecting => Self::Reconnecting,
            EndpointFailureStatus::Attention => Self::Attention,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_display_uses_the_label_not_the_ssh_target() {
        let label = MachineLabel::parse("build").expect("test precondition");
        assert_eq!(ClientEndpointId::Ssh(label).to_string(), "ssh:build");
    }

    #[test]
    fn endpoint_display_labels_name_local_and_each_machine() {
        let label = MachineLabel::parse("build").expect("test precondition");
        let local = MachineLabel::parse("desk").expect("test precondition");
        assert_eq!(ClientEndpointId::Local.display_label(&local), "desk");
        assert_eq!(ClientEndpointId::Ssh(label).display_label(&local), "build");
    }
}
