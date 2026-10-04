use std::io::Write as _;

use super::*;
use crate::args::RemoteCliCommand;
use crate::discovery::{known_remote_binary_candidate_script, parse_client_status_json};
use crate::machine::RemoteExecutable;
use crate::ssh::normalize_remote_stdout;

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

#[test]
fn remote_server_commands_name_no_session() {
    let shepr = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    for (command, line) in [
        (RemoteCliCommand::ServerStatus, "status server --json"),
        (
            RemoteCliCommand::ServerStop {
                expected_boot: "4242-1700000000",
            },
            "server stop --expect-boot 4242-1700000000",
        ),
        (RemoteCliCommand::ClientBridge, "remote-client-bridge"),
    ] {
        assert_eq!(
            shepr.command(&command.args()).as_str(),
            format!("{} {line}", shepr.as_str())
        );
    }
}

#[test]
fn remote_executable_rejects_paths_that_need_shell_quoting() {
    for path in [
        "/home/user's files/shepr",
        "/home/$literal/shepr",
        "/opt/shepr bin/shepr",
    ] {
        assert!(RemoteExecutable::parse(path).is_err(), "{path}");
    }
    let path = "/home/user/.local/bin/shepr-0.1+dev";
    let resolved = RemoteExecutable::parse(path).expect("test precondition");
    assert_eq!(resolved.as_str(), path);
    assert_eq!(resolved.shell_word(), shell_quote(path));
}

#[test]
fn remote_bridge_command_uses_installed_binary() {
    let remote_shepr = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    assert_eq!(
        remote_shepr.bridge_command().as_str(),
        format!(
            "/bin/sh -c 'echo; echo shepr-remote-output-ready; /usr/bin/shepr remote-client-bridge; shepr_exit_status=$?; if [ $shepr_exit_status -eq {SSH_OWN_FAILURE_EXIT_CODE} ]; then exit {REMAPPED_REMOTE_255_EXIT_CODE}; fi; exit $shepr_exit_status'"
        )
    );
}

/// The bridge command is interpreted by /bin/sh, not by the account shell: the
/// account shell only sees `/bin/sh -c` and one quoted word without newlines.
#[test]
fn bridge_command_is_one_quoted_word_for_bin_sh_that_frames_its_output() {
    let remote = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    let command = remote.bridge_command();
    let script = command
        .as_str()
        .strip_prefix("/bin/sh -c '")
        .and_then(|rest| rest.strip_suffix('\''))
        .expect("wrapped in /bin/sh -c");
    assert!(!script.contains('\''), "{script}");
    assert!(!script.contains('\n'), "{script}");

    // The script, run by a real /bin/sh, still frames its output with the marker.
    // host-program-ok: the generated remote script is the subject, run as sshd runs it
    let output = shepr_test_support::command_in_scratch("/bin/sh", "machine-bridge-command-sh")
        .arg("-c")
        .arg(posix_remote_output_command(&PosixScript::new("printf payload")).as_str())
        .output()
        .expect("test precondition");
    let mut stdout = output.stdout;
    normalize_remote_stdout(&mut stdout, output.status.success()).expect("marker line present");
    assert_eq!(stdout, b"payload");
}

/// `sh_output` scripts end with a newline and are fed to `/bin/sh -s`; the
/// wrapper must stay valid shell for them and keep 255 for ssh's own failures.
#[test]
fn remote_output_wrapper_accepts_newline_scripts_and_remaps_exit_255() {
    let run = |script: &str| {
        // host-program-ok: the generated remote script is the subject, run as sshd runs it
        let mut child =
            shepr_test_support::command_in_scratch("/bin/sh", "remote-output-wrapper-sh")
                .arg("-s")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("test precondition");
        child
            .stdin
            .take()
            .expect("test precondition")
            .write_all(
                posix_remote_output_command(&PosixScript::new(script))
                    .as_str()
                    .as_bytes(),
            )
            .expect("test precondition");
        child.wait_with_output().expect("test precondition")
    };

    let output = run("printf payload\n");
    assert!(output.status.success(), "{output:?}");
    let mut stdout = output.stdout;
    normalize_remote_stdout(&mut stdout, true).expect("marker line present");
    assert_eq!(stdout, b"payload");

    assert_eq!(
        run(&known_remote_binary_candidate_script()).status.code(),
        Some(0)
    );
    assert_eq!(run("exit 3\n").status.code(), Some(3));
    let remote_ssh_status = format!("(exit {SSH_OWN_FAILURE_EXIT_CODE})\n");
    assert_eq!(
        run(&remote_ssh_status).status.code(),
        Some(REMAPPED_REMOTE_255_EXIT_CODE)
    );
}
