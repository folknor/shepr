use super::*;

use serde::Deserialize;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

pub(super) const REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteServerStatus {
    Running {
        version: Option<String>,
        build_id: Option<String>,
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
    #[serde(default)]
    pub(super) build_id: Option<String>,
    pub(super) capabilities: Option<RemoteServerCapabilitiesJson>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RemoteServerCapabilitiesJson {
    pub(super) detached_server_daemon: bool,
}

pub(super) fn ensure_remote_server_ready(
    operator: &mut dyn Operator,
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
    if confirm_remote_server_stop(operator, &ssh.destination(), version.as_deref())? {
        return stop_remote_server(operator, ssh, remote_shepr);
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
        build_id: parsed.build_id,
        detached_server_daemon: parsed
            .capabilities
            .is_some_and(|capabilities| capabilities.detached_server_daemon),
    })
}

fn remote_server_compatibility_error(
    target: &str,
    version: Option<&str>,
    build_id: Option<&str>,
) -> io::Error {
    let printable = |value: Option<&str>| {
        value
            .filter(|value| value.chars().all(|ch| ch.is_ascii_graphic()))
            .unwrap_or("unknown")
            .to_owned()
    };
    let version = printable(version);
    let build_id = printable(build_id);
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "remote Shepr server compatibility error on {target}: found version {version} build {build_id}; this client is version {} build {}. To keep that server and its panes, save the machine with a session of its own instead: `shepr machine add <ssh-target> --label <label> --remote-session <name>`. To replace it with this build instead, stop the remote server and retry",
            shepr_protocol::build_version(),
            shepr_protocol::BUILD_ID
        ),
    )
}

/// Offers to restart a remote server that was not started as a detached
/// daemon. Declining, or having no terminal to ask on, keeps it running.
pub(super) fn confirm_remote_server_stop(
    operator: &mut dyn Operator,
    target: &str,
    version: Option<&str>,
) -> io::Result<bool> {
    let confirmation = Confirmation {
        context: vec![
            format!("remote shepr server on {target} is currently running:"),
            format!("  server: v{}", version_label(version)),
            String::new(),
            "the remote server was not started as a detached daemon and may not survive SSH connection loss. restart it so network drops disconnect only this client."
                .to_owned(),
            "This stops active remote pane processes, including shells, agents, dev servers, and tests."
                .to_owned(),
        ],
        question: "restart the remote server now?".to_owned(),
        default: false,
    };
    match operator.confirm(&confirmation)? {
        Some(answer) => Ok(answer),
        None => {
            operator.notice(&format!(
                "remote shepr server on {target} is still running v{}.",
                version_label(version)
            ));
            Ok(false)
        }
    }
}

/// The operator on the far side of an interactive remote operation. This
/// crate never writes to the terminal or reads stdin itself: the binary owns
/// operator output and implements this.
pub trait Operator {
    /// Shows one line of progress or status text.
    fn notice(&mut self, line: &str);

    /// Asks a yes/no question. `Ok(None)` means there is no terminal to ask
    /// on, which the caller treats as the question going unanswered.
    fn confirm(&mut self, confirmation: &Confirmation) -> io::Result<Option<bool>>;
}

/// A yes/no question for the operator. The question carries its own default,
/// so the rendered `[y/N]` hint and the answer to an empty line come from one
/// value and cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// Lines shown before the question, in order; an empty string is a blank
    /// line.
    pub context: Vec<String>,
    /// The question itself, without the answer hint.
    pub question: String,
    /// The answer an empty line gives.
    pub default: bool,
}

impl Confirmation {
    /// The prompt line, question plus answer hint, with a trailing space and
    /// no newline.
    pub fn prompt(&self) -> String {
        let hint = if self.default { "[Y/n]" } else { "[y/N]" };
        format!("{} {hint} ", self.question)
    }

    /// Reads one answer line. End of input and an unrecognised answer cancel
    /// the operation.
    pub fn read_answer(&self, reader: &mut impl io::BufRead) -> io::Result<bool> {
        read_remote_confirmation(reader, self.default)
    }
}

pub(super) fn stop_remote_server(
    operator: &mut dyn Operator,
    ssh: &RemoteSsh,
    remote_shepr: &RemoteExecutable,
) -> io::Result<()> {
    // Forced: the operator already confirmed this stop, and the server being
    // replaced may be of another build than the remote binary, which an
    // unforced stop refuses.
    let command = remote_shepr.session_command(
        ssh.session_name(),
        &["server", "stop", shepr_api::session::FORCE_STOP_FLAG],
    );
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server stop failed", &output));
    }

    wait_for_remote_server_shutdown(ssh, remote_shepr)?;
    operator.notice(&format!(
        "stopped the remote shepr server on {}; it will restart when the remote client bridge attaches.",
        ssh.target()
    ));
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

    fn running(version: Option<String>, build_id: Option<&str>) -> RemoteServerStatus {
        RemoteServerStatus::Running {
            version,
            build_id: build_id.map(str::to_owned),
            detached_server_daemon: true,
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

    /// The rendered hint and the answer to an empty line come from one value.
    #[test]
    fn a_confirmation_prompt_and_its_empty_answer_share_one_default() {
        for (default, hint) in [(false, "[y/N]"), (true, "[Y/n]")] {
            let confirmation = Confirmation {
                context: Vec::new(),
                question: "restart?".into(),
                default,
            };
            assert_eq!(confirmation.prompt(), format!("restart? {hint} "));
            assert_eq!(
                confirmation
                    .read_answer(&mut "\n".as_bytes())
                    .expect("an empty line takes the default"),
                default
            );
        }
    }

    /// With no terminal to ask on, the server keeps running and the operator
    /// is told so; nothing is printed by this crate.
    #[test]
    fn an_unanswerable_restart_question_keeps_the_server() {
        struct Absent(Vec<String>);
        impl Operator for Absent {
            fn notice(&mut self, line: &str) {
                self.0.push(line.to_owned());
            }
            fn confirm(&mut self, _confirmation: &Confirmation) -> io::Result<Option<bool>> {
                Ok(None)
            }
        }
        let mut operator = Absent(Vec::new());
        assert!(
            !confirm_remote_server_stop(&mut operator, "host", Some("1.0"))
                .expect("no terminal is not an error")
        );
        assert_eq!(
            operator.0,
            ["remote shepr server on host is still running v1.0."]
        );
    }

    #[test]
    fn server_build_mismatch_offers_a_separate_remote_session_before_a_stop() {
        let other_build = if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
            "0000000000000000"
        } else {
            "ffffffffffffffff"
        };
        let stale = running(Some(shepr_protocol::build_version()), Some(other_build));
        let error = ensure_remote_server_build("host", &stale).expect_err("stale daemon");
        let message = error.to_string();
        assert!(message.contains("--remote-session <name>"), "{message}");
        let session_offer = message
            .find("--remote-session <name>")
            .expect("checked above");
        let stop_mention = message.find("stop").expect("mentions stopping the server");
        assert!(
            session_offer < stop_mention,
            "the separate-session offer should come before the stop instruction: {message}"
        );
    }
}
