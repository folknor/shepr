use std::io;
use std::path::{Path, PathBuf};

/// Legacy environment variable for overriding the client socket path.
///
/// Contractual override behavior for auto-detect uses `SHEPR_SOCKET_PATH`.
/// This variable is kept as a fallback for callers that explicitly need a
/// client-only override when `SHEPR_SOCKET_PATH` is not set.
pub const CLIENT_SOCKET_PATH_ENV_VAR: &str = "SHEPR_CLIENT_SOCKET_PATH";

/// Returns the path for the client protocol socket.
///
/// Contract-aligned override behavior:
/// 1. If CLI `--session <name>` is active, use that session's client socket.
/// 2. If `SHEPR_SOCKET_PATH` is set, derive the client socket path from it by
///    inserting `-client` before `.sock` (e.g. `shepr.sock` -> `shepr-client.sock`).
///    This keeps JSON API and client socket overrides consistent.
/// 3. Otherwise, honor `SHEPR_CLIENT_SOCKET_PATH` (legacy/testing fallback).
/// 4. Otherwise, use the active session data directory.
pub fn client_socket_path(paths: &crate::config::AppPaths) -> PathBuf {
    if crate::session::explicit_session_requested() {
        return crate::session::client_socket_path_for(
            paths,
            crate::session::active_name().as_deref(),
        );
    }
    client_socket_path_from_overrides(
        paths,
        std::env::var(crate::api::SOCKET_PATH_ENV_VAR)
            .ok()
            .as_deref(),
        std::env::var(CLIENT_SOCKET_PATH_ENV_VAR).ok().as_deref(),
    )
}

pub(crate) fn client_socket_path_from_overrides(
    paths: &crate::config::AppPaths,
    api_socket_override: Option<&str>,
    client_socket_override: Option<&str>,
) -> PathBuf {
    if let Some(api_socket_override) = api_socket_override {
        return derive_client_socket_from_api_socket(Path::new(api_socket_override));
    }

    if let Some(client_socket_override) = client_socket_override {
        return PathBuf::from(client_socket_override);
    }

    crate::session::client_socket_path_for(paths, crate::session::active_name().as_deref())
}

pub(crate) fn derive_client_socket_from_api_socket(api_socket_path: &Path) -> PathBuf {
    let stem = api_socket_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shepr");
    let parent = api_socket_path.parent().unwrap_or_else(|| Path::new(""));

    parent.join(format!("{stem}-client.sock"))
}

/// Prepares a socket path for binding: creates parent directories,
/// removes stale socket files where no server is listening, and rejects live
/// sockets that are already in use.
///
/// This only keeps two servers off one socket. Session files follow the
/// session name, not the socket, so a server on an overridden socket can share
/// another server's data directory; the persistence layer guards that with a
/// lock of its own (`persist/lock.rs`).
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
        let paths = crate::config::AppPaths::default();
        let path = client_socket_path_from_overrides(&paths, Some("/tmp/test-shepr.sock"), None);
        assert_eq!(path, PathBuf::from("/tmp/test-shepr-client.sock"));
    }

    #[test]
    fn client_socket_path_api_override_takes_precedence_over_legacy_client_override() {
        let paths = crate::config::AppPaths::default();
        let path = client_socket_path_from_overrides(
            &paths,
            Some("/tmp/test-shepr.sock"),
            Some("/tmp/legacy-client.sock"),
        );
        assert_eq!(path, PathBuf::from("/tmp/test-shepr-client.sock"));
    }

    #[test]
    fn client_socket_path_respects_legacy_client_override_without_api_override() {
        let paths = crate::config::AppPaths::default();
        let path =
            client_socket_path_from_overrides(&paths, None, Some("/tmp/test-shepr-client.sock"));
        assert_eq!(path, PathBuf::from("/tmp/test-shepr-client.sock"));
    }

    #[test]
    fn client_socket_path_defaults_to_config_dir() {
        let scratch = crate::test_support::ScratchDir::new("socket-path");
        let paths = crate::config::AppPaths::test_at(scratch.path());
        let path = client_socket_path_from_overrides(&paths, None, None);
        assert_eq!(path, paths.config_dir().join("shepr-client.sock"));
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
