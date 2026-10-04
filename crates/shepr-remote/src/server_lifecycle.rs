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

/// What the startup check learned about a machine that can be served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MachineSshCheck {
    /// A matching shepr pair is installed, and any server running or starting
    /// there is this build. A stopped or stopping server counts: the bridge
    /// starts one on attach.
    Ready,
    /// A server of another build is running or starting there, and the
    /// installed pair is this build, so a restart would bring up the right one.
    DifferentBuild(DifferentBuildServer),
}

/// A running remote server of another build, as observed: enough to offer its
/// restart and to stop exactly that instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DifferentBuildServer {
    /// The remote `shepr` discovery verified as this build (with its sibling
    /// server), through which the stop runs.
    pub executable: RemoteExecutable,
    /// The running server's build id, printable.
    pub build_id: shepr_protocol::BuildIdentity,
    /// The running server's boot identity, which the conditional stop names.
    pub boot_id: shepr_protocol::BootId,
}

/// Judges a remote server's state against this build, before a bridge starts and
/// fails its preamble on every retry. A stopped server passes: the bridge starts
/// one from the discovered executable, whose build discovery already matched.
///
/// A running server of another build is a
/// [`MachineSshCheck::DifferentBuild`], which can be restarted by its boot
/// identity. The JSON schema requires both identities whenever a server is
/// starting or running, so this intermediate state cannot represent a partial
/// identity.
pub(crate) fn judge_remote_server(
    executable: &RemoteExecutable,
    status: &RemoteServerStatus,
) -> MachineSshCheck {
    let RemoteServerStatus::Running { build_id, boot_id } = status else {
        return MachineSshCheck::Ready;
    };
    if build_id.is_this_build() {
        return MachineSshCheck::Ready;
    }
    MachineSshCheck::DifferentBuild(DifferentBuildServer {
        executable: executable.clone(),
        build_id: *build_id,
        boot_id: boot_id.clone(),
    })
}

/// Queries the remote server's state without judging its build.
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
/// since the bridge waits it out and starts a successor from the verified
/// install. A server that listens but does not answer fails the check: the
/// bridge could neither use nor replace it.
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
        ServerStatus::Unresponsive => Err(io::Error::other(format!(
            "the remote shepr server at {} is not answering status requests",
            remote_display_value(Some(&parsed.socket))
        ))),
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

/// Stops the remote server instance that reported `server.boot_id`, and no
/// other, by running the discovered remote `shepr stop --expect-boot` over a
/// BatchMode connection. The remote command waits for the server's
/// named boot to stop answering. It exits with
/// `ServerStopExit::BootMismatch` when another boot answers the stop request or
/// appears while the named boot shuts down, or `ServerStopExit::NoServer` when
/// the observed server was already gone by the time its stop request ran.
///
/// `ssh` is the machine's preflight transport; the stop's own timeout replaces
/// any attempt deadline it carries.
pub(crate) fn stop_remote_server_with_ssh(
    ssh: &RemoteSsh,
    server: &DifferentBuildServer,
) -> io::Result<StopOutcome> {
    let args = RemoteCliCommand::ServerStop {
        expected_boot: &server.boot_id,
    }
    .args();
    let output =
        ssh.sh_output_within(&server.executable.command(&args), REMOTE_STOP_SSH_TIMEOUT)?;
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
