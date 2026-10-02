use shepr_core::env::EnvVar;
use shepr_core::socket_path::SocketPath;
use std::path::{Path, PathBuf};
use std::process::Command;

impl crate::machine::SshTarget {
    /// Append this checked destination as one command-line argument.
    pub fn append_to(&self, command: &mut Command) {
        command.arg(self.as_str());
    }

    /// Render this destination as one POSIX shell word for operator guidance.
    pub fn shell_word(&self) -> String {
        shepr_core::shell_quote::quote(self.as_str())
    }
}

/// The server socket this process targets: the build profile's runtime
/// `shepr.sock`, or the socket `SHEPR_SOCKET_PATH` selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    socket: PathBuf,
    /// A socket override picked the address, so it names an existing server
    /// that this client may attach to or stop but never start.
    overridden: bool,
}

impl ServerAddress {
    // Address construction needs a resolved runtime directory and the socket
    // override. A context-free default cannot describe the selected server
    // target or guarantee valid paths. An override equal to the runtime socket
    // is not an override: a pane exports its server's resolved socket, and a
    // pane of the same profile must still count as the runtime address.
    pub(crate) fn resolve_paths(runtime_dir: &Path, socket_override: Option<&Path>) -> Self {
        let runtime = runtime_dir.join(SOCKET_FILE_NAME);
        let socket_override = socket_override.filter(|path| *path != runtime);
        Self {
            socket: socket_override.map_or(runtime, Path::to_path_buf),
            overridden: socket_override.is_some(),
        }
    }

    /// [`Self::resolve_paths`], refusing a socket path no Unix socket can
    /// have, so a too-long runtime directory or override fails the launch
    /// instead of the later bind or connect.
    pub(crate) fn resolve_paths_checked(
        runtime_dir: &Path,
        socket_override: Option<&Path>,
    ) -> std::io::Result<Self> {
        let address = Self::resolve_paths(runtime_dir, socket_override);
        SocketPath::new(address.socket.clone())?;
        Ok(address)
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Whether this is the build profile's own runtime address, as opposed to
    /// one a socket override picked. Only the runtime address is one a client
    /// may start a server for.
    pub fn is_runtime_address(&self) -> bool {
        !self.overridden
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
        if self.overridden {
            format!(
                "{}={} {command}",
                EnvVar::SheprSocketPath,
                shepr_core::shell_quote::quote(&self.socket.to_string_lossy())
            )
        } else {
            command.to_owned()
        }
    }

    /// Clears the inherited socket selector from a daemon child command. The
    /// local launcher requires its runtime address before it spawns the
    /// daemon, so a child must never inherit a selector for an existing
    /// server.
    pub fn apply_to_child_command(&self, command: &mut Command) {
        command.env_remove(EnvVar::SheprSocketPath);
    }
}

/// File name of the server socket inside a runtime directory.
const SOCKET_FILE_NAME: &str = "shepr.sock";

/// The command an operator runs to reach this build, for the attach and stop
/// guidance above: `shepr` for a release build, which is the one installed on
/// the path. A dev build is not, so its guidance names the running executable,
/// or `brokkr run --` when that cannot be resolved.
pub fn operator_entrypoint() -> String {
    match crate::BuildProfile::current() {
        crate::BuildProfile::Release => "shepr".to_owned(),
        crate::BuildProfile::Dev => shepr_platform::launch_executable().map_or_else(
            |_| "brokkr run --".to_owned(),
            |path| shepr_core::shell_quote::quote(&path.to_string_lossy()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_equals_sign_is_quoted_for_zsh() {
        assert_eq!(shepr_core::shell_quote::quote("=shepr"), "'=shepr'");
        assert_eq!(shepr_core::shell_quote::quote("/x/a=b.sock"), "/x/a=b.sock");
    }

    #[test]
    fn runtime_address_guidance_is_plain() {
        let address = ServerAddress::resolve_paths(Path::new("/run/user/1/shepr"), None);
        assert_eq!(address.socket(), Path::new("/run/user/1/shepr/shepr.sock"));
        assert!(address.is_runtime_address());
        assert_eq!(address.attach_command("shepr"), "shepr");
        assert_eq!(address.stop_command("shepr"), "shepr server stop");
    }

    #[test]
    fn an_override_is_not_the_runtime_address() {
        let address = ServerAddress::resolve_paths(
            Path::new("/run/user/1/shepr"),
            Some(Path::new("/x/a.sock")),
        );
        assert!(!address.is_runtime_address());
        assert_eq!(address.socket(), Path::new("/x/a.sock"));
    }

    #[test]
    fn a_pane_exported_runtime_socket_is_not_an_override() {
        let runtime = Path::new("/run/user/1/shepr");
        let socket = runtime.join("shepr.sock");
        let address = ServerAddress::resolve_paths(runtime, Some(&socket));
        assert!(address.is_runtime_address());
        assert_eq!(address.socket(), socket);
        assert_eq!(address.stop_command("shepr"), "shepr server stop");
        let mut command = shepr_test_support::command_in_scratch("shepr", "address-env");
        address.apply_to_child_command(&mut command);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == EnvVar::SheprSocketPath.name() && value.is_none())
        );
    }

    #[test]
    fn build_mismatch_guidance_names_the_stop_and_attach_commands() {
        let address = ServerAddress::resolve_paths(Path::new("/run/user/1/shepr"), None);
        let guidance = address.build_mismatch_guidance("shepr");
        assert_eq!(
            guidance,
            "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes. Run `shepr server stop`, then run `shepr` again."
        );
        for banned in ["--session", "SHEPR_SESSION", "--force"] {
            assert!(!guidance.contains(banned), "{guidance}");
        }
    }

    #[test]
    fn build_mismatch_guidance_keeps_the_socket_override() {
        let address = ServerAddress::resolve_paths(
            Path::new("/run/user/1/shepr"),
            Some(Path::new("/x/a.sock")),
        );
        let guidance = address.build_mismatch_guidance("shepr");
        assert_eq!(
            guidance,
            "To keep the running server and its panes, keep using the shepr build that started it.\nThis shepr cannot start a server at the selected socket override, so it cannot restart this address. To stop the running server anyway, run `SHEPR_SOCKET_PATH=/x/a.sock shepr server stop`."
        );
    }

    #[test]
    fn override_guidance_names_the_override() {
        let address = ServerAddress::resolve_paths(
            Path::new("/run/user/1/shepr"),
            Some(Path::new("/x/a b.sock")),
        );
        assert_eq!(
            address.stop_command("shepr"),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr server stop"
        );
    }
}
