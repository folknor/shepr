use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Legacy environment variable for overriding the client socket path.
///
/// Contractual override behavior for auto-detect uses `SHEPR_SOCKET_PATH`.
/// This variable is kept as a fallback for callers that explicitly need a
/// client-only override when `SHEPR_SOCKET_PATH` is not set.
pub const CLIENT_SOCKET_PATH_ENV_VAR: &str = "SHEPR_CLIENT_SOCKET_PATH";
pub const SOCKET_PATH_ENV_VAR: &str = "SHEPR_SOCKET_PATH";

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
        runtime_dir: &Path,
        session: &super::SessionId,
        session_was_requested: bool,
        api_socket_override: Option<&str>,
        client_socket_override: Option<&str>,
    ) -> Self {
        let session_api = session.api_socket_path_under(runtime_dir);
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

    pub fn attach_command(&self, session: &super::SessionId) -> String {
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

    pub fn stop_command(&self, session: &super::SessionId) -> String {
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
        session: &super::SessionId,
        api_socket: &Path,
        command: &str,
    ) -> String {
        format!(
            "{}={} {}={} {command}",
            super::SESSION_ENV_VAR,
            shell_quote(session.display_name()),
            SOCKET_PATH_ENV_VAR,
            shell_quote(&api_socket.to_string_lossy())
        )
    }

    fn command_with_client_override(
        &self,
        session: &super::SessionId,
        client_socket: &Path,
        command: &str,
    ) -> String {
        format!(
            "{}={} {}={} {command}",
            super::SESSION_ENV_VAR,
            shell_quote(session.display_name()),
            CLIENT_SOCKET_PATH_ENV_VAR,
            shell_quote(&client_socket.to_string_lossy())
        )
    }

    pub(crate) fn apply_to_child_command(&self, command: &mut Command) {
        match self.source {
            AddressSource::Session => {
                command
                    .env_remove(SOCKET_PATH_ENV_VAR)
                    .env_remove(CLIENT_SOCKET_PATH_ENV_VAR);
            }
            AddressSource::ApiOverride => {
                command
                    .env(SOCKET_PATH_ENV_VAR, &self.api_socket)
                    .env_remove(CLIENT_SOCKET_PATH_ENV_VAR);
            }
            AddressSource::ClientOverride => {
                command
                    .env_remove(SOCKET_PATH_ENV_VAR)
                    .env(CLIENT_SOCKET_PATH_ENV_VAR, &self.client_socket);
            }
        }
    }
}

pub(crate) fn derive_client_socket_from_api_socket(api_socket_path: &Path) -> PathBuf {
    let stem = api_socket_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shepr");
    let parent = api_socket_path.parent().unwrap_or_else(|| Path::new(""));

    parent.join(format!("{stem}-client.sock"))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}
