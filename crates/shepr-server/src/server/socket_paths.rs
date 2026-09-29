use std::path::PathBuf;

#[cfg(test)]
use shepr_config::derive_client_socket_from_api_socket;

/// Returns the resolved client protocol socket for this process.
pub fn client_socket_path(paths: &shepr_config::AppPaths) -> PathBuf {
    paths.server_address().client_socket().to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{IsolatedEnv, ScratchDir};

    #[test]
    fn client_socket_path_derived_from_api_socket_override() {
        let env = IsolatedEnv::new();
        let scratch = ScratchDir::new("socket-paths-api-override");
        let api_socket = scratch.join("test-shepr.sock");
        let expected_client = scratch.join("test-shepr-client.sock");
        env.set(
            shepr_core::env::EnvVar::SheprSocketPath,
            api_socket.as_os_str(),
        );
        let paths = shepr_config::AppPaths::resolve().expect("API socket override resolves");
        assert_eq!(
            paths.server_address().client_socket(),
            expected_client.as_path()
        );
    }

    #[test]
    fn client_socket_path_api_override_takes_precedence_over_client_override() {
        let env = IsolatedEnv::new();
        let scratch = ScratchDir::new("socket-paths-api-precedence");
        let api_socket = scratch.join("test-shepr.sock");
        let client_override = scratch.join("client.sock");
        let expected_client = scratch.join("test-shepr-client.sock");
        env.set(
            shepr_core::env::EnvVar::SheprSocketPath,
            api_socket.as_os_str(),
        );
        env.set(
            shepr_core::env::EnvVar::SheprClientSocketPath,
            client_override.as_os_str(),
        );
        let paths = shepr_config::AppPaths::resolve().expect("socket overrides resolve");
        assert_eq!(
            paths.server_address().client_socket(),
            expected_client.as_path()
        );
    }

    #[test]
    fn explicit_session_address_ignores_both_socket_overrides() {
        let env = IsolatedEnv::new();
        let scratch = ScratchDir::new("socket-paths-session-overrides");
        let api_socket = scratch.join("other-api.sock");
        let client_socket = scratch.join("other-client.sock");
        env.set(
            shepr_core::env::EnvVar::SheprSocketPath,
            api_socket.as_os_str(),
        );
        env.set(
            shepr_core::env::EnvVar::SheprClientSocketPath,
            client_socket.as_os_str(),
        );
        let session = shepr_config::SessionId::parse("work").expect("test precondition");
        let paths = shepr_config::AppPaths::resolve_with_session(Some(session.clone()))
            .expect("explicit session resolves");
        let address = paths.server_address();
        let expected_api = session.api_socket_path_under(paths.runtime_dir());
        let expected_client = session.client_socket_path_under(paths.runtime_dir());

        assert_eq!(address.api_socket(), expected_api.as_path());
        assert_eq!(address.client_socket(), expected_client.as_path());
        assert_eq!(
            address.attach_command(&session),
            "shepr session attach work"
        );
    }

    #[test]
    fn client_socket_path_respects_client_override_without_api_override() {
        let env = IsolatedEnv::new();
        let scratch = ScratchDir::new("socket-paths-client-override");
        let client_socket = scratch.join("test-shepr-client.sock");
        env.set(
            shepr_core::env::EnvVar::SheprClientSocketPath,
            client_socket.as_os_str(),
        );
        let paths = shepr_config::AppPaths::resolve().expect("client socket override resolves");
        let address = paths.server_address();
        let expected_api = paths.runtime_dir().join("shepr.sock");
        assert_eq!(address.client_socket(), client_socket.as_path());
        assert_eq!(address.api_socket(), expected_api.as_path());
    }

    #[test]
    fn client_socket_path_defaults_to_runtime_dir() {
        let _env = IsolatedEnv::new();
        let paths = shepr_config::AppPaths::resolve().expect("default paths resolve");
        let address = paths.server_address();
        let expected_client = paths.runtime_dir().join("shepr-client.sock");
        assert_eq!(address.client_socket(), expected_client.as_path());
    }

    #[test]
    fn named_session_client_socket_matches_derived_api_socket_name() {
        let runtime = ScratchDir::new("socket-paths-runtime");
        let session = shepr_config::SessionId::parse("work").expect("test precondition");
        let api = session.api_socket_path_under(runtime.path());
        let client = session.client_socket_path_under(runtime.path());
        let derived = derive_client_socket_from_api_socket(&api);
        assert_eq!(client, derived);
        assert_eq!(client, runtime.join("sessions/work/shepr-client.sock"));
    }

    #[test]
    fn derive_client_socket_from_api_socket_without_sock_extension() {
        let runtime = ScratchDir::new("socket-paths-without-extension");
        let api = runtime.join("custom-api");
        let derived = derive_client_socket_from_api_socket(&api);
        assert_eq!(derived, runtime.join("custom-api-client.sock"));
    }
}
