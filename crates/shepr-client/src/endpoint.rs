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
pub use shepr_remote::machine::{
    EndpointCatalog, EndpointCatalogChanges, EndpointCatalogWatch, ProfileId, SavedSshEndpoint,
};
pub use supervisor::MAX_RETRY_DELAY;
pub(crate) use supervisor::*;
pub(crate) use writer::NativeEndpointTransport;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClientEndpointId {
    Local,
    Ssh(ProfileId),
}

impl ClientEndpointId {
    pub(crate) fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub(crate) fn storage_key(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Ssh(profile_id) => format!("ssh:{profile_id}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_storage_keys_do_not_contain_ssh_targets() {
        let profile =
            ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition");
        assert_eq!(
            ClientEndpointId::Ssh(profile).storage_key(),
            "ssh:0123456789abcdef0123456789abcdef"
        );
    }
}
