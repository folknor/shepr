use super::*;

use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteServerStatus {
    Running {
        version: Option<String>,
        build_id: Option<shepr_protocol::BuildIdentity>,
        boot_id: Option<shepr_protocol::BootId>,
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
/// A running server of another build that reported both a printable build and a
/// boot identity that parses as a [`shepr_protocol::BootId`] is a
/// [`MachineSshCheck::DifferentBuild`], which can be restarted. One that did not
/// (an unknown build, no boot identity, or one in a form the remote stop command
/// refuses) cannot be stopped as a specific instance, so it is an error the
/// operator has to act on.
pub(super) fn judge_remote_server(
    target: &SshTarget,
    executable: &RemoteExecutable,
    status: &RemoteServerStatus,
) -> io::Result<MachineSshCheck> {
    let RemoteServerStatus::Running {
        version,
        build_id,
        boot_id,
    } = status
    else {
        return Ok(MachineSshCheck::Ready);
    };
    if build_id.is_some_and(shepr_protocol::BuildIdentity::is_this_build) {
        return Ok(MachineSshCheck::Ready);
    }
    let build = *build_id;
    let boot = boot_id.clone();
    if let (Some(build_id), Some(boot_id)) = (build, boot) {
        return Ok(MachineSshCheck::DifferentBuild(DifferentBuildServer {
            executable: executable.clone(),
            build_id,
            boot_id,
        }));
    }
    Err(remote_server_compatibility_error(
        target,
        version.as_deref(),
        build_id.map(|identity| identity.to_string()).as_deref(),
    ))
}

/// Queries the remote server's state without judging its build.
pub(super) fn remote_server_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<RemoteServerStatus> {
    let args = RemoteCliCommand::ServerStatus.args();
    let script = remote_shepr.command_as_posix_script(&args);
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
pub(super) fn parse_remote_server_status_json(status: &str) -> io::Result<RemoteServerStatus> {
    use shepr_api::schema::ServerStatus;
    let parsed: shepr_api::schema::ServerStatusJson =
        serde_json::from_str(status).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                crate::EndpointFailure::incompatible(format!(
                    "could not parse remote server status JSON: {err}"
                )),
            )
        })?;
    match parsed.state {
        ServerStatus::Gone | ServerStatus::Stopping(_) => Ok(RemoteServerStatus::NotRunning),
        ServerStatus::Starting(identity) | ServerStatus::Running(identity) => {
            Ok(RemoteServerStatus::Running {
                version: Some(identity.version),
                build_id: Some(identity.build_id),
                boot_id: Some(identity.boot_id),
            })
        }
        // Not a link failure kind: SSH answered, the remote server did not.
        ServerStatus::Unresponsive => Err(io::Error::other(format!(
            "the remote shepr server at {} is not answering status requests",
            remote_display_value(Some(&parsed.socket))
        ))),
    }
}

pub(super) fn remote_server_compatibility_error(
    target: &SshTarget,
    version: Option<&str>,
    build_id: Option<&str>,
) -> io::Error {
    let version = remote_display_value(version);
    let build_id = remote_display_value(build_id);
    crate::remote_compatibility_error(format!(
        "remote Shepr server compatibility error on {target}: found version {version} build {build_id}; this client is version {} build {}. To use this build, stop the remote server and retry",
        shepr_protocol::build_version(),
        shepr_protocol::BUILD_ID
    ))
}

/// A remote-reported display value (a version, build id or socket path),
/// restricted to printable ASCII and spaces, else `unknown`.
pub(super) fn remote_display_value(value: Option<&str>) -> crate::RemoteText {
    let value = value
        .filter(|value| {
            !value.is_empty() && value.chars().all(|ch| ch.is_ascii_graphic() || ch == ' ')
        })
        .unwrap_or("unknown");
    crate::RemoteText::from_untrusted(value)
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
            build_id: build_id.and_then(|id| id.parse().ok()),
            boot_id: boot_id.and_then(|id| id.parse().ok()),
        }
    }

    fn executable() -> RemoteExecutable {
        RemoteExecutable::parse("/home/u/.cargo/bin/shepr").expect("test precondition")
    }

    fn target() -> SshTarget {
        SshTarget::parse("host").expect("test precondition")
    }

    fn judge(status: &RemoteServerStatus) -> io::Result<MachineSshCheck> {
        judge_remote_server(&target(), &executable(), status)
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
            MachineSshCheck::Ready
        );
        assert_eq!(
            judge(&RemoteServerStatus::NotRunning).expect("stopped"),
            MachineSshCheck::Ready
        );
    }

    #[test]
    fn a_server_of_another_build_with_a_boot_identity_can_be_restarted() {
        let stale = running(Some("0.0.0-old".into()), Some(other_build()), Some("17-23"));
        assert_eq!(
            judge(&stale).expect("restartable"),
            MachineSshCheck::DifferentBuild(DifferentBuildServer {
                executable: executable(),
                build_id: other_build().parse().expect("build identity"),
                boot_id: "17-23".parse().expect("boot identity"),
            })
        );
    }

    #[test]
    fn a_server_that_cannot_be_named_as_an_instance_is_an_error() {
        for stale in [
            running(Some("v".into()), Some(other_build()), None),
            running(Some("v".into()), Some(other_build()), Some("has space")),
            // A printable token the remote stop command's boot id parser refuses.
            running(Some("v".into()), Some(other_build()), Some("not-a-boot")),
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
            crate::RemoteText::from_untrusted("Connection refused\n\x1b[2J").to_string(),
            "Connection refused\n?[2J"
        );
        assert_eq!(
            crate::RemoteText::from_untrusted(
                "Warning: added host\r\nbanner \u{9b}2J caf\u{e9}\ttab"
            )
            .to_string(),
            "Warning: added host\nbanner ?2J caf\u{e9}\ttab"
        );
        assert_eq!(
            remote_display_value(Some("build id")).to_string(),
            "build id"
        );
        assert_eq!(
            remote_display_value(Some("bad\tvalue")).to_string(),
            "unknown"
        );
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
