//! Operator text naming the commands that reach a server.
//!
//! The text needs the server's address (its socket override) and this build's
//! entry point, and every launcher shows it: the CLI, the local server
//! launcher and the TUI client, and the `shepr-server` executable's ready
//! notice names the client to run. Each public function resolves its entry
//! point itself ([`operator_entrypoint`], or the server's sibling client for
//! [`server_ready_hint`]); the `_with` forms take the entry point or the
//! executable as an argument so tests can pin it.

use std::path::Path;

use shepr_paths::{BuildProfile, ServerAddress};

use crate::invocation::{COMMAND_STOP, PROGRAM_NAME};

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

/// The one-line hint `shepr status` prints under a server of another build at
/// `address`: stop it and start this build, or, at a socket override this
/// client cannot start a server at, only how to stop it.
pub fn status_build_mismatch_hint(address: &ServerAddress) -> String {
    status_build_mismatch_hint_with(address, &operator_entrypoint())
}

/// What the TUI tells the operator on the restored terminal after they
/// detached from the server at `address`: how to attach again and how to stop
/// the server. `machines_configured` adds that the servers on configured
/// machines were left running too.
pub fn detach_guidance(address: &ServerAddress, machines_configured: bool) -> String {
    detach_guidance_with(address, &operator_entrypoint(), machines_configured)
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
    address.command(&format!("{entrypoint} {COMMAND_STOP}"))
}

fn detach_guidance_with(
    address: &ServerAddress,
    entrypoint: &str,
    machines_configured: bool,
) -> String {
    let attach_command = attach_command_with(address, entrypoint);
    let stop_command = stop_command_with(address, entrypoint);
    let mut guidance = format!(
        "Detached. Run `{attach_command}` to re-attach, or `{stop_command}` to stop the local server and everything running in it."
    );
    if machines_configured {
        guidance.push_str(" Servers on configured machines keep running.");
    }
    guidance
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

fn status_build_mismatch_hint_with(address: &ServerAddress, entrypoint: &str) -> String {
    let stop_command = stop_command_with(address, entrypoint);
    if !address.is_runtime_address() {
        return format!(
            "this shepr cannot start a server at the socket override; run `{stop_command}` to stop it"
        );
    }
    let attach_command = attach_command_with(address, entrypoint);
    format!("run `{stop_command}`, then `{attach_command}`")
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

/// The line a foreground `shepr-server` ends its ready notice with, naming the
/// client command that opens the TUI: `shepr` for a release build, which is
/// the one installed on the path, and for a dev build the `shepr` client
/// beside the running server executable, or `brokkr run --` when there is
/// none.
pub fn server_ready_hint() -> String {
    let entrypoint = match BuildProfile::current() {
        BuildProfile::Release => PROGRAM_NAME.to_owned(),
        BuildProfile::Dev => {
            let server_executable = shepr_platform::launch_executable().ok();
            server_ready_entrypoint_with(
                BuildProfile::Dev,
                server_executable.as_deref(),
                executable_file,
            )
        }
    };
    format!(
        "did you mean to open the Shepr TUI? run `{entrypoint}`, which starts the server itself."
    )
}

fn server_ready_entrypoint_with(
    profile: BuildProfile,
    server_executable: Option<&Path>,
    is_executable: impl FnOnce(&Path) -> bool,
) -> String {
    match profile {
        BuildProfile::Release => PROGRAM_NAME.to_owned(),
        BuildProfile::Dev => {
            let Some(server_executable) = server_executable else {
                return "brokkr run --".to_owned();
            };
            let client_executable = server_executable.with_file_name(PROGRAM_NAME);
            if is_executable(&client_executable) {
                shepr_core::shell_quote::quote(&client_executable.to_string_lossy())
            } else {
                "brokkr run --".to_owned()
            }
        }
    }
}

fn executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        && shepr_platform::has_execute_access(path)
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
        assert_eq!(stop_command_with(&address, "shepr"), "shepr stop");
    }

    #[test]
    fn the_default_entry_point_is_this_builds() {
        let address = runtime_address();
        let entrypoint = operator_entrypoint();
        assert_eq!(attach_command(&address), entrypoint);
        assert_eq!(
            detach_guidance(&address, false),
            detach_guidance_with(&address, &entrypoint, false)
        );
        assert_eq!(
            build_mismatch_guidance(&address),
            build_mismatch_guidance_with(&address, &entrypoint)
        );
        assert_eq!(
            status_build_mismatch_hint(&address),
            status_build_mismatch_hint_with(&address, &entrypoint)
        );
    }

    #[test]
    fn a_server_ready_notice_uses_the_sibling_dev_client() {
        assert_eq!(
            server_ready_entrypoint_with(
                BuildProfile::Dev,
                Some(Path::new("/src/shepr/target/debug/shepr-server")),
                |_| true,
            ),
            "/src/shepr/target/debug/shepr"
        );
        assert_eq!(
            server_ready_entrypoint_with(
                BuildProfile::Dev,
                Some(Path::new("/src/shepr/target/debug/shepr-server")),
                |_| false,
            ),
            "brokkr run --"
        );
        assert_eq!(
            server_ready_entrypoint_with(BuildProfile::Dev, None, |_| false),
            "brokkr run --"
        );
        assert_eq!(
            server_ready_entrypoint_with(BuildProfile::Release, None, |_| false),
            "shepr"
        );
    }

    #[test]
    fn the_status_hint_restarts_a_runtime_address_and_only_stops_an_override() {
        assert_eq!(
            status_build_mismatch_hint_with(&runtime_address(), "shepr"),
            "run `shepr stop`, then `shepr`"
        );
        assert_eq!(
            status_build_mismatch_hint_with(&runtime_address(), "/src/shepr/target/debug/shepr"),
            "run `/src/shepr/target/debug/shepr stop`, then `/src/shepr/target/debug/shepr`"
        );
        assert_eq!(
            status_build_mismatch_hint_with(&overridden_address("/x/a.sock"), "shepr"),
            "this shepr cannot start a server at the socket override; run `SHEPR_SOCKET_PATH=/x/a.sock shepr stop` to stop it"
        );
    }

    #[test]
    fn build_mismatch_guidance_names_the_stop_and_attach_commands() {
        let guidance = build_mismatch_guidance_with(&runtime_address(), "shepr");
        assert_eq!(
            guidance,
            "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes. Run `shepr stop`, then run `shepr` again."
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
            "To keep the running server and its panes, keep using the shepr build that started it.\nThis shepr cannot start a server at the selected socket override, so it cannot restart this address. To stop the running server anyway, run `SHEPR_SOCKET_PATH=/x/a.sock shepr stop`."
        );
    }

    #[test]
    fn override_guidance_names_the_override() {
        let address = overridden_address("/x/a b.sock");
        assert_eq!(
            stop_command_with(&address, "shepr"),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr stop"
        );
        assert_eq!(
            attach_command_with(&address, "shepr"),
            "SHEPR_SOCKET_PATH='/x/a b.sock' shepr"
        );
    }

    #[test]
    fn detach_guidance_names_the_attach_and_stop_commands() {
        assert_eq!(
            detach_guidance_with(&runtime_address(), "shepr", false),
            "Detached. Run `shepr` to re-attach, or `shepr stop` to stop the local server and everything running in it."
        );
        assert_eq!(
            detach_guidance_with(&runtime_address(), "shepr", true),
            "Detached. Run `shepr` to re-attach, or `shepr stop` to stop the local server and everything running in it. Servers on configured machines keep running."
        );
    }

    #[test]
    fn detach_guidance_follows_the_entry_point_and_the_socket_override() {
        assert_eq!(
            detach_guidance_with(
                &overridden_address("/x/a.sock"),
                "/src/shepr/target/debug/shepr",
                false
            ),
            "Detached. Run `SHEPR_SOCKET_PATH=/x/a.sock /src/shepr/target/debug/shepr` to re-attach, or `SHEPR_SOCKET_PATH=/x/a.sock /src/shepr/target/debug/shepr stop` to stop the local server and everything running in it."
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
