//! Operator text naming the commands that reach a [`ServerAddress`].
//!
//! The text needs the address (its socket override) and this build's entry
//! point, and every launcher shows it: the CLI, the local server launcher and
//! the TUI client, which links neither the API nor the server. Each public
//! method names [`operator_entrypoint`] itself; the `_with` forms take the
//! entry point as an argument so tests can pin it.

use crate::{BuildProfile, ServerAddress};

impl ServerAddress {
    /// The command that attaches to this server: [`operator_entrypoint`],
    /// prefixed with the socket override that selected the server, if one
    /// did.
    pub fn attach_command(&self) -> String {
        self.attach_command_with(&operator_entrypoint())
    }

    /// What to tell an operator whose build met a running server of another
    /// build at this address. A dev and a release build keep separate runtime
    /// directories, so a runtime address can be switched by stopping the old
    /// server and starting this build. A socket override only names an existing
    /// server; this client cannot start its replacement there.
    pub fn build_mismatch_guidance(&self) -> String {
        self.build_mismatch_guidance_with(&operator_entrypoint())
    }

    fn attach_command_with(&self, entrypoint: &str) -> String {
        self.command(entrypoint)
    }

    fn stop_command_with(&self, entrypoint: &str) -> String {
        self.command(&format!("{entrypoint} server stop"))
    }

    fn build_mismatch_guidance_with(&self, entrypoint: &str) -> String {
        let stop_command = self.stop_command_with(entrypoint);
        if !self.is_runtime_address() {
            return format!(
                "To keep the running server and its panes, keep using the shepr build that started it.\nThis shepr cannot start a server at the selected socket override, so it cannot restart this address. To stop the running server anyway, run `{stop_command}`."
            );
        }
        let attach_command = self.attach_command_with(entrypoint);
        format!(
            "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes. Run `{stop_command}`, then run `{attach_command}` again."
        )
    }
}

/// The command an operator runs to reach this build, for the attach and stop
/// guidance above: `shepr` for a release build, which is the one installed on
/// the path. A dev build is not, so its guidance names the running executable,
/// or `brokkr run --` when that cannot be resolved.
pub fn operator_entrypoint() -> String {
    match BuildProfile::current() {
        BuildProfile::Release => "shepr".to_owned(),
        BuildProfile::Dev => shepr_platform::launch_executable().map_or_else(
            |_| "brokkr run --".to_owned(),
            |path| shepr_core::shell_quote::quote(&path.to_string_lossy()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn runtime_address_guidance_is_plain() {
        let address = ServerAddress::resolve_paths(Path::new("/run/user/1/shepr"), None);
        assert_eq!(address.attach_command_with("shepr"), "shepr");
        assert_eq!(address.stop_command_with("shepr"), "shepr server stop");
    }

    #[test]
    fn the_default_entry_point_is_this_builds() {
        let address = ServerAddress::resolve_paths(Path::new("/run/user/1/shepr"), None);
        let entrypoint = operator_entrypoint();
        assert_eq!(address.attach_command(), entrypoint);
        assert_eq!(
            address.build_mismatch_guidance(),
            address.build_mismatch_guidance_with(&entrypoint)
        );
    }

    #[test]
    fn build_mismatch_guidance_names_the_stop_and_attach_commands() {
        let address = ServerAddress::resolve_paths(Path::new("/run/user/1/shepr"), None);
        let guidance = address.build_mismatch_guidance_with("shepr");
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
        let guidance = address.build_mismatch_guidance_with("shepr");
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
            address.stop_command_with("shepr"),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr server stop"
        );
        assert_eq!(
            address.attach_command_with("shepr"),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr"
        );
    }
}
