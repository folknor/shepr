#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteServerRestartReason {
    EndpointProtocol,
    SurfaceInterest,
    HealthCheck,
    DaemonDetach,
}

pub(super) fn remote_server_restart_reason(
    protocol: Option<u32>,
    detached_server_daemon: bool,
    require_surface_interest: bool,
    surface_interest: bool,
    health_check: bool,
) -> Option<RemoteServerRestartReason> {
    if protocol != Some(crate::protocol::PROTOCOL_VERSION) {
        return Some(RemoteServerRestartReason::EndpointProtocol);
    }
    if require_surface_interest && !surface_interest {
        return Some(RemoteServerRestartReason::SurfaceInterest);
    }
    if require_surface_interest && !health_check {
        return Some(RemoteServerRestartReason::HealthCheck);
    }
    if !detached_server_daemon {
        return Some(RemoteServerRestartReason::DaemonDetach);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_floor_server_requires_update() {
        assert_eq!(
            remote_server_restart_reason(None, true, false, false, false),
            Some(RemoteServerRestartReason::EndpointProtocol)
        );
    }

    #[test]
    fn compatible_server_can_keep_running() {
        assert_eq!(
            remote_server_restart_reason(
                Some(crate::protocol::PROTOCOL_VERSION),
                true,
                false,
                false,
                false
            ),
            None
        );
    }

    #[test]
    fn saved_endpoint_requires_surface_interest() {
        assert_eq!(
            remote_server_restart_reason(
                Some(crate::protocol::PROTOCOL_VERSION),
                true,
                true,
                false,
                false
            ),
            Some(RemoteServerRestartReason::SurfaceInterest)
        );
    }

    #[test]
    fn saved_endpoint_requires_health_checks() {
        assert_eq!(
            remote_server_restart_reason(
                Some(crate::protocol::PROTOCOL_VERSION),
                true,
                true,
                true,
                false
            ),
            Some(RemoteServerRestartReason::HealthCheck)
        );
    }

    #[test]
    fn saved_endpoint_accepts_complete_lifecycle_support() {
        assert_eq!(
            remote_server_restart_reason(
                Some(crate::protocol::PROTOCOL_VERSION),
                true,
                true,
                true,
                true
            ),
            None
        );
    }

    #[test]
    fn old_daemon_requires_restart() {
        assert_eq!(
            remote_server_restart_reason(
                Some(crate::protocol::PROTOCOL_VERSION),
                false,
                false,
                false,
                false
            ),
            Some(RemoteServerRestartReason::DaemonDetach)
        );
    }
}
