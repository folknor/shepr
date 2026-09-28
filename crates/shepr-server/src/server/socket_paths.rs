use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
use shepr_config::derive_client_socket_from_api_socket;

/// Returns the resolved client protocol socket for this process.
pub fn client_socket_path(paths: &shepr_config::AppPaths) -> PathBuf {
    paths.server_address().client_socket().to_path_buf()
}

/// Prepares a socket path for binding: creates parent directories,
/// removes stale socket files where no server is listening, and rejects live
/// sockets that are already in use.
///
/// This only keeps two servers off one socket. Session files follow the
/// session name, not the socket, so a server on an overridden socket can share
/// another server's data directory; the server claims that directory with a
/// persistence lease before it opens either socket.
pub(crate) fn prepare_socket_path(path: &Path) -> io::Result<()> {
    shepr_platform::ipc::prepare_socket_path(path, |path| {
        format!(
            "shepr server is already running (socket busy at {})",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::IsolatedEnv;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    #[test]
    fn client_socket_path_derived_from_api_socket_override() {
        let env = IsolatedEnv::new();
        env.set(
            shepr_core::env::EnvVar::SheprSocketPath,
            "/tmp/test-shepr.sock",
        );
        let paths = shepr_config::AppPaths::resolve().expect("API socket override resolves");
        assert_eq!(
            paths.server_address().client_socket(),
            Path::new("/tmp/test-shepr-client.sock")
        );
    }

    #[test]
    fn client_socket_path_api_override_takes_precedence_over_client_override() {
        let env = IsolatedEnv::new();
        env.set(
            shepr_core::env::EnvVar::SheprSocketPath,
            "/tmp/test-shepr.sock",
        );
        env.set(
            shepr_core::env::EnvVar::SheprClientSocketPath,
            "/tmp/client.sock",
        );
        let paths = shepr_config::AppPaths::resolve().expect("socket overrides resolve");
        assert_eq!(
            paths.server_address().client_socket(),
            Path::new("/tmp/test-shepr-client.sock")
        );
    }

    #[test]
    fn explicit_session_address_ignores_both_socket_overrides() {
        let env = IsolatedEnv::new();
        env.set(
            shepr_core::env::EnvVar::SheprSocketPath,
            "/tmp/other-api.sock",
        );
        env.set(
            shepr_core::env::EnvVar::SheprClientSocketPath,
            "/tmp/other-client.sock",
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
        env.set(
            shepr_core::env::EnvVar::SheprClientSocketPath,
            "/tmp/test-shepr-client.sock",
        );
        let paths = shepr_config::AppPaths::resolve().expect("client socket override resolves");
        let address = paths.server_address();
        let expected_api = paths.runtime_dir().join("shepr.sock");
        assert_eq!(
            address.client_socket(),
            Path::new("/tmp/test-shepr-client.sock")
        );
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
        let session = shepr_config::SessionId::parse("work").expect("test precondition");
        let api = session.api_socket_path_under(Path::new("/tmp/runtime"));
        let client = session.client_socket_path_under(Path::new("/tmp/runtime"));
        let derived = derive_client_socket_from_api_socket(&api);
        assert_eq!(client, derived);
        assert_eq!(
            client,
            Path::new("/tmp/runtime/sessions/work/shepr-client.sock")
        );
    }

    #[test]
    fn derive_client_socket_from_api_socket_without_sock_extension() {
        let derived = derive_client_socket_from_api_socket(Path::new("/tmp/custom-api"));
        assert_eq!(derived, PathBuf::from("/tmp/custom-api-client.sock"));
    }

    #[test]
    fn prepare_socket_path_removes_stale_socket() {
        let dir = crate::test_support::ScratchDir::new("stale");
        let socket_path = dir.join("stale.sock");

        {
            let _listener = UnixListener::bind(&socket_path).expect("bind stale socket");
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(&socket_path).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let result = prepare_socket_path(&socket_path);
        assert!(result.is_ok(), "should remove stale socket: {result:?}");
        assert!(!socket_path.exists());
    }

    #[test]
    fn prepare_socket_path_rejects_live_socket() {
        let dir = crate::test_support::ScratchDir::new("live");
        let socket_path = dir.join("live.sock");

        let _listener = UnixListener::bind(&socket_path).expect("bind");

        let result = prepare_socket_path(&socket_path);
        assert!(result.is_err());
        assert_eq!(
            result.expect_err("test precondition").kind(),
            io::ErrorKind::AddrInUse
        );
    }
}
