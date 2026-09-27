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
        protocol: Option<u32>,
        /// Started as a detached daemon, so an SSH drop disconnects only the
        /// client. A daemon lifecycle requirement; `ensure_remote_server_build`
        /// checks the build separately.
        detached_server_daemon: bool,
    },
    NotRunning,
}

#[derive(Debug, Deserialize)]
pub(super) struct RemoteServerStatusJson {
    pub(super) running: bool,
    pub(super) version: Option<String>,
    pub(super) protocol: Option<u32>,
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
    let status = remote_server_status(ssh, remote_shepr)?;
    let RemoteServerStatus::Running {
        version,
        detached_server_daemon: false,
        ..
    } = &status
    else {
        return ensure_remote_server_build(ssh.target(), &status);
    };
    if confirm_remote_server_stop(&ssh.destination(), version.as_deref())? {
        return stop_remote_server(ssh, remote_shepr);
    }
    ensure_remote_server_build(ssh.target(), &status)
}

/// Rejects a running remote server from another build once, before a bridge starts
/// and fails its preamble on every retry. A stopped server passes: the bridge starts
/// one from the discovered executable, whose build discovery already matched.
pub(super) fn ensure_remote_server_build(
    target: &str,
    status: &RemoteServerStatus,
) -> io::Result<()> {
    let RemoteServerStatus::Running {
        version, protocol, ..
    } = status
    else {
        return Ok(());
    };
    let expected_version = shepr_protocol::build_version();
    if version.as_deref() == Some(expected_version.as_str())
        && *protocol == Some(shepr_protocol::PROTOCOL_VERSION)
    {
        return Ok(());
    }
    Err(remote_server_compatibility_error(
        target,
        version.as_deref(),
        *protocol,
    ))
}

/// Queries the remote server's state without judging its build, so shutdown polling
/// can watch a server from another build go away.
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
        protocol: parsed.protocol,
        detached_server_daemon: parsed
            .capabilities
            .is_some_and(|capabilities| capabilities.detached_server_daemon),
    })
}

fn remote_server_compatibility_error(
    target: &str,
    version: Option<&str>,
    protocol: Option<u32>,
) -> io::Error {
    let version = version
        .filter(|version| version.chars().all(|ch| ch.is_ascii_graphic()))
        .unwrap_or("unknown");
    let protocol = protocol
        .map(|protocol| protocol.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "remote Shepr server compatibility error on {target}: found version {version} and protocol {protocol}; this client requires version {} and protocol {}. Restart the remote server with this build and retry",
            shepr_protocol::build_version(),
            shepr_protocol::PROTOCOL_VERSION
        ),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn running(version: Option<String>, protocol: Option<u32>) -> RemoteServerStatus {
        RemoteServerStatus::Running {
            version,
            protocol,
            detached_server_daemon: true,
        }
    }

    #[test]
    fn only_a_running_server_from_this_build_passes_the_build_check() {
        let this_build = running(
            Some(shepr_protocol::build_version()),
            Some(shepr_protocol::PROTOCOL_VERSION),
        );
        assert!(ensure_remote_server_build("host", &this_build).is_ok());
        assert!(ensure_remote_server_build("host", &RemoteServerStatus::NotRunning).is_ok());

        for stale in [
            running(
                Some("0.0.0-old".into()),
                Some(shepr_protocol::PROTOCOL_VERSION),
            ),
            running(
                Some(shepr_protocol::build_version()),
                Some(shepr_protocol::PROTOCOL_VERSION.wrapping_add(1)),
            ),
            running(None, None),
        ] {
            let error = ensure_remote_server_build("host", &stale).expect_err("stale daemon");
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("compatibility error on host"));
        }
    }
}
