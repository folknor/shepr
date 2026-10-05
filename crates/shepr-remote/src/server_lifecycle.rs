use std::io;

use shepr_launch::restart::StopOutcome;
use shepr_launch::stop::ServerStopExit;
use shepr_launch::{EndpointFailure, RemoteText};

use crate::args::RemoteCliCommand;
use crate::limits::REMOTE_STOP_SSH_TIMEOUT;
use crate::machine::RemoteExecutable;
use crate::ssh::{RemoteSsh, command_failed};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteServerStatus {
    Running {
        build_id: shepr_protocol::BuildIdentity,
        boot_id: shepr_protocol::BootId,
    },
    NotRunning,
}

/// Queries the remote server's state without judging its build. The JSON
/// schema requires both identities whenever a server is starting or running,
/// so a partial identity cannot be represented.
pub(crate) fn remote_server_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<RemoteServerStatus> {
    let args = RemoteCliCommand::ServerStatus.args();
    let script = remote_shepr.command(&args);
    let output = ssh.sh_output(&script)?;
    if !output.status.success() {
        return Err(command_failed("remote server status failed", &output));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_remote_server_status_json(stdout.trim())
}

/// Reads the remote `status server --json`. A starting server is judged like a
/// running one: it already names its build and boot, and one of another build
/// would refuse the bridge once it opens. A stopping server counts as none,
/// since a starting bridge waits it out and starts a successor from the
/// verified install. A server that listens but does not answer fails the
/// check: a bridge could neither use nor replace it.
pub(crate) fn parse_remote_server_status_json(status: &str) -> io::Result<RemoteServerStatus> {
    use shepr_api::schema::ServerStatus;
    let parsed: shepr_api::schema::ServerStatusJson =
        serde_json::from_str(status).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                EndpointFailure::incompatible(format!(
                    "could not parse remote server status JSON: {err}"
                )),
            )
        })?;
    match parsed.state {
        ServerStatus::Gone | ServerStatus::Stopping(_) => Ok(RemoteServerStatus::NotRunning),
        ServerStatus::Starting(identity) | ServerStatus::Running(identity) => {
            Ok(RemoteServerStatus::Running {
                build_id: identity.build_id,
                boot_id: identity.boot_id,
            })
        }
        // Not a link failure kind: SSH answered, the remote server did not.
        ServerStatus::Unresponsive => {
            Err(io::Error::other(EndpointFailure::remote_repair(format!(
                "the remote shepr server at {} is not answering status requests",
                remote_display_value(Some(&parsed.socket))
            ))))
        }
    }
}

/// A remote-reported display value (a version, build id or socket path),
/// restricted to printable ASCII and spaces, else `unknown`.
pub(crate) fn remote_display_value(value: Option<&str>) -> RemoteText {
    let value = value
        .filter(|value| {
            !value.is_empty() && value.chars().all(|ch| ch.is_ascii_graphic() || ch == ' ')
        })
        .unwrap_or("unknown");
    RemoteText::from_untrusted(value)
}

/// Before an operator's Restart starts this build's server on a machine:
/// stops the running server of another build there, naming the boot its
/// status reported. No server, and a server of this build, need no stop.
pub(crate) fn stop_server_of_another_build(
    ssh: &RemoteSsh,
    executable: &RemoteExecutable,
) -> io::Result<()> {
    stop_for_restart(remote_server_status(ssh, executable)?, |boot_id| {
        stop_remote_server_with_ssh(ssh, executable, boot_id)
    })
}

/// What a Restart does with the status it read: `stop` runs only for a
/// server of another build, with that server's boot identity, so the stop
/// cannot reach a server that replaced it. A stop that met another boot fails
/// the Restart and leaves that server running; the client's next attempt
/// reads whatever runs there now.
fn stop_for_restart(
    status: RemoteServerStatus,
    stop: impl FnOnce(&shepr_protocol::BootId) -> io::Result<StopOutcome>,
) -> io::Result<()> {
    let RemoteServerStatus::Running { build_id, boot_id } = status else {
        return Ok(());
    };
    if build_id.is_this_build() {
        return Ok(());
    }
    match stop(&boot_id)? {
        StopOutcome::Stopped | StopOutcome::NoServer => Ok(()),
        StopOutcome::BootChanged => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            EndpointFailure::retry(
                "the server was replaced while it was being stopped; the new one was left running",
            ),
        )),
    }
}

/// Stops the remote server instance that reported `boot_id`, and no other, by
/// running the discovered remote `shepr stop --expect-boot` over a BatchMode
/// connection. The remote command waits for the server's named boot to stop
/// answering. It exits with `ServerStopExit::BootMismatch` when another boot
/// answers the stop request or appears while the named boot shuts down, or
/// `ServerStopExit::NoServer` when the observed server was already gone by the
/// time its stop request ran.
///
/// `ssh` is the machine's transport; the stop's own timeout replaces any
/// attempt deadline it carries.
pub(crate) fn stop_remote_server_with_ssh(
    ssh: &RemoteSsh,
    executable: &RemoteExecutable,
    boot_id: &shepr_protocol::BootId,
) -> io::Result<StopOutcome> {
    let args = RemoteCliCommand::ServerStop {
        expected_boot: boot_id,
    }
    .args();
    let output = ssh.sh_output_within(&executable.command(&args), REMOTE_STOP_SSH_TIMEOUT)?;
    if output.status.success() {
        return Ok(StopOutcome::Stopped);
    }
    match output.status.code().and_then(ServerStopExit::from_code) {
        Some(ServerStopExit::NoServer) => {
            return Ok(StopOutcome::NoServer);
        }
        Some(ServerStopExit::BootMismatch) => {
            return Ok(StopOutcome::BootChanged);
        }
        None => {}
    }
    Err(command_failed("remote server stop failed", &output))
}

#[cfg(test)]
mod tests;
