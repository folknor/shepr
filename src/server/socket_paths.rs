use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
use crate::config::{ServerAddress, derive_client_socket_from_api_socket};

/// Returns the resolved client protocol socket for this process.
pub fn client_socket_path(paths: &crate::config::AppPaths) -> PathBuf {
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
    crate::ipc::prepare_socket_path(path, |path| {
        format!(
            "shepr server is already running (socket busy at {})",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    #[test]
    fn client_socket_path_derived_from_api_socket_override() {
        let address = ServerAddress::resolve(
            Path::new("/tmp"),
            &crate::config::SessionId::Default,
            false,
            Some("/tmp/test-shepr.sock"),
            None,
        );
        assert_eq!(
            address.client_socket(),
            Path::new("/tmp/test-shepr-client.sock")
        );
    }

    #[test]
    fn client_socket_path_api_override_takes_precedence_over_legacy_client_override() {
        let address = ServerAddress::resolve(
            Path::new("/tmp"),
            &crate::config::SessionId::Default,
            false,
            Some("/tmp/test-shepr.sock"),
            Some("/tmp/legacy-client.sock"),
        );
        assert_eq!(
            address.client_socket(),
            Path::new("/tmp/test-shepr-client.sock")
        );
    }

    #[test]
    fn explicit_session_address_ignores_both_socket_overrides() {
        let session = crate::config::SessionId::parse("work").expect("test precondition");
        let address = ServerAddress::resolve(
            Path::new("/tmp/runtime"),
            &session,
            true,
            Some("/tmp/other-api.sock"),
            Some("/tmp/other-client.sock"),
        );

        assert_eq!(
            address.api_socket(),
            Path::new("/tmp/runtime/sessions/work/shepr.sock")
        );
        assert_eq!(
            address.client_socket(),
            Path::new("/tmp/runtime/sessions/work/shepr-client.sock")
        );
        assert_eq!(
            address.attach_command(&session),
            "shepr session attach work"
        );
    }

    #[test]
    fn client_socket_path_respects_legacy_client_override_without_api_override() {
        let address = ServerAddress::resolve(
            Path::new("/tmp"),
            &crate::config::SessionId::Named(
                crate::config::SessionName::parse("work").expect("test precondition"),
            ),
            false,
            None,
            Some("/tmp/test-shepr-client.sock"),
        );
        assert_eq!(
            address.client_socket(),
            Path::new("/tmp/test-shepr-client.sock")
        );
        assert_eq!(
            address.api_socket(),
            Path::new("/tmp/sessions/work/shepr.sock")
        );
    }

    #[test]
    fn client_socket_path_defaults_to_runtime_dir() {
        let session = crate::config::SessionId::Default;
        let address =
            ServerAddress::resolve(Path::new("/tmp/runtime"), &session, false, None, None);
        assert_eq!(
            address.client_socket(),
            Path::new("/tmp/runtime/shepr-client.sock")
        );
    }

    #[test]
    fn named_session_client_socket_matches_derived_api_socket_name() {
        let session = crate::config::SessionId::parse("work").expect("test precondition");
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
