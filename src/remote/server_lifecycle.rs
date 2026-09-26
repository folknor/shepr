use super::*;

use serde::Deserialize;
use std::io::{self, IsTerminal, Write as _};
use std::thread;
use std::time::{Duration, Instant};

pub(super) const REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteServerStatus {
    Running {
        version: Option<String>,
        /// Started as a detached daemon, so an SSH drop disconnects only the
        /// client. A daemon lifecycle requirement, not a build check: the
        /// build is settled by the preamble when the client attaches.
        detached_server_daemon: bool,
    },
    NotRunning,
}

#[derive(Debug, Deserialize)]
pub(super) struct RemoteServerStatusJson {
    pub(super) running: bool,
    pub(super) version: Option<String>,
    pub(super) capabilities: Option<RemoteServerCapabilitiesJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RemoteServerCapabilitiesJson {
    pub(super) detached_server_daemon: bool,
}

pub(super) fn ensure_remote_server_ready(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<()> {
    let RemoteServerStatus::Running {
        version,
        detached_server_daemon: false,
    } = remote_server_status(ssh, remote_shepr)?
    else {
        return Ok(());
    };
    if confirm_remote_server_stop(&ssh.destination(), version.as_deref())? {
        stop_remote_server(ssh, remote_shepr)?;
    }
    Ok(())
}

pub(super) fn remote_server_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<RemoteServerStatus> {
    let command = remote_shepr.session_command(ssh.session_name(), &["status", "server", "--json"]);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server status failed", &output));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_remote_server_status_json(stdout.trim())
}

pub(super) fn parse_remote_server_status_json(status: &str) -> io::Result<RemoteServerStatus> {
    let parsed: RemoteServerStatusJson = serde_json::from_str(status).map_err(|err| {
        io::Error::other(format!(
            "could not parse remote server status JSON from `{status}`: {err}"
        ))
    })?;
    if !parsed.running {
        return Ok(RemoteServerStatus::NotRunning);
    }
    Ok(RemoteServerStatus::Running {
        version: parsed.version,
        detached_server_daemon: parsed
            .capabilities
            .is_some_and(|capabilities| capabilities.detached_server_daemon),
    })
}

/// Offers to restart a remote server that was not started as a detached
/// daemon. Declining, or having no terminal to ask on, keeps it running.
pub(super) fn confirm_remote_server_stop(target: &str, version: Option<&str>) -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        eprintln!(
            "remote shepr server on {target} is still running v{}.",
            version_label(version)
        );
        return Ok(false);
    }

    eprintln!("remote shepr server on {target} is currently running:");
    eprintln!("  server: v{}", version_label(version));
    eprintln!();
    eprintln!(
        "the remote server was not started as a detached daemon and may not survive SSH connection loss. restart it so network drops disconnect only this client."
    );
    eprintln!(
        "This stops active remote pane processes, including shells, agents, dev servers, and tests."
    );
    eprint!("restart the remote server now? [y/N] ");
    io::stderr().flush()?;

    read_remote_confirmation(&mut io::stdin().lock(), false)
}

pub(super) fn stop_remote_server(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<()> {
    let command = remote_shepr.session_command(ssh.session_name(), &["server", "stop"]);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server stop failed", &output));
    }

    wait_for_remote_server_shutdown(ssh, remote_shepr)?;
    eprintln!(
        "stopped the remote shepr server on {}; it will restart when the remote client bridge attaches.",
        ssh.target()
    );
    Ok(())
}

pub(super) fn wait_for_remote_server_shutdown(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<()> {
    let deadline = Instant::now() + REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT;
    loop {
        if remote_server_status(ssh, remote_shepr)? == RemoteServerStatus::NotRunning {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "shutdown was requested, but the old remote shepr server on {target} is still responding after {} seconds",
                    REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT.as_secs(),
                    target = ssh.target()
                ),
            ));
        }
        thread::sleep(REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL);
    }
}

pub(super) fn version_label(version: Option<&str>) -> &str {
    version.unwrap_or("unknown")
}

pub(super) fn read_remote_confirmation(
    reader: &mut impl io::BufRead,
    default: bool,
) -> io::Result<bool> {
    let mut answer = String::new();
    if reader.read_line(&mut answer)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote setup cancelled",
        ));
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        "" => Ok(default),
        _ => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote setup cancelled: expected yes or no",
        )),
    }
}
