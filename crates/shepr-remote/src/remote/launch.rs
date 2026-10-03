use super::*;

use std::io;

pub(super) const REMOTE_OUTPUT_READY_MARKER: &str = "shepr-remote-output-ready";

/// Stops the remote server instance that reported `server.boot_id`, and no
/// other, by running the discovered remote `shepr server stop --expect-boot` over
/// a BatchMode connection. The remote command waits for the server's
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
) -> io::Result<RemoteStop> {
    let args = RemoteCliCommand::ServerStop {
        expected_boot: &server.boot_id,
    }
    .args();
    let output = ssh.sh_output_within(
        &server.executable.command(&args),
        crate::limits::REMOTE_STOP_SSH_TIMEOUT,
    )?;
    if output.status.success() {
        return Ok(RemoteStop::Stopped);
    }
    match output
        .status
        .code()
        .and_then(shepr_api::server_stop::ServerStopExit::from_code)
    {
        Some(shepr_api::server_stop::ServerStopExit::NoServer) => {
            return Ok(RemoteStop::NoServer);
        }
        Some(shepr_api::server_stop::ServerStopExit::BootMismatch) => {
            return Ok(RemoteStop::BootChanged);
        }
        None => {}
    }
    Err(command_failed("remote server stop failed", &output))
}

/// How a conditional remote stop ended when it did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteStop {
    /// The observed instance stopped answering and no replacement was found.
    Stopped,
    /// No server was present when the stop request ran.
    NoServer,
    /// A different boot answered while stopping the instance that had been observed.
    BootChanged,
}

impl RemoteExecutable {
    pub(super) fn command(&self, args: &[&str]) -> PosixScript {
        let arguments = shepr_core::shell_quote::join_argv(args.iter().copied());
        PosixScript::new(if arguments.is_empty() {
            self.shell_word().to_owned()
        } else {
            format!("{} {arguments}", self.shell_word())
        })
    }

    pub(super) fn status_client_command(&self) -> PosixScript {
        let args = RemoteCliCommand::ClientStatus.args();
        self.command(&args)
    }

    pub(super) fn bridge_command(&self) -> AccountShellCommand {
        let args = RemoteCliCommand::ClientBridge.args();
        // sshd hands this string to the user's account shell, which need not be POSIX
        // (xonsh, fish, nushell). Run the script under /bin/sh (discovery feeds its
        // script to `/bin/sh -s` instead), so the account shell only has to launch one
        // quoted command.
        posix_shell_command(&posix_remote_output_command(&self.command(&args)))
    }
}

/// Prefixes `command` with the output-ready marker line (preceded by a newline, so
/// the marker starts a line of its own after any shell startup output) and maps a remote
/// command's exit 255 to 254. OpenSSH also uses 255 for its own failures, so the
/// wrapper keeps a remote program's 255 from being mistaken for a broken SSH link.
///
/// The prefix is deliberately plain words with no quotes or newlines. For a plain
/// `command` such as the client bridge's `<path> ...`, the wrapped result of
/// [`posix_shell_command`] reaches a non-POSIX account shell as `/bin/sh -c` plus one
/// single-quoted argument with nothing inside it to escape.
///
/// Scripts fed to `/bin/sh -s` end with a newline; it is trimmed so the status
/// suffix does not start a line with `;`, which is a shell syntax error.
pub(super) fn posix_remote_output_command(command: &PosixScript) -> PosixScript {
    let command = command.as_str().trim_end();
    PosixScript::new(format!(
        "echo; echo {REMOTE_OUTPUT_READY_MARKER}; {command}; shepr_exit_status=$?; if [ $shepr_exit_status -eq {SSH_OWN_FAILURE_EXIT_CODE} ]; then exit {REMAPPED_REMOTE_255_EXIT_CODE}; fi; exit $shepr_exit_status"
    ))
}

/// Runs a POSIX script under `/bin/sh` regardless of the remote account shell.
pub(super) fn posix_shell_command(script: &PosixScript) -> AccountShellCommand {
    AccountShellCommand::from_account_shell_text(format!(
        "/bin/sh -c {}",
        shell_quote(script.as_str())
    ))
}

pub fn shell_quote(value: &str) -> String {
    shepr_core::shell_quote::quote(value)
}

#[cfg(test)]
mod shell_command_tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn status_probe_runs_under_posix_sh() {
        use shepr_test_support::fixture::{self, Step};
        use std::process::Stdio;

        // The remote shepr: a fixture stand-in answering exactly the
        // invocation the probe makes, and failing any other.
        let scratch = shepr_test_support::ScratchDir::new("status-probe");
        let answers = |operands: &[&str], steps: Vec<Step>| Step::When {
            operands: operands
                .iter()
                .map(|operand| (*operand).to_owned())
                .collect(),
            steps,
        };
        let executable_path = fixture::stand_in(
            &scratch,
            "shepr",
            &[
                answers(
                    &RemoteCliCommand::ClientStatus.args(),
                    vec![
                        Step::Print(
                            "{\"version\":\"test\",\"build_id\":\"0123456789abcdef\"}\n".into(),
                        ),
                        Step::Exit(0),
                    ],
                ),
                Step::Exit(64),
            ],
        );

        let executable = RemoteExecutable::parse(
            executable_path
                .to_str()
                .expect("scratch path is valid UTF-8"),
        )
        .expect("fake executable path is valid");
        let script = posix_remote_output_command(&executable.status_client_command());
        // host-program-ok: the generated remote script is the subject, run as sshd runs it
        let mut child = shepr_test_support::command_in_scratch("/bin/sh", "status-probe-sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start POSIX shell");
        child
            .stdin
            .take()
            .expect("shell stdin is piped")
            .write_all(script.as_str().as_bytes())
            .expect("write probe script");
        let output = child.wait_with_output().expect("wait for POSIX shell");
        assert!(
            output.status.success(),
            "probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let mut stdout = output.stdout;
        normalize_remote_stdout(&mut stdout, true).expect("output marker is present");
        assert!(parse_client_status_json(&String::from_utf8_lossy(&stdout)).is_some());
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod launch_tests;
