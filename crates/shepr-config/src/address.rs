use serde::{Deserialize, Serialize};
use shepr_core::env::EnvVar;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerAddress {
    api_socket: PathBuf,
    client_socket: PathBuf,
    source: AddressSource,
}

impl<'de> Deserialize<'de> for ServerAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            api_socket: PathBuf,
            client_socket: PathBuf,
            source: AddressSource,
        }

        let wire = Wire::deserialize(deserializer)?;
        let address = Self {
            api_socket: wire.api_socket,
            client_socket: wire.client_socket,
            source: wire.source,
        };
        address.validate_paths().map_err(serde::de::Error::custom)?;
        Ok(address)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum AddressSource {
    /// The build's runtime directory: no socket override applies.
    Runtime,
    ApiOverride,
    ClientOverride,
}

impl ServerAddress {
    pub(crate) fn validate_paths(&self) -> Result<(), String> {
        if !self.api_socket.is_absolute() {
            return Err("API socket path must be absolute".to_owned());
        }
        if !self.client_socket.is_absolute() {
            return Err("client socket path must be absolute".to_owned());
        }
        Ok(())
    }

    // Address construction needs a resolved runtime directory and the socket overrides.
    // A context-free default cannot describe the selected server target or guarantee valid paths.
    pub(crate) fn resolve_paths(
        runtime_dir: &Path,
        api_socket_override: Option<&Path>,
        client_socket_override: Option<&Path>,
    ) -> Self {
        let runtime_api = runtime_dir.join(API_SOCKET_FILE_NAME);
        if let Some(api_socket) = api_socket_override {
            let api_socket = api_socket.to_path_buf();
            return Self {
                client_socket: derive_client_socket_from_api_socket(&api_socket),
                api_socket,
                source: AddressSource::ApiOverride,
            };
        }
        if let Some(client_socket) = client_socket_override {
            return Self {
                api_socket: runtime_api,
                client_socket: client_socket.to_path_buf(),
                source: AddressSource::ClientOverride,
            };
        }
        let client_socket = derive_client_socket_from_api_socket(&runtime_api);
        Self {
            api_socket: runtime_api,
            client_socket,
            source: AddressSource::Runtime,
        }
    }

    pub fn api_socket(&self) -> &Path {
        &self.api_socket
    }

    pub fn client_socket(&self) -> &Path {
        &self.client_socket
    }

    /// Whether this is the build profile's own runtime address, as opposed to
    /// one a socket override picked. Only the runtime address is one a client
    /// may start a server for.
    pub fn is_runtime_address(&self) -> bool {
        self.source == AddressSource::Runtime
    }

    /// The socket override variable that picked this address, if one did.
    pub fn override_variable(&self) -> Option<EnvVar> {
        match self.source {
            AddressSource::Runtime => None,
            AddressSource::ApiOverride => Some(EnvVar::SheprSocketPath),
            AddressSource::ClientOverride => Some(EnvVar::SheprClientSocketPath),
        }
    }

    /// The command that attaches to this server: plain `shepr` for the
    /// build's own runtime directory, prefixed with the socket override that
    /// selected it otherwise.
    pub fn attach_command(&self) -> String {
        self.command("shepr")
    }

    /// The command that stops this server, as [`attach_command`](Self::attach_command).
    pub fn stop_command(&self) -> String {
        self.command("shepr server stop")
    }

    /// What to tell an operator whose build met a running server of another
    /// build at this address. A dev and a release build keep separate runtime
    /// directories, so the way forward is to stop that server, which exits its
    /// panes, with the plain `server stop` command: it stops whatever server
    /// answers, whatever its build.
    pub fn build_mismatch_guidance(&self) -> String {
        let stop_command = self.stop_command();
        let attach_command = self.attach_command();
        format!(
            "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes. Run `{stop_command}`, then run `{attach_command}` again."
        )
    }

    fn command(&self, command: &str) -> String {
        match self.source {
            AddressSource::Runtime => command.to_owned(),
            AddressSource::ApiOverride => format!(
                "{}={} {command}",
                EnvVar::SheprSocketPath,
                shell_quote(&self.api_socket.to_string_lossy())
            ),
            AddressSource::ClientOverride => format!(
                "{}={} {command}",
                EnvVar::SheprClientSocketPath,
                shell_quote(&self.client_socket.to_string_lossy())
            ),
        }
    }

    pub fn apply_to_child_command(&self, command: &mut Command) {
        match self.source {
            AddressSource::Runtime => {
                command
                    .env_remove(EnvVar::SheprSocketPath)
                    .env_remove(EnvVar::SheprClientSocketPath);
            }
            AddressSource::ApiOverride => {
                command
                    .env(EnvVar::SheprSocketPath, &self.api_socket)
                    .env_remove(EnvVar::SheprClientSocketPath);
            }
            AddressSource::ClientOverride => {
                command
                    .env_remove(EnvVar::SheprSocketPath)
                    .env(EnvVar::SheprClientSocketPath, &self.client_socket);
            }
        }
    }
}

/// File name of the API socket inside a runtime directory.
const API_SOCKET_FILE_NAME: &str = "shepr.sock";

pub fn derive_client_socket_from_api_socket(api_socket_path: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_address_guidance_is_plain() {
        let address = ServerAddress::resolve_paths(Path::new("/run/user/1/shepr"), None, None);
        assert_eq!(
            address.api_socket(),
            Path::new("/run/user/1/shepr/shepr.sock")
        );
        assert_eq!(
            address.client_socket(),
            Path::new("/run/user/1/shepr/shepr-client.sock")
        );
        assert_eq!(address.attach_command(), "shepr");
        assert_eq!(address.stop_command(), "shepr server stop");
        assert!(address.is_runtime_address());
        assert_eq!(address.override_variable(), None);
    }

    #[test]
    fn an_override_is_not_the_runtime_address() {
        let runtime = Path::new("/run/user/1/shepr");
        let api = ServerAddress::resolve_paths(runtime, Some(Path::new("/x/a.sock")), None);
        assert!(!api.is_runtime_address());
        assert_eq!(api.override_variable(), Some(EnvVar::SheprSocketPath));
        let client = ServerAddress::resolve_paths(runtime, None, Some(Path::new("/x/c.sock")));
        assert!(!client.is_runtime_address());
        assert_eq!(
            client.override_variable(),
            Some(EnvVar::SheprClientSocketPath)
        );
    }

    const KEEP_GUIDANCE: &str = "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes.";

    #[test]
    fn build_mismatch_guidance_names_the_stop_and_attach_commands() {
        let runtime = Path::new("/run/user/1/shepr");
        let plain = ServerAddress::resolve_paths(runtime, None, None).build_mismatch_guidance();
        assert_eq!(
            plain,
            format!("{KEEP_GUIDANCE} Run `shepr server stop`, then run `shepr` again.")
        );
        for banned in ["--session", "SHEPR_SESSION", "--force"] {
            assert!(!plain.contains(banned), "{plain}");
        }
    }

    #[test]
    fn build_mismatch_guidance_keeps_the_socket_override() {
        let runtime = Path::new("/run/user/1/shepr");
        let api =
            ServerAddress::resolve_paths(runtime, Some(Path::new("/tmp/custom-shepr.sock")), None);
        assert_eq!(
            api.build_mismatch_guidance(),
            format!(
                "{KEEP_GUIDANCE} Run `SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr server stop`, then run `SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr` again."
            )
        );
        let client =
            ServerAddress::resolve_paths(runtime, None, Some(Path::new("/tmp/work-client.sock")));
        assert_eq!(
            client.build_mismatch_guidance(),
            format!(
                "{KEEP_GUIDANCE} Run `SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr server stop`, then run `SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr` again."
            )
        );
    }

    #[test]
    fn override_guidance_names_the_override() {
        let runtime = Path::new("/run/user/1/shepr");
        let api = ServerAddress::resolve_paths(runtime, Some(Path::new("/x/a b.sock")), None);
        assert_eq!(
            api.stop_command(),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr server stop"
        );
        let client = ServerAddress::resolve_paths(runtime, None, Some(Path::new("/x/c.sock")));
        assert_eq!(
            client.attach_command(),
            "SHEPR_CLIENT_SOCKET_PATH=/x/c.sock shepr"
        );
        assert_eq!(
            client.api_socket(),
            Path::new("/run/user/1/shepr/shepr.sock")
        );
    }
}
