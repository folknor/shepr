mod choice;
pub(crate) mod commands;
mod health;
mod local_failure;
mod message_policy;
mod registry;
mod supervisor;
pub mod view;
mod writer;

pub use choice::*;
pub(crate) use local_failure::*;
pub(crate) use message_policy::*;
pub(crate) use registry::*;
pub use registry::{EndpointRegistry, EndpointTransport};
pub use shepr_config::MachineLabel;
pub(crate) use supervisor::*;
pub use view::{HostBaseline, StartOutcome};
pub(crate) use writer::{EndpointReadActivity, NativeEndpointTransport};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClientEndpointId {
    Local,
    Ssh(MachineLabel),
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

    /// The name the client shows for this endpoint: "Local", or the machine's configured label.
    /// Machine labels refuse the local name, so the two never read alike.
    pub(crate) fn display_label(&self) -> &str {
        match self {
            Self::Local => shepr_config::LOCAL_ENDPOINT_LABEL,
            Self::Ssh(label) => label.as_str(),
        }
    }

    pub(crate) fn storage_key(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Ssh(label) => format!("ssh:{label}"),
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
pub enum ClientEndpointStatus {
    Connecting,
    Online,
    Reconnecting,
    Attention,
}

impl ClientEndpointStatus {
    /// The status a failed connection attempt leaves: Attention for a failure that needs a
    /// repair outside this client, Reconnecting for one a later attempt can outlive.
    pub(crate) fn after_failure(failure: &shepr_remote::EndpointFailure) -> Self {
        if failure.disposition().needs_attention() {
            Self::Attention
        } else {
            Self::Reconnecting
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_storage_keys_use_the_label_not_the_ssh_target() {
        let label = MachineLabel::parse("build").expect("test precondition");
        assert_eq!(ClientEndpointId::Ssh(label).storage_key(), "ssh:build");
    }

    #[test]
    fn endpoint_display_labels_name_local_and_each_machine() {
        let label = MachineLabel::parse("build").expect("test precondition");
        assert_eq!(ClientEndpointId::Local.display_label(), "Local");
        assert_eq!(ClientEndpointId::Ssh(label).display_label(), "build");
    }
}
