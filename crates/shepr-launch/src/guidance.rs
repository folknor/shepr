//! Operator text naming the commands that reach a server.
//!
//! The text needs the server's address (its socket override) and this build's
//! entry point, and every launcher shows it: the CLI, the local server
//! launcher and the TUI client. Each public function names
//! [`operator_entrypoint`] itself; the `_with` forms take the entry point as
//! an argument so tests can pin it.

use std::path::Path;

use shepr_paths::{BuildProfile, ServerAddress};

use crate::invocation::{COMMAND_SERVER, COMMAND_STOP, PROGRAM_NAME};

/// The command that attaches to the server at `address`:
/// [`operator_entrypoint`], prefixed with the socket override that selected
/// the server, if one did.
pub fn attach_command(address: &ServerAddress) -> String {
    attach_command_with(address, &operator_entrypoint())
}

/// What to tell an operator whose build met a running server of another build
/// at `address`. A dev and a release build keep separate runtime directories,
/// so a runtime address can be switched by stopping the old server and
/// starting this build. A socket override only names an existing server; this
/// client cannot start its replacement there.
pub fn build_mismatch_guidance(address: &ServerAddress) -> String {
    build_mismatch_guidance_with(address, &operator_entrypoint())
}

/// What to tell an operator whose command found no server listening at
/// `socket_path`, with the command that starts or attaches to it.
pub fn server_not_running(socket_path: &Path, attach_command: &str) -> String {
    format!(
        "no shepr server is running at {}; run `{attach_command}` to start or attach it",
        socket_path.display()
    )
}

fn attach_command_with(address: &ServerAddress, entrypoint: &str) -> String {
    address.command(entrypoint)
}

fn stop_command_with(address: &ServerAddress, entrypoint: &str) -> String {
    address.command(&format!("{entrypoint} {COMMAND_SERVER} {COMMAND_STOP}"))
}

fn build_mismatch_guidance_with(address: &ServerAddress, entrypoint: &str) -> String {
    let stop_command = stop_command_with(address, entrypoint);
    if !address.is_runtime_address() {
        return format!(
            "To keep the running server and its panes, keep using the shepr build that started it.\nThis shepr cannot start a server at the selected socket override, so it cannot restart this address. To stop the running server anyway, run `{stop_command}`."
        );
    }
    let attach_command = attach_command_with(address, entrypoint);
    format!(
        "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes. Run `{stop_command}`, then run `{attach_command}` again."
    )
}

/// The command an operator runs to reach this build, for the attach and stop
/// guidance above: `shepr` for a release build, which is the one installed on
/// the path. A dev build is not, so its guidance names the running executable,
/// or `brokkr run --` when that cannot be resolved.
pub fn operator_entrypoint() -> String {
    match BuildProfile::current() {
        BuildProfile::Release => PROGRAM_NAME.to_owned(),
        BuildProfile::Dev => shepr_platform::launch_executable().map_or_else(
            |_| "brokkr run --".to_owned(),
            |path| shepr_core::shell_quote::quote(&path.to_string_lossy()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_address() -> ServerAddress {
        ServerAddress::for_runtime_dir(Path::new("/run/user/1/shepr"), None)
            .expect("valid test socket path")
    }

    fn overridden_address(socket: &str) -> ServerAddress {
        ServerAddress::for_runtime_dir(Path::new("/run/user/1/shepr"), Some(Path::new(socket)))
            .expect("valid test socket path")
    }

    #[test]
    fn runtime_address_guidance_is_plain() {
        let address = runtime_address();
        assert_eq!(attach_command_with(&address, "shepr"), "shepr");
        assert_eq!(stop_command_with(&address, "shepr"), "shepr server stop");
    }

    #[test]
    fn the_default_entry_point_is_this_builds() {
        let address = runtime_address();
        let entrypoint = operator_entrypoint();
        assert_eq!(attach_command(&address), entrypoint);
        assert_eq!(
            build_mismatch_guidance(&address),
            build_mismatch_guidance_with(&address, &entrypoint)
        );
    }

    #[test]
    fn build_mismatch_guidance_names_the_stop_and_attach_commands() {
        let guidance = build_mismatch_guidance_with(&runtime_address(), "shepr");
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
        let guidance = build_mismatch_guidance_with(&overridden_address("/x/a.sock"), "shepr");
        assert_eq!(
            guidance,
            "To keep the running server and its panes, keep using the shepr build that started it.\nThis shepr cannot start a server at the selected socket override, so it cannot restart this address. To stop the running server anyway, run `SHEPR_SOCKET_PATH=/x/a.sock shepr server stop`."
        );
    }

    #[test]
    fn override_guidance_names_the_override() {
        let address = overridden_address("/x/a b.sock");
        assert_eq!(
            stop_command_with(&address, "shepr"),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr server stop"
        );
        assert_eq!(
            attach_command_with(&address, "shepr"),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr"
        );
    }

    #[test]
    fn a_missing_server_names_its_socket_and_the_attach_command() {
        assert_eq!(
            server_not_running(Path::new("/run/user/1/shepr/shepr.sock"), "shepr"),
            "no shepr server is running at /run/user/1/shepr/shepr.sock; run `shepr` to start or attach it"
        );
    }
}
