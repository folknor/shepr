use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Legacy environment variable for overriding the client socket path.
///
/// Contractual override behavior for auto-detect uses `SHEPR_SOCKET_PATH`.
/// This variable is kept as a fallback for callers that explicitly need a
/// client-only override when `SHEPR_SOCKET_PATH` is not set.
pub const CLIENT_SOCKET_PATH_ENV_VAR: &str = "SHEPR_CLIENT_SOCKET_PATH";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerAddress {
    api_socket: PathBuf,
    client_socket: PathBuf,
    source: AddressSource,
}

// Only for `AppPaths::default()` in tests; production addresses come from
// `resolve`, never from relative placeholder paths.
#[cfg(test)]
impl Default for ServerAddress {
    fn default() -> Self {
        Self {
            api_socket: PathBuf::from("shepr.sock"),
            client_socket: PathBuf::from("shepr-client.sock"),
            source: AddressSource::Session,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum AddressSource {
    Session,
    ApiOverride,
    ClientOverride,
}

impl ServerAddress {
    pub(crate) fn resolve(
        config_dir: &Path,
        session: &crate::session::SessionId,
        session_was_requested: bool,
        api_socket_override: Option<&str>,
        client_socket_override: Option<&str>,
    ) -> Self {
        let session_api = session.api_socket_path_under(config_dir);
        if session_was_requested {
            return Self::for_session(session_api);
        }
        if let Some(api_socket) = api_socket_override {
            let api_socket = PathBuf::from(api_socket);
            return Self {
                client_socket: derive_client_socket_from_api_socket(&api_socket),
                api_socket,
                source: AddressSource::ApiOverride,
            };
        }
        if let Some(client_socket) = client_socket_override {
            return Self {
                api_socket: session_api,
                client_socket: PathBuf::from(client_socket),
                source: AddressSource::ClientOverride,
            };
        }
        Self::for_session(session_api)
    }

    fn for_session(api_socket: PathBuf) -> Self {
        let client_socket = derive_client_socket_from_api_socket(&api_socket);
        Self {
            api_socket,
            client_socket,
            source: AddressSource::Session,
        }
    }

    pub fn api_socket(&self) -> &Path {
        &self.api_socket
    }

    pub fn client_socket(&self) -> &Path {
        &self.client_socket
    }

    pub fn attach_command(&self, session: &crate::session::SessionId) -> String {
        match self.source {
            AddressSource::Session => session.attach_command(),
            AddressSource::ApiOverride => {
                self.command_with_api_override(session, &self.api_socket, "shepr")
            }
            AddressSource::ClientOverride => {
                self.command_with_client_override(session, &self.client_socket, "shepr")
            }
        }
    }

    pub fn stop_command(&self, session: &crate::session::SessionId) -> String {
        match self.source {
            AddressSource::Session => session.stop_command(),
            AddressSource::ApiOverride => {
                self.command_with_api_override(session, &self.api_socket, "shepr server stop")
            }
            AddressSource::ClientOverride => {
                self.command_with_client_override(session, &self.client_socket, "shepr server stop")
            }
        }
    }

    fn command_with_api_override(
        &self,
        session: &crate::session::SessionId,
        api_socket: &Path,
        command: &str,
    ) -> String {
        format!(
            "{}={} {}={} {command}",
            crate::session::SESSION_ENV_VAR,
            crate::remote::shell_quote(session.display_name()),
            crate::api::SOCKET_PATH_ENV_VAR,
            crate::remote::shell_quote(&api_socket.to_string_lossy())
        )
    }

    fn command_with_client_override(
        &self,
        session: &crate::session::SessionId,
        client_socket: &Path,
        command: &str,
    ) -> String {
        format!(
            "{}={} {}={} {command}",
            crate::session::SESSION_ENV_VAR,
            crate::remote::shell_quote(session.display_name()),
            CLIENT_SOCKET_PATH_ENV_VAR,
            crate::remote::shell_quote(&client_socket.to_string_lossy())
        )
    }

    pub(crate) fn apply_to_child_command(&self, command: &mut Command) {
        match self.source {
            AddressSource::Session => {
                command
                    .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
                    .env_remove(CLIENT_SOCKET_PATH_ENV_VAR);
            }
            AddressSource::ApiOverride => {
                command
                    .env(crate::api::SOCKET_PATH_ENV_VAR, &self.api_socket)
                    .env_remove(CLIENT_SOCKET_PATH_ENV_VAR);
            }
            AddressSource::ClientOverride => {
                command
                    .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
                    .env(CLIENT_SOCKET_PATH_ENV_VAR, &self.client_socket);
            }
        }
    }
}

/// Returns the resolved client protocol socket for this process.
pub fn client_socket_path(paths: &crate::config::AppPaths) -> PathBuf {
    paths.server_address().client_socket().to_path_buf()
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
        let address = ServerAddress::resolve(
            Path::new("/tmp"),
            &crate::session::SessionId::Default,
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
            &crate::session::SessionId::Default,
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
        let session = crate::session::SessionId::parse("work").expect("test precondition");
        let address = ServerAddress::resolve(
            Path::new("/tmp/config"),
            &session,
            true,
            Some("/tmp/other-api.sock"),
            Some("/tmp/other-client.sock"),
        );

        assert_eq!(
            address.api_socket(),
            Path::new("/tmp/config/sessions/work/shepr.sock")
        );
        assert_eq!(
            address.client_socket(),
            Path::new("/tmp/config/sessions/work/shepr-client.sock")
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
            &crate::session::SessionId::Named(
                crate::session::SessionName::parse("work").expect("test precondition"),
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
    fn client_socket_path_defaults_to_config_dir() {
        let session = crate::session::SessionId::Default;
        let address = ServerAddress::resolve(Path::new("/tmp/config"), &session, false, None, None);
        assert_eq!(
            address.client_socket(),
            Path::new("/tmp/config/shepr-client.sock")
        );
    }

    #[test]
    fn named_session_client_socket_matches_derived_api_socket_name() {
        let session = crate::session::SessionId::parse("work").expect("test precondition");
        let api = session.api_socket_path_under(Path::new("/tmp/config"));
        let client = session.client_socket_path_under(Path::new("/tmp/config"));
        let derived = derive_client_socket_from_api_socket(&api);
        assert_eq!(client, derived);
        assert_eq!(
            client,
            Path::new("/tmp/config/sessions/work/shepr-client.sock")
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
