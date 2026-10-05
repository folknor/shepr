use shepr_core::env::EnvVar;
use shepr_core::socket_path::SocketPath;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{server_socket_path, socket_startup_lock_path};

/// The server socket this process targets: the build profile's runtime
/// `shepr.sock`, or the socket `SHEPR_SOCKET_PATH` selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    socket: SocketPath,
    /// A socket override picked the address, so it names an existing server
    /// that this client may attach to or stop but never start.
    overridden: bool,
}

impl ServerAddress {
    // Address construction needs a resolved runtime directory and the socket
    // override. A context-free default cannot describe the selected server
    // target or guarantee valid paths. An override naming the runtime socket
    // is not an override: a pane exports its server's resolved socket, and a
    // pane of the same profile must still count as the runtime address, also
    // when it reaches the socket through another spelling of the runtime
    // directory (a symlink), which is why the two are compared canonically.
    /// Resolve and retain the checked socket pathname for the entire launch.
    pub fn for_runtime_dir(
        runtime_dir: &Path,
        socket_override: Option<&Path>,
    ) -> std::io::Result<Self> {
        let runtime = server_socket_path(runtime_dir);
        let socket_override = socket_override.filter(|path| !same_socket_path(path, &runtime));
        Ok(Self {
            socket: SocketPath::new(socket_override.map_or(runtime, Path::to_path_buf))?,
            overridden: socket_override.is_some(),
        })
    }

    pub fn socket_path(&self) -> &SocketPath {
        &self.socket
    }

    pub fn socket(&self) -> &Path {
        self.socket.as_path()
    }

    /// The persistent startup-lock sidecar for this selected server socket.
    pub fn startup_lock_path(&self) -> PathBuf {
        socket_startup_lock_path(self.socket())
    }

    /// Whether this is the build profile's own runtime address, as opposed to
    /// one a socket override picked. Only the runtime address is one a client
    /// may start a server for.
    pub fn is_runtime_address(&self) -> bool {
        !self.overridden
    }

    /// `command` as an operator runs it against this server: prefixed with
    /// the socket override that selected the server, if one did.
    pub fn command(&self, command: &str) -> String {
        if self.overridden {
            format!(
                "{}={} {command}",
                EnvVar::SheprSocketPath,
                shepr_core::shell_quote::quote(&self.socket.as_path().to_string_lossy())
            )
        } else {
            command.to_owned()
        }
    }

    /// Clears the inherited socket selector from a daemon child command. The
    /// local launcher requires its runtime address before it spawns the
    /// daemon, so a child must never inherit a selector for an existing
    /// server.
    pub fn apply_to_child_command(command: &mut Command) {
        command.env_remove(EnvVar::SheprSocketPath);
    }
}

/// Compare socket pathnames after resolving existing path components. The
/// socket usually does not exist yet, so compare the canonical parent and the
/// final component when canonicalizing the full path cannot succeed.
fn same_socket_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    canonical_socket_path(left)
        .zip(canonical_socket_path(right))
        .is_some_and(|(left, right)| left == right)
}

fn canonical_socket_path(path: &Path) -> Option<std::path::PathBuf> {
    if let Ok(path) = std::fs::canonicalize(path) {
        return Some(path);
    }

    let parent = path.parent()?;
    let file_name = path.file_name()?;
    Some(std::fs::canonicalize(parent).ok()?.join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ServerAddress {
        pub(crate) fn resolve_paths(runtime_dir: &Path, socket_override: Option<&Path>) -> Self {
            Self::for_runtime_dir(runtime_dir, socket_override).expect("valid test socket path")
        }
    }

    #[test]
    fn a_leading_equals_sign_is_quoted_for_zsh() {
        assert_eq!(shepr_core::shell_quote::quote("=shepr"), "'=shepr'");
        assert_eq!(shepr_core::shell_quote::quote("/x/a=b.sock"), "/x/a=b.sock");
    }

    #[test]
    fn the_runtime_address_is_its_runtime_socket() {
        let address = ServerAddress::resolve_paths(Path::new("/run/user/1/shepr"), None);
        assert_eq!(address.socket(), Path::new("/run/user/1/shepr/shepr.sock"));
        assert_eq!(
            address.startup_lock_path().as_path(),
            Path::new("/run/user/1/shepr/shepr.sock.lock")
        );
        assert!(address.is_runtime_address());
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
        let socket = server_socket_path(runtime);
        let address = ServerAddress::resolve_paths(runtime, Some(&socket));
        assert!(address.is_runtime_address());
        assert_eq!(address.socket(), socket);
        assert_eq!(address.command("shepr stop"), "shepr stop");
        let mut command = shepr_test_support::command_in_scratch("shepr", "address-env");
        ServerAddress::apply_to_child_command(&mut command);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == EnvVar::SheprSocketPath.name() && value.is_none())
        );
    }

    /// The runtime socket reached through a symlinked runtime directory is
    /// still the runtime address, whether or not the socket exists yet.
    #[test]
    fn the_runtime_socket_through_a_symlink_is_not_an_override() {
        let scratch = shepr_test_support::ScratchDir::new("address-symlink");
        let runtime = scratch.join("runtime");
        std::fs::create_dir_all(&runtime).expect("test precondition");
        let alias = scratch.join("alias");
        std::os::unix::fs::symlink(&runtime, &alias).expect("test precondition");
        let through_alias = server_socket_path(&alias);

        let address = ServerAddress::resolve_paths(&runtime, Some(&through_alias));
        assert!(address.is_runtime_address());
        assert_eq!(address.socket(), server_socket_path(&runtime));

        std::fs::write(server_socket_path(&runtime), b"").expect("test precondition");
        let address = ServerAddress::resolve_paths(&runtime, Some(&through_alias));
        assert!(address.is_runtime_address());

        let elsewhere = scratch.join("elsewhere.sock");
        assert!(!ServerAddress::resolve_paths(&runtime, Some(&elsewhere)).is_runtime_address());
    }
}
