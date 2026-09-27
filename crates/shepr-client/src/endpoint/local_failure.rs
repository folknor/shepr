use super::{ClientEndpointId, EndpointCatalog};

/// Whether losing Local is fatal for the client process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalFailurePolicy {
    ExitClient,
    Reconnect,
}

impl LocalFailurePolicy {
    /// Resolve the client lifetime rule from its configured saved machines.
    pub(crate) fn for_catalog(catalog: &EndpointCatalog) -> Self {
        Self::for_saved_machines(catalog.has_ssh())
    }

    fn for_saved_machines(has_saved_machines: bool) -> Self {
        if has_saved_machines {
            Self::Reconnect
        } else {
            Self::ExitClient
        }
    }

    pub(crate) fn reconnects_local(self) -> bool {
        matches!(self, Self::Reconnect)
    }

    pub(crate) fn ends_client_for(self, endpoint_id: &ClientEndpointId) -> bool {
        endpoint_id.is_local() && !self.reconnects_local()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catalog_without_saved_machines_makes_local_failure_fatal() {
        let catalog = EndpointCatalog::default();
        let policy = LocalFailurePolicy::for_catalog(&catalog);
        assert!(policy.ends_client_for(&ClientEndpointId::Local));
        assert!(!policy.reconnects_local());
    }

    #[test]
    fn saved_machines_keep_the_client_alive_when_local_fails() {
        let policy = LocalFailurePolicy::for_saved_machines(true);
        assert!(!policy.ends_client_for(&ClientEndpointId::Local));
        assert!(policy.reconnects_local());

        let remote = ClientEndpointId::Ssh(
            super::super::ProfileId::parse("0123456789abcdef0123456789abcdef")
                .expect("test precondition"),
        );
        assert!(!policy.ends_client_for(&remote));
    }
}
