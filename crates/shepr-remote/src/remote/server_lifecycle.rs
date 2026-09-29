use super::*;

use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteServerStatus {
    Running {
        version: Option<String>,
        build_id: Option<String>,
    },
    NotRunning,
}

/// Rejects a running remote server from another build once, before a bridge starts
/// and fails its preamble on every retry. A stopped server passes: the bridge starts
/// one from the discovered executable, whose build discovery already matched.
pub(super) fn ensure_remote_server_build(
    target: &str,
    status: &RemoteServerStatus,
) -> io::Result<()> {
    let RemoteServerStatus::Running {
        version, build_id, ..
    } = status
    else {
        return Ok(());
    };
    if build_id
        .as_deref()
        .is_some_and(shepr_protocol::is_this_build)
    {
        return Ok(());
    }
    Err(remote_server_compatibility_error(
        target,
        version.as_deref(),
        build_id.as_deref(),
    ))
}

/// Queries the remote server's state without judging its build.
pub(super) fn remote_server_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<RemoteServerStatus> {
    let args = RemoteCliCommand::ServerStatus.args();
    let command = remote_shepr.command(&args);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server status failed", &output));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_remote_server_status_json(stdout.trim())
}

pub(super) fn parse_remote_server_status_json(status: &str) -> io::Result<RemoteServerStatus> {
    let parsed: shepr_api::schema::ServerStatusJson =
        serde_json::from_str(status).map_err(|err| {
            io::Error::other(format!("could not parse remote server status JSON: {err}"))
        })?;
    if !parsed.running {
        return Ok(RemoteServerStatus::NotRunning);
    }
    Ok(RemoteServerStatus::Running {
        version: parsed.version,
        build_id: parsed.build_id,
    })
}

fn remote_server_compatibility_error(
    target: &str,
    version: Option<&str>,
    build_id: Option<&str>,
) -> io::Error {
    let version = printable_remote_value(version);
    let build_id = printable_remote_value(build_id);
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "remote Shepr server compatibility error on {target}: found version {version} build {build_id}; this client is version {} build {}. To use this build, stop the remote server and retry",
            shepr_protocol::build_version(),
            shepr_protocol::BUILD_ID
        ),
    )
}

/// A single remote-reported token (a version, a build id) for a local message:
/// printable ASCII and spaces only, else `unknown`.
pub(super) fn printable_remote_value(value: Option<&str>) -> String {
    value
        .filter(|value| {
            !value.is_empty() && value.chars().all(|ch| ch.is_ascii_graphic() || ch == ' ')
        })
        .unwrap_or("unknown")
        .to_owned()
}

/// Remote free text (SSH diagnostics, remote stderr) made safe for a local
/// terminal. Line breaks and tabs stay. Carriage returns are dropped: OpenSSH
/// ends its stderr lines with CRLF, and a bare one could overwrite the line
/// before it. Every other control character, which could start a terminal
/// escape sequence, becomes `?`. The mapping is per character, so text can be
/// filtered in arbitrary chunks as it streams.
pub(super) fn printable_remote_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '\r')
        .map(|ch| {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                '?'
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(version: Option<String>, build_id: Option<&str>) -> RemoteServerStatus {
        RemoteServerStatus::Running {
            version,
            build_id: build_id.map(str::to_owned),
        }
    }

    #[test]
    fn only_a_running_server_from_this_build_passes_the_build_check() {
        // The build id alone decides; the version string is only reported.
        let this_build = running(Some("0.0.0-old".into()), Some(shepr_protocol::BUILD_ID));
        assert!(ensure_remote_server_build("host", &this_build).is_ok());
        assert!(ensure_remote_server_build("host", &RemoteServerStatus::NotRunning).is_ok());

        let other_build = if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
            "0000000000000000"
        } else {
            "ffffffffffffffff"
        };
        for stale in [
            running(Some(shepr_protocol::build_version()), Some(other_build)),
            running(Some(shepr_protocol::build_version()), None),
            running(None, None),
        ] {
            let error = ensure_remote_server_build("host", &stale).expect_err("stale daemon");
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("compatibility error on host"));
        }
    }

    #[test]
    fn remote_version_text_is_filtered_before_local_output() {
        let injected = "\x1b[2J";
        let error =
            ensure_remote_server_build("host", &running(Some(injected.into()), Some(injected)))
                .expect_err("different build is rejected");
        assert!(
            error
                .to_string()
                .contains("found version unknown build unknown")
        );
        assert!(!error.to_string().contains('\x1b'));

        let parse_error = parse_remote_server_status_json(injected).expect_err("invalid JSON");
        assert!(!parse_error.to_string().contains('\x1b'));
    }

    #[test]
    fn shared_remote_text_filter_keeps_printable_lines_and_rejects_controls() {
        assert_eq!(
            printable_remote_text("Connection refused\n\x1b[2J"),
            "Connection refused\n?[2J"
        );
        assert_eq!(
            printable_remote_text("Warning: added host\r\nbanner \u{9b}2J caf\u{e9}\ttab"),
            "Warning: added host\nbanner ?2J caf\u{e9}\ttab"
        );
        assert_eq!(printable_remote_value(Some("build id")), "build id");
        assert_eq!(printable_remote_value(Some("bad\tvalue")), "unknown");
    }

    #[test]
    fn server_build_mismatch_says_to_stop_the_remote_server() {
        let other_build = if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
            "0000000000000000"
        } else {
            "ffffffffffffffff"
        };
        let stale = running(Some(shepr_protocol::build_version()), Some(other_build));
        let error = ensure_remote_server_build("host", &stale).expect_err("stale daemon");
        let message = error.to_string();
        assert!(
            message.contains("stop the remote server and retry"),
            "{message}"
        );
        assert!(!message.contains("session"), "{message}");
    }
}

#[cfg(test)]
#[path = "server_lifecycle_tests.rs"]
mod server_lifecycle_tests;
