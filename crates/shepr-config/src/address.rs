use shepr_core::env::EnvVar;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    api_socket: PathBuf,
    client_socket: PathBuf,
    source: AddressSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddressSource {
    /// The build's runtime directory: no socket override applies.
    Runtime,
    ApiOverride,
    ClientOverride,
}

impl ServerAddress {
    // Address construction needs a resolved runtime directory and the socket overrides.
    // A context-free default cannot describe the selected server target or guarantee valid paths.
    pub(crate) fn resolve_paths(
        runtime_dir: &Path,
        api_socket_override: Option<&Path>,
        client_socket_override: Option<&Path>,
    ) -> Self {
        let runtime_api = runtime_dir.join(API_SOCKET_FILE_NAME);
        let runtime_client = derive_client_socket_from_api_socket(&runtime_api);
        let api_socket_override = api_socket_override.filter(|path| *path != runtime_api.as_path());
        let client_socket_override =
            client_socket_override.filter(|path| *path != runtime_client.as_path());
        // A client socket override can pair a custom client socket with the
        // profile's API socket. The TUI attaches through the client socket,
        // and server stop waits for both resolved sockets to close. An
        // override therefore names an existing server; the TUI will not start
        // one there. A non-runtime API override still selects a different
        // server when both variables are set. Inherited values equal to the
        // runtime paths are not overrides.
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
        Self {
            api_socket: runtime_api,
            client_socket: runtime_client,
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

    /// The command that attaches to this server: `entrypoint` (normally
    /// [`operator_entrypoint`]), prefixed with the socket override that
    /// selected the server, if one did. Socket resolution cannot tell which
    /// executable the operator must run, so the caller names it.
    pub fn attach_command(&self, entrypoint: &str) -> String {
        self.command(entrypoint)
    }

    /// The command that stops this server, as [`attach_command`](Self::attach_command).
    pub fn stop_command(&self, entrypoint: &str) -> String {
        self.command(&format!("{entrypoint} server stop"))
    }

    /// What to tell an operator whose build met a running server of another
    /// build at this address. A dev and a release build keep separate runtime
    /// directories, so a runtime address can be switched by stopping the old
    /// server and starting this build. A socket override only names an existing
    /// server; this client cannot start its replacement there.
    pub fn build_mismatch_guidance(&self, entrypoint: &str) -> String {
        let stop_command = self.stop_command(entrypoint);
        if !self.is_runtime_address() {
            return format!(
                "To keep the running server and its panes, keep using the shepr build that started it.\nThis shepr cannot start a server at the selected socket override, so it cannot restart this address. To stop the running server anyway, run `{stop_command}`."
            );
        }
        let attach_command = self.attach_command(entrypoint);
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

/// The command an operator runs to reach this build, for the attach and stop
/// guidance above: `shepr` for a release build, which is the one installed on
/// the path. A dev build is not, so its guidance names the running executable,
/// or `brokkr run --` when that cannot be resolved.
pub fn operator_entrypoint() -> String {
    match crate::BuildProfile::current() {
        crate::BuildProfile::Release => "shepr".to_owned(),
        crate::BuildProfile::Dev => shepr_platform::launch_executable().map_or_else(
            |_| "brokkr run --".to_owned(),
            |path| shell_quote(&path.to_string_lossy()),
        ),
    }
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
        assert_eq!(address.attach_command("shepr"), "shepr");
        assert_eq!(address.stop_command("shepr"), "shepr server stop");
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

    #[test]
    fn api_socket_override_takes_precedence_over_client_socket_override() {
        let address = ServerAddress::resolve_paths(
            Path::new("/run/user/1/shepr"),
            Some(Path::new("/x/server.sock")),
            Some(Path::new("/x/other-client.sock")),
        );

        assert_eq!(address.api_socket(), Path::new("/x/server.sock"));
        assert_eq!(address.client_socket(), Path::new("/x/server-client.sock"));
        assert_eq!(address.override_variable(), Some(EnvVar::SheprSocketPath));
    }

    #[test]
    fn client_socket_override_pairs_with_the_runtime_api_socket() {
        // A client can attach to an existing server and server stop can wait
        // for it to release its separately selected client socket.
        let runtime = Path::new("/run/user/1/shepr");
        let address = ServerAddress::resolve_paths(
            runtime,
            Some(Path::new("/run/user/1/shepr/shepr.sock")),
            Some(Path::new("/x/work-client.sock")),
        );

        assert_eq!(
            address.api_socket(),
            Path::new("/run/user/1/shepr/shepr.sock")
        );
        assert_eq!(address.client_socket(), Path::new("/x/work-client.sock"));
        assert_eq!(
            address.override_variable(),
            Some(EnvVar::SheprClientSocketPath)
        );
    }

    #[test]
    fn pane_exported_runtime_sockets_are_not_overrides() {
        let runtime = Path::new("/run/user/1/shepr");
        let runtime_api = runtime.join("shepr.sock");
        let runtime_client = runtime.join("shepr-client.sock");
        let address = ServerAddress::resolve_paths(
            runtime,
            Some(runtime_api.as_path()),
            Some(runtime_client.as_path()),
        );

        assert!(address.is_runtime_address());
        assert_eq!(address.override_variable(), None);
        assert_eq!(address.api_socket(), runtime_api.as_path());
        assert_eq!(address.client_socket(), runtime_client.as_path());
        assert_eq!(address.stop_command("shepr"), "shepr server stop");

        let mut command = shepr_test_support::command_in_scratch("shepr", "address-env");
        address.apply_to_child_command(&mut command);
        let envs: Vec<_> = command.get_envs().collect();
        for variable in [EnvVar::SheprSocketPath, EnvVar::SheprClientSocketPath] {
            assert!(
                envs.iter().any(|(key, value)| {
                    *key == std::ffi::OsStr::new(variable.name()) && value.is_none()
                }),
                "{variable} must not be inherited as a socket override"
            );
        }
    }

    const KEEP_GUIDANCE: &str = "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes.";

    #[test]
    fn build_mismatch_guidance_names_the_stop_and_attach_commands() {
        let runtime = Path::new("/run/user/1/shepr");
        let plain =
            ServerAddress::resolve_paths(runtime, None, None).build_mismatch_guidance("shepr");
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
            api.build_mismatch_guidance("shepr"),
            format!(
                "{OVERRIDE_GUIDANCE} To stop the running server anyway, run `SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr server stop`."
            )
        );
        let client =
            ServerAddress::resolve_paths(runtime, None, Some(Path::new("/tmp/work-client.sock")));
        assert_eq!(
            client.build_mismatch_guidance("shepr"),
            format!(
                "{OVERRIDE_GUIDANCE} To stop the running server anyway, run `SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr server stop`."
            )
        );
    }

    #[test]
    fn override_guidance_names_the_override() {
        let runtime = Path::new("/run/user/1/shepr");
        let api = ServerAddress::resolve_paths(runtime, Some(Path::new("/x/a b.sock")), None);
        assert_eq!(
            api.stop_command("shepr"),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr server stop"
        );
        let client = ServerAddress::resolve_paths(runtime, None, Some(Path::new("/x/c.sock")));
        assert_eq!(
            client.attach_command("shepr"),
            "SHEPR_CLIENT_SOCKET_PATH=/x/c.sock shepr"
        );
        assert_eq!(
            client.api_socket(),
            Path::new("/run/user/1/shepr/shepr.sock")
        );
    }

    const OVERRIDE_GUIDANCE: &str = "To keep the running server and its panes, keep using the shepr build that started it.\nThis shepr cannot start a server at the selected socket override, so it cannot restart this address.";
}
