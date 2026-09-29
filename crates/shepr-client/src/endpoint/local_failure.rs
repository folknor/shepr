use super::ClientEndpointId;

/// Whether losing Local is fatal for the client process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalFailurePolicy {
    ExitClient,
    Reconnect,
}

impl LocalFailurePolicy {
    /// Resolve the client lifetime rule from its configured machines.
    pub(crate) fn for_machines(machines: &[shepr_config::MachineConfig]) -> Self {
        if !machines.is_empty() {
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
    fn a_config_without_machines_makes_local_failure_fatal() {
        let policy = LocalFailurePolicy::for_machines(&[]);
        assert!(policy.ends_client_for(&ClientEndpointId::Local));
        assert!(!policy.reconnects_local());
    }

    #[test]
    fn configured_machines_keep_the_client_alive_when_local_fails() {
        let machine = shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse("Build").expect("test precondition"),
            ssh: shepr_config::SshTarget::parse("build").expect("test precondition"),
        };
        let policy = LocalFailurePolicy::for_machines(std::slice::from_ref(&machine));
        assert!(!policy.ends_client_for(&ClientEndpointId::Local));
        assert!(policy.reconnects_local());

        let remote = ClientEndpointId::Ssh(machine.label);
        assert!(!policy.ends_client_for(&remote));
    }
}
