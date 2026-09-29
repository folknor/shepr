use super::*;

use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteServerStatus {
    Running {
        version: Option<String>,
        build_id: Option<String>,
        boot_id: Option<String>,
    },
    NotRunning,
}

/// What the startup check learned about a machine that can be served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SavedSshCheck {
    /// A matching shepr pair is installed, and any server running there is this
    /// build. A stopped server counts: the bridge starts one on attach.
    Ready,
    /// A server of another build is running there, and the installed pair is
    /// this build, so a restart would bring up the right one.
    DifferentBuild(DifferentBuildServer),
}

/// A running remote server of another build, as observed: enough to offer its
/// restart and to stop exactly that instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DifferentBuildServer {
    /// The remote `shepr` discovery verified as this build (with its sibling
    /// server), through which the stop runs.
    pub executable: RemoteExecutable,
    /// The running server's version, printable or `unknown`.
    pub version: String,
    /// The running server's build id, printable.
    pub build_id: String,
    /// The running server's boot identity, which the conditional stop names.
    pub boot_id: String,
}

/// Judges a remote server's state against this build, before a bridge starts and
/// fails its preamble on every retry. A stopped server passes: the bridge starts
/// one from the discovered executable, whose build discovery already matched.
///
/// A running server of another build that reported both a printable build and a
/// boot identity is a [`SavedSshCheck::DifferentBuild`], which can be restarted.
/// One that did not (an unknown build, or one that predates the boot identity)
/// cannot be stopped as a specific instance, so it is an error the operator has
/// to act on.
pub(super) fn judge_remote_server(
    target: &str,
    executable: &RemoteExecutable,
    status: &RemoteServerStatus,
) -> io::Result<SavedSshCheck> {
    let RemoteServerStatus::Running {
        version,
        build_id,
        boot_id,
    } = status
    else {
        return Ok(SavedSshCheck::Ready);
    };
    if build_id
        .as_deref()
        .is_some_and(shepr_protocol::is_this_build)
    {
        return Ok(SavedSshCheck::Ready);
    }
    let build = printable_remote_token(build_id.as_deref());
    let boot = printable_remote_token(boot_id.as_deref());
    if let (Some(build_id), Some(boot_id)) = (build, boot) {
        return Ok(SavedSshCheck::DifferentBuild(DifferentBuildServer {
            executable: executable.clone(),
            version: printable_remote_value(version.as_deref()),
            build_id,
            boot_id,
        }));
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
        boot_id: parsed.boot_id,
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

/// A remote-reported identifier (a build id, a boot id): a non-empty run of
/// printable ASCII with no spaces, or `None`. It is safe to show and to hand back
/// to the remote as one argument.
pub(super) fn printable_remote_token(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_graphic()))
        .map(str::to_owned)
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

    fn running(
        version: Option<String>,
        build_id: Option<&str>,
        boot_id: Option<&str>,
    ) -> RemoteServerStatus {
        RemoteServerStatus::Running {
            version,
            build_id: build_id.map(str::to_owned),
            boot_id: boot_id.map(str::to_owned),
        }
    }

    fn executable() -> RemoteExecutable {
        RemoteExecutable::parse("/home/u/.cargo/bin/shepr").expect("test precondition")
    }

    fn judge(status: &RemoteServerStatus) -> io::Result<SavedSshCheck> {
        judge_remote_server("host", &executable(), status)
    }

    fn other_build() -> &'static str {
        if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
            "0000000000000000"
        } else {
            "ffffffffffffffff"
        }
    }

    #[test]
    fn a_stopped_or_same_build_server_is_ready() {
        // The build id alone decides; the version string is only reported.
        let this_build = running(
            Some("0.0.0-old".into()),
            Some(shepr_protocol::BUILD_ID),
            Some("17-23"),
        );
        assert_eq!(
            judge(&this_build).expect("same build"),
            SavedSshCheck::Ready
        );
        assert_eq!(
            judge(&RemoteServerStatus::NotRunning).expect("stopped"),
            SavedSshCheck::Ready
        );
    }

    #[test]
    fn a_server_of_another_build_with_a_boot_identity_can_be_restarted() {
        let stale = running(Some("0.0.0-old".into()), Some(other_build()), Some("17-23"));
        assert_eq!(
            judge(&stale).expect("restartable"),
            SavedSshCheck::DifferentBuild(DifferentBuildServer {
                executable: executable(),
                version: "0.0.0-old".into(),
                build_id: other_build().into(),
                boot_id: "17-23".into(),
            })
        );
    }

    #[test]
    fn a_server_that_cannot_be_named_as_an_instance_is_an_error() {
        for stale in [
            running(Some("v".into()), Some(other_build()), None),
            running(Some("v".into()), Some(other_build()), Some("has space")),
            running(Some("v".into()), None, Some("17-23")),
            running(None, None, None),
        ] {
            let error = judge(&stale).expect_err("not restartable");
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("compatibility error on host"));
        }
    }

    #[test]
    fn remote_version_text_is_filtered_before_local_output() {
        let injected = "\x1b[2J";
        let error = judge(&running(
            Some(injected.into()),
            Some(injected),
            Some("17-23"),
        ))
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
        let stale = running(
            Some(shepr_protocol::build_version()),
            Some(other_build()),
            None,
        );
        let error = judge(&stale).expect_err("stale daemon");
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
