//! Operator guidance for launch, CLI, preflight and endpoint failures.
//!
//! The text needs the server's address (its socket override) and this build's
//! entry point, and every launcher shows it: the CLI, the local server
//! launcher and the TUI client, and the `shepr-server` executable's ready
//! notice names the client to run. Command-building functions resolve their entry
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
        "did you mean to open the shepr TUI? run `{entrypoint}`, which starts the server itself."
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

/// Usage guidance names this build's executable, including dev builds.
pub fn usage_hint() -> String {
    format!("run '{} --help' for usage", operator_entrypoint())
}

/// A TUI launch from one of its own profile's panes is refused.
pub const NESTED_REFUSAL: &str =
    "shepr does not run inside a pane of a server of its own build profile.";

/// A missing local connection is retried while other machines stay usable.
pub const LOCAL_RECONNECT_HINT: &str = "the local server is unavailable; start it to reconnect";

pub fn machine_login_hint(entrypoint: &str, ssh: &str) -> String {
    format!("run {entrypoint} again, or {ssh}")
}

pub fn fleet_login_hint(message: &impl std::fmt::Display) -> String {
    format!(
        "needs an SSH login: run `{}` or ssh to it ({message})",
        operator_entrypoint()
    )
}

pub fn local_startup_notice(error: &impl std::fmt::Display) -> String {
    format!("shepr: the local server is unavailable; configured machines stay available.\n{error}")
}

pub fn terminal_geometry_failure(error: &impl std::fmt::Display) -> String {
    format!("cannot attach without a usable terminal: {error}; run inside a terminal")
}

pub fn cli_build_mismatch(
    address: &ServerAddress,
    build_id: shepr_protocol::BuildIdentity,
) -> String {
    format!(
        "this shepr client (build {}) differs from the running server (build {build_id}); restart the server with this build before using this command. {}",
        shepr_protocol::BUILD_ID,
        build_mismatch_guidance(address)
    )
}

pub fn unresponsive_server(address: &ServerAddress) -> String {
    format!(
        "a shepr server is listening at {}, but it is not answering status requests, so its build cannot be confirmed and no second server is started.\n\n{}\nIf that fails, inspect the server log and stop the server process manually; forcing it to exit can lose the final save.",
        address.socket().display(),
        build_mismatch_guidance(address)
    )
}

pub fn running_build_mismatch(
    address: &ServerAddress,
    status: &crate::status::RuntimeStatus,
) -> String {
    let summary = if address.is_runtime_address() {
        "the running shepr server is a different build; restart it before attaching."
    } else {
        "the running shepr server is a different build, and this client cannot start a replacement at the selected socket override."
    };
    format!(
        "{summary}\n\nserver: v{} build {}\nclient: v{} build {}\n\n{}",
        status.version,
        status.build_id,
        shepr_protocol::build_version(),
        shepr_protocol::BUILD_ID,
        build_mismatch_guidance(address)
    )
}

pub fn no_server_at_override(address: &ServerAddress, runtime_dir: &Path) -> String {
    let selected_by = shepr_core::env::EnvVar::SheprSocketPath;
    format!(
        "no shepr server is running at {}, which {selected_by} selects. A client starts a server only for its own runtime address ({}); a socket override names a server that is already running.",
        address.socket().display(),
        runtime_dir.display()
    )
}

pub fn stop_timeout(label: &str, timeout: std::time::Duration, socket: &Path) -> String {
    format!(
        "{label} did not stop within {}ms; the socket at {} is still reachable. \
         The server may still be saving its layout; wait for shutdown to finish and \
         inspect the server log before retrying. Forcing the process to exit can lose \
         the final save",
        timeout.as_millis(),
        socket.display()
    )
}

pub fn local_install_hint() -> String {
    let server = crate::invocation::SERVER_BINARY_NAME;
    format!(
        "{PROGRAM_NAME} starts its server from the same directory as itself; install {PROGRAM_NAME} and {server} together (`brokkr install`)"
    )
}

pub fn server_already_running() -> String {
    format!(
        "{} is already running",
        crate::invocation::SERVER_BINARY_NAME
    )
}

/// Discovery classifies the installation; guidance renders the typed cause.
pub enum RemoteInstallationFailure<'a> {
    MissingSibling,
    UnusableSibling {
        binary: Option<&'a str>,
        error: &'a str,
    },
    DifferentSibling {
        version: &'a str,
        build_id: shepr_protocol::BuildIdentity,
    },
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the cause is a one-shot typed value built at the call site"
)]
pub fn remote_sibling_mismatch(
    target: &impl std::fmt::Display,
    cause: RemoteInstallationFailure<'_>,
) -> String {
    use crate::invocation::SERVER_BINARY_NAME;
    let install_hint = format!(
        "Install {PROGRAM_NAME} and {SERVER_BINARY_NAME} together from the same build on the host and retry"
    );
    match cause {
        RemoteInstallationFailure::MissingSibling => format!(
            "remote shepr installation error on {target}: {PROGRAM_NAME} did not report a {SERVER_BINARY_NAME} beside it. {install_hint}"
        ),
        RemoteInstallationFailure::UnusableSibling { binary, error } => {
            let binary = binary.map_or_else(String::new, |binary| format!(" ({binary})"));
            format!(
                "remote shepr installation error on {target}: {SERVER_BINARY_NAME}{binary} is unusable: {error}. {install_hint}"
            )
        }
        RemoteInstallationFailure::DifferentSibling { version, build_id } => {
            // Remote identity fields must remain printable single-line values.
            let version = if !version.is_empty()
                && version.chars().all(|ch| ch.is_ascii_graphic() || ch == ' ')
            {
                version
            } else {
                "unknown"
            };
            format!(
                "remote shepr installation error on {target}: the {SERVER_BINARY_NAME} beside {PROGRAM_NAME} is version {version} build {build_id}; this client is version {} build {}. {install_hint}",
                shepr_protocol::build_version(),
                shepr_protocol::BUILD_ID
            )
        }
    }
}

pub fn remote_client_mismatch(
    target: &impl std::fmt::Display,
    version: &impl std::fmt::Display,
    build_id: &str,
) -> String {
    let advice = if shepr_paths::BuildProfile::current() == shepr_paths::BuildProfile::Dev {
        "This is a dev client, which needs a dev build of shepr on the remote host; discovery only finds installed builds (normally release), so install a dev build there and retry"
    } else {
        "Install the same shepr build on the host and retry"
    };
    format!(
        "remote shepr compatibility error on {target}: found version {version} build {build_id}; this client is version {} build {}. {advice}",
        shepr_protocol::build_version(),
        shepr_protocol::BUILD_ID
    )
}

pub fn remote_install_not_ready(target: &impl std::fmt::Display, rejection: &str) -> String {
    format!(
        "matching {PROGRAM_NAME} is not ready on {target}{rejection}; install or update it there manually and retry"
    )
}

pub fn local_offer(status: &crate::status::RuntimeStatus) -> String {
    format!(
        "shepr: the local shepr server is a different build (server build {}, boot {}, this shepr build {}).\n\
         Restarting it stops that server, which ends every pane process it hosts.\n\
         The saved layout is restored with fresh shells, and agents are resumed where they can be.\n\
         Restart it now? [y/N] ",
        status.build_id,
        status.boot_id,
        shepr_protocol::BUILD_ID
    )
}

/// What the operator is told about the local server's restart. A server that
/// was kept running, or that no one could be asked about, is reported by the
/// launch that follows, with the stop command.
pub fn local_notice(local: &crate::restart::RestartResult) -> Option<String> {
    match local {
        crate::restart::RestartResult::NotNeeded | crate::restart::RestartResult::NoTerminal | crate::restart::RestartResult::Declined => None,
        crate::restart::RestartResult::Stopped => Some(
            "shepr: stopped the local server of a different build; one of this build starts now."
                .to_owned(),
        ),
        crate::restart::RestartResult::NoServer => Some(
            "shepr: the local server of a different build had already stopped; one of this build starts now."
                .to_owned(),
        ),
        crate::restart::RestartResult::OccupantChanged => Some(
            "shepr: the local server changed while it was being stopped; no stop was sent to a new occupant."
                .to_owned(),
        ),
        crate::restart::RestartResult::Failed(error) => {
            Some(format!("shepr: could not stop the local server: {error}"))
        }
    }
}

pub fn ssh_prompt_notice(
    label: &impl std::fmt::Display,
    target: &impl std::fmt::Display,
) -> String {
    format!("shepr: machine {label} ({target}) needs authentication; running ssh for it.")
}

/// Preflight decisions retain their typed cause until this wording boundary.
pub enum MachinePreflightNotice<'a> {
    AuthenticationFailed(&'a dyn std::fmt::Display),
    NoTerminal,
    AuthenticationRefused(&'a crate::EndpointFailure),
    HostKey(&'a crate::EndpointFailure),
    Incompatible(&'a crate::EndpointFailure),
    Failed(&'a crate::EndpointFailure),
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the notice is a one-shot typed value built at the call site"
)]
pub fn machine_preflight_notice(
    label: &impl std::fmt::Display,
    notice: MachinePreflightNotice<'_>,
) -> String {
    match notice {
        MachinePreflightNotice::AuthenticationFailed(error) => format!(
            "shepr: authentication for machine {label} failed: {error}. The client keeps retrying it."
        ),
        MachinePreflightNotice::NoTerminal => format!(
            "shepr: machine {label} needs authentication, but there is no terminal to prompt on; run `{}` from an interactive terminal.",
            operator_entrypoint()
        ),
        MachinePreflightNotice::AuthenticationRefused(failure) => format!(
            "shepr: machine {label} still refuses the client's connection after ssh authenticated: {failure}. The client keeps retrying it."
        ),
        MachinePreflightNotice::HostKey(failure) => format!("shepr: machine {label}: {failure}"),
        MachinePreflightNotice::Incompatible(failure) => format!(
            "shepr: machine {label} cannot be used: {failure}. {}",
            failure.disposition().client_action()
        ),
        MachinePreflightNotice::Failed(failure) => format!(
            "shepr: machine {label} could not be checked: {failure}. {}",
            failure.disposition().client_action()
        ),
    }
}

pub fn machine_failure_hints(
    failure: &crate::EndpointFailure,
    ssh_check_command: &str,
) -> Vec<String> {
    use crate::{FailureCause, SshFailureClass};
    match failure.cause() {
        FailureCause::Ssh(SshFailureClass::HostKey) => vec![
            "hint: configured machines use strict host-key checking; add the host key to the configured known_hosts file, then retry.".to_owned(),
        ],
        FailureCause::Ssh(SshFailureClass::Configuration) => vec![
            "hint: check the configured SSH target and local SSH configuration; OpenSSH reports the file and line for configuration errors.".to_owned(),
        ],
        FailureCause::Ssh(SshFailureClass::Authentication) => vec![
            format!("hint: verify SSH access first with `{ssh_check_command}`."),
            "hint: if your SSH key has a passphrase, load it into ssh-agent with `ssh-add` before retrying.".to_owned(),
        ],
        _ => Vec::new(),
    }
}

pub fn failure_client_action(disposition: crate::FailureDisposition) -> &'static str {
    if disposition.needs_attention() {
        "The client shows it as unavailable and needs attention; it keeps retrying it."
    } else {
        "The client keeps retrying it."
    }
}

/// The boot outcome is determined by the launcher; guidance only renders it.
pub enum ServerBootNotice {
    Exited {
        class: crate::daemon_exit::DaemonExit,
        status: std::process::ExitStatus,
    },
    LogOverflow {
        max_bytes: u64,
    },
    TimedOut {
        timeout: std::time::Duration,
        occupant_only: bool,
    },
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the notice is a one-shot typed value built at the call site"
)]
pub fn server_boot_notice(notice: ServerBootNotice) -> String {
    use crate::invocation::SERVER_BINARY_NAME;
    match notice {
        ServerBootNotice::Exited { class, status } => format!(
            "{SERVER_BINARY_NAME} {} ({status})",
            class.describe_boot_end()
        ),
        ServerBootNotice::LogOverflow { max_bytes } => format!(
            "{SERVER_BINARY_NAME} wrote more than {max_bytes} bytes to its boot log while starting and was stopped"
        ),
        ServerBootNotice::TimedOut {
            timeout,
            occupant_only: true,
        } => format!(
            "{SERVER_BINARY_NAME} found another server already running, but that server did not answer a status request within {}s",
            timeout.as_secs()
        ),
        ServerBootNotice::TimedOut {
            timeout,
            occupant_only: false,
        } => format!(
            "{SERVER_BINARY_NAME} did not become ready within {}s and was stopped",
            timeout.as_secs()
        ),
    }
}

pub fn append_boot_log_notice(
    message: &mut String,
    boot_log: &Path,
    server_log: &Path,
    tail: Result<String, std::io::Error>,
) {
    match tail {
        Ok(tail) if !tail.is_empty() => message.push_str(&format!(
            "\nserver output ({}):\n{tail}",
            boot_log.display()
        )),
        Ok(_) => message.push_str(&format!(
            "\nthe server printed nothing during boot ({})",
            boot_log.display()
        )),
        Err(error) => message.push_str(&format!(
            "\ncould not read the server boot log {}: {error}",
            boot_log.display()
        )),
    }
    message.push_str(&format!(
        "\nonce it is running, the server logs to {}",
        server_log.display()
    ));
}

pub fn sibling_build_mismatch(server: &Path, status: &crate::status::RuntimeStatus) -> String {
    let server_name = crate::invocation::SERVER_BINARY_NAME;
    format!(
        "{} is a different build than this {PROGRAM_NAME} and was stopped; install {PROGRAM_NAME} and {server_name} together (`brokkr install`).\n\nserver: v{} build {}\nclient: v{} build {}",
        server.display(),
        status.version,
        status.build_id,
        shepr_protocol::build_version(),
        shepr_protocol::BUILD_ID
    )
}

pub fn server_transition_timeout(socket: &Path, timeout: std::time::Duration) -> String {
    format!(
        "the shepr server at {} did not finish starting or release its socket within {}ms",
        socket.display(),
        timeout.as_millis()
    )
}

pub fn disconnect_notice(
    cause: crate::FailureCause,
    disposition: crate::FailureDisposition,
) -> &'static str {
    use crate::FailureCause;
    use shepr_platform::ipc::{StreamFailure, classify_stream_error};
    if disposition.needs_attention() {
        return "connection failed; needs attention";
    }
    match cause {
        FailureCause::Backpressure => "local output queue filled; reconnecting",
        FailureCause::Shutdown(_) => "server shut down; reconnecting",
        FailureCause::Io(kind) => match classify_stream_error(kind) {
            StreamFailure::TimedOut => "connection timed out; reconnecting",
            StreamFailure::PeerGone => "connection was lost; reconnecting",
            StreamFailure::NoListener | StreamFailure::Other => "connection failed; reconnecting",
        },
        _ => "connection failed; reconnecting",
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
