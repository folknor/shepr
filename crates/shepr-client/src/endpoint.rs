mod activation;
pub(crate) mod commands;
mod health;
mod local_failure;
mod message_policy;
mod registry;
pub(crate) mod selection;
mod supervisor;
mod writer;

pub use activation::*;
pub(crate) use local_failure::*;
pub(crate) use message_policy::*;
pub(crate) use registry::*;
pub use registry::{EndpointRegistry, EndpointTransport};
pub use shepr_config::MachineLabel;
pub(crate) use supervisor::*;
pub(crate) use writer::{EndpointReadActivity, NativeEndpointTransport};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClientEndpointId {
    Local,
    Ssh(MachineLabel),
}

impl ClientEndpointId {
    pub(crate) fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// The name the client shows for this endpoint: "Local", or the machine's configured label.
    pub(crate) fn display_label(&self) -> &str {
        match self {
            Self::Local => "Local",
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
    pub(crate) fn after_failure(failure: &shepr_remote::SshFailureDiagnostic) -> Self {
        if failure.needs_attention() {
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
