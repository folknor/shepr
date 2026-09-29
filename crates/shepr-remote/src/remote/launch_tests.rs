use super::*;

#[test]
fn saved_machine_server_commands_are_scoped_to_the_explicit_session() {
    let shepr = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    for (named_command, default_command, line) in [
        (
            RemoteCliCommand::ServerStatus { session: "agents" },
            RemoteCliCommand::ServerStatus {
                session: shepr_config::DEFAULT_SESSION_NAME,
            },
            "status server --json",
        ),
        (
            RemoteCliCommand::ServerStop {
                session: "agents",
                force: true,
            },
            RemoteCliCommand::ServerStop {
                session: shepr_config::DEFAULT_SESSION_NAME,
                force: true,
            },
            "server stop --force",
        ),
        (
            RemoteCliCommand::ClientBridge { session: "agents" },
            RemoteCliCommand::ClientBridge {
                session: shepr_config::DEFAULT_SESSION_NAME,
            },
            "remote-client-bridge",
        ),
    ] {
        assert_eq!(
            shepr.command(&named_command.args()),
            format!("{} --session agents {line}", shepr.as_str())
        );
        assert_eq!(
            shepr.command(&default_command.args()),
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
    assert_eq!(resolved.quoted(), shell_quote(path));
}

#[test]
fn remote_bridge_command_passes_a_named_session() {
    let remote = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    assert!(
        remote
            .bridge_command("agents")
            .contains(" --session agents remote-client-bridge; shepr_exit_status=")
    );
}

#[test]
fn remote_bridge_command_uses_installed_binary() {
    let remote_shepr = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    assert_eq!(
        remote_shepr.bridge_command(shepr_config::DEFAULT_SESSION_NAME),
        format!(
            "/bin/sh -c 'echo; echo shepr-remote-output-ready; /usr/bin/shepr remote-client-bridge; shepr_exit_status=$?; if [ $shepr_exit_status -eq {SSH_OWN_FAILURE_EXIT_CODE} ]; then exit {REMAPPED_REMOTE_255_EXIT_CODE}; fi; exit $shepr_exit_status'"
        )
    );
    assert_eq!(
        remote_shepr.saved_bridge_command("agents"),
        "/usr/bin/shepr --session agents remote-client-bridge </dev/null"
    );
}

/// The bridge command is interpreted by /bin/sh, not by the login shell: the
/// login shell only sees `/bin/sh -c` and one quoted word without newlines.
#[test]
fn saved_bridge_command_does_not_depend_on_a_posix_login_shell() {
    let remote = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    let command = remote.bridge_command("agents");
    let script = command
        .strip_prefix("/bin/sh -c '")
        .and_then(|rest| rest.strip_suffix('\''))
        .expect("wrapped in /bin/sh -c");
    assert!(!script.contains('\''), "{script}");
    assert!(!script.contains('\n'), "{script}");

    // The script, run by a real /bin/sh, still frames its output with the marker.
    // host-program-ok: the generated remote script is the subject, run as sshd runs it
    let output = shepr_test_support::command_in_scratch("/bin/sh", "saved-bridge-command-sh")
        .arg("-c")
        .arg(posix_remote_output_command("printf payload"))
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
    use std::io::Write as _;

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
            .write_all(posix_remote_output_command(script).as_bytes())
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

/// The cached API-bridge command reaches the login shell as `/bin/sh -c`
/// plus one single-quoted word with no quote, backslash or newline inside,
/// and both of its branches behave when a real /bin/sh runs it.
#[test]
fn cached_api_command_does_not_depend_on_a_posix_login_shell() {
    use shepr_test_support::fixture::{self, Step};

    // The remote shepr: a fixture stand-in that passes the bridge check
    // and otherwise answers as the bridge, naming the session it was given.
    let dir = shepr_test_support::ScratchDir::new("api-command");
    let check = RemoteCliCommand::ApiBridge {
        session: "agents",
        check: true,
    }
    .args()
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    let fake = fixture::stand_in(
        &dir,
        "shepr",
        &[
            Step::When {
                operands: check,
                steps: vec![Step::Exit(0)],
            },
            Step::Print("bridged-".into()),
            Step::PrintArg(2),
            Step::Print("\n".into()),
        ],
    );

    let run = |executable: &str| {
        let executable = RemoteExecutable::parse(executable.to_owned()).expect("test precondition");
        let command = cached_remote_api_command(&executable, "agents");
        let script = command
            .strip_prefix("/bin/sh -c '")
            .and_then(|rest| rest.strip_suffix('\''))
            .expect("wrapped in /bin/sh -c")
            .to_owned();
        for forbidden in ['\'', '\n', '\\', '"'] {
            assert!(!script.contains(forbidden), "{forbidden:?} in {script}");
        }
        // host-program-ok: the generated remote script is the subject, run as sshd runs it
        shepr_test_support::command_in_scratch("/bin/sh", "cached-api-command-sh")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("test precondition")
    };

    let fake_path = fake.to_str().expect("test precondition").to_owned();
    let output = run(&fake_path);
    let mut stdout = output.stdout;
    assert!(output.status.success());
    normalize_remote_stdout(&mut stdout, true).expect("marker line present");
    assert_eq!(stdout, b"bridged-agents\n");

    let output = run("/nonexistent/shepr");
    assert_eq!(output.status.code(), Some(78));
    assert!(String::from_utf8_lossy(&output.stderr).contains(STALE_API_METADATA));
}
