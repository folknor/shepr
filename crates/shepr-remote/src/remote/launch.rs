use super::*;

use std::io;

pub(super) const REMOTE_OUTPUT_READY_MARKER: &str = "shepr-remote-output-ready";

pub fn run_remote(
    remote: RemoteLaunch,
    settings: super::SavedSshSettings,
    paths: &shepr_config::AppPaths,
    operator: &mut dyn Operator,
) -> io::Result<()> {
    let session_name = paths.session_id().display_name().to_owned();
    let runtime_dir = paths.xdg_runtime_dir();
    let local_socket = local_forward_socket_path(runtime_dir, &remote.target, &session_name)?;
    let program = std::env::args()
        .next()
        .unwrap_or_else(|| "shepr".to_string());
    let reattach_command =
        reattach_command(&program, &remote.target, &session_name, remote.keybindings);
    let remote_ssh = RemoteSsh::new(
        remote.target.clone(),
        settings.manage_ssh_config,
        session_name.clone(),
        paths,
    )?;
    let prepared_remote = prepare_remote_shepr(&remote_ssh)?;
    ensure_remote_server_ready(operator, &remote_ssh, &prepared_remote.remote_shepr)?;

    let bridge = SshStdioBridge::start(
        remote.target,
        &prepared_remote.remote_shepr,
        local_socket.clone(),
        &session_name,
        remote_ssh.options(),
        false,
    )?;

    // The bridge reports its own failure (typed, so the binary's hints can
    // classify it) instead of printing it under the client's TUI. When the
    // client exits unsuccessfully, that failure is the more useful cause.
    let launch_dir = paths
        .current_dir()
        .unwrap_or_else(|| std::path::Path::new("/"));
    run_client_process(
        &local_socket,
        &reattach_command,
        remote.keybindings,
        launch_dir,
    )
    .map_err(|client_error| bridge.reported_failure().unwrap_or(client_error))
}

pub fn check_saved_ssh(
    paths: &shepr_config::AppPaths,
    target: &SshTarget,
    session: &str,
    settings: super::SavedSshSettings,
) -> io::Result<()> {
    shepr_api::session::validate_name(session)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut ssh =
        RemoteSsh::new_noninteractive_with(target.clone(), settings.manage_ssh_config, paths)?;
    ssh.set_session_name(session.to_owned());
    let remote = find_installed_remote_shepr(&ssh)?;
    let status = remote_server_status(&ssh, &remote)?;
    ensure_remote_server_build(ssh.target(), &status)?;
    match status {
        RemoteServerStatus::Running {
            detached_server_daemon: true,
            ..
        } => Ok(()),
        _ => Err(io::Error::other(format!(
            "remote Shepr server is stopped or was not started as a detached daemon; run `{}`",
            super::saved_ssh_bootstrap_command(target.as_str(), session),
        ))),
    }
}

pub fn prepare_saved_ssh(
    paths: &shepr_config::AppPaths,
    target: &SshTarget,
    session_name: &str,
    settings: super::SavedSshSettings,
    operator: &mut dyn Operator,
) -> io::Result<RemoteExecutable> {
    shepr_api::session::validate_name(session_name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let ssh = RemoteSsh::new(
        target.clone(),
        settings.manage_ssh_config,
        session_name.to_owned(),
        paths,
    )?;
    let prepared = prepare_remote_shepr(&ssh)?;
    ensure_remote_server_ready(operator, &ssh, &prepared.remote_shepr)?;

    // The bridge already owns daemon startup. EOF closes only this temporary attachment,
    // leaving the named server running even when no local TUI is open yet.
    let command = prepared.remote_shepr.saved_bridge_command(session_name);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server startup failed", &output));
    }
    let status = remote_server_status(&ssh, &prepared.remote_shepr)?;
    ensure_remote_server_build(ssh.target(), &status)?;
    match status {
        RemoteServerStatus::Running {
            detached_server_daemon: true,
            ..
        } => Ok(prepared.remote_shepr.clone()),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "remote server is not ready for saved machines",
        )),
    }
}

impl RemoteExecutable {
    pub(super) fn quoted(&self) -> String {
        shell_quote(self.as_str())
    }

    pub(super) fn command(&self, args: &[&str]) -> String {
        let mut command = self.quoted();
        for arg in args {
            command.push(' ');
            command.push_str(&shell_quote(arg));
        }
        command
    }

    pub(super) fn session_command(&self, session_name: &str, args: &[&str]) -> String {
        self.command(&Self::session_args(session_name, args))
    }

    pub(super) fn session_args<'a>(session_name: &'a str, args: &[&'a str]) -> Vec<&'a str> {
        let mut session_args = Vec::with_capacity(args.len() + 2);
        if session_name != shepr_config::DEFAULT_SESSION_NAME {
            session_args.extend(["--session", session_name]);
        }
        session_args.extend_from_slice(args);
        session_args
    }

    pub(super) fn status_client_command(&self) -> String {
        format!(
            "test -x {} && {}",
            self.quoted(),
            self.command(&["status", "client", "--json"])
        )
    }

    pub(super) fn api_bridge_check_command(&self, session_name: &str) -> String {
        let path = self.quoted();
        let status = self.command(&["status", "client", "--json"]);
        let check = self.command(&["--session", session_name, "remote-api-bridge", "--check"]);
        format!("test -x {path} && {status} && {check} </dev/null")
    }

    pub(super) fn bridge_command(&self, session_name: &str) -> String {
        let args = Self::session_args(session_name, &["remote-client-bridge"]);
        // sshd hands this string to the user's login shell, which need not be POSIX
        // (xonsh, fish, nushell). Run the script under /bin/sh, as the API bridge does
        // (discovery feeds its script to `/bin/sh -s` instead), so the login shell only
        // has to launch one quoted command.
        posix_shell_command(&posix_remote_output_command(&self.command(&args)))
    }

    pub(super) fn saved_bridge_command(&self, session_name: &str) -> String {
        let args = Self::session_args(session_name, &["remote-client-bridge"]);
        // This redirects bridge stdin to /dev/null. The bridge forwards EOF as a
        // socket write shutdown, so the server's handshake reader returns without
        // waiting for its deadline.
        format!("{} </dev/null", self.command(&args))
    }
}

/// Prefixes `command` with the output-ready marker line (preceded by a newline, so
/// the marker starts a line of its own after any login banner) and maps a remote
/// command's exit 255 to 254. OpenSSH also uses 255 for its own failures, so the
/// wrapper keeps a remote program's 255 from being mistaken for a broken SSH link.
///
/// The prefix is deliberately plain words with no quotes or newlines. For a plain
/// `command` such as the client bridge's `<path> ...`, the wrapped result of
/// [`posix_shell_command`] reaches a non-POSIX login shell as `/bin/sh -c` plus one
/// single-quoted argument with nothing inside it to escape.
///
/// Scripts fed to `/bin/sh -s` end with a newline; it is trimmed so the status
/// suffix does not start a line with `;`, which is a shell syntax error.
pub(super) fn posix_remote_output_command(command: &str) -> String {
    let command = command.trim_end();
    format!(
        "echo; echo {REMOTE_OUTPUT_READY_MARKER}; {command}; shepr_exit_status=$?; if [ $shepr_exit_status -eq 255 ]; then exit 254; fi; exit $shepr_exit_status"
    )
}

/// Runs a POSIX script under `/bin/sh` regardless of the remote login shell.
pub(super) fn posix_shell_command(script: &str) -> String {
    format!("/bin/sh -c {}", shell_quote(script))
}

pub(super) struct PreparedRemoteShepr {
    pub(super) remote_shepr: RemoteExecutable,
}

pub(crate) const STALE_API_METADATA: &str = "shepr-machine-metadata-stale";
pub(crate) const STALE_API_METADATA_EXIT_CODE: i32 = 78;

pub(crate) fn cached_remote_api_command(executable: &RemoteExecutable, session: &str) -> String {
    let path = shell_quote(executable.as_str());
    let session = shell_quote(session);
    // The API bridge's stdin is the data channel, so unlike discovery this
    // script cannot be fed to `/bin/sh -s`; it has to reach the login shell as
    // `/bin/sh -c '<script>'`. Keep it to one line with no single quote,
    // backslash or double quote inside, so the login shell (xonsh, fish,
    // nushell or POSIX) sees one plain single-quoted word with nothing to
    // escape, the same as the client bridge command. A path or session that
    // needs quoting would bring `'\''` back; paths come from discovery and
    // session names are validated.
    let script = format!(
        "if {path} --session {session} remote-api-bridge --check </dev/null >/dev/null 2>&1; then {}; else echo {STALE_API_METADATA} >&2; exit {STALE_API_METADATA_EXIT_CODE}; fi",
        posix_remote_output_command(&format!("{path} --session {session} remote-api-bridge")),
    );
    posix_shell_command(&script)
}

pub(super) fn reattach_command(
    program: &str,
    target: &str,
    session_name: &str,
    keybindings: RemoteKeybindings,
) -> String {
    let program = shell_quote(if program.is_empty() { "shepr" } else { program });
    let target = shell_quote(target);
    let mut command = format!("{program} --remote {target}");
    if keybindings != RemoteKeybindings::Local {
        command.push_str(" --remote-keybindings ");
        command.push_str(keybindings.as_str());
    }
    if session_name != shepr_config::DEFAULT_SESSION_NAME {
        command.push_str(" --session ");
        command.push_str(&shell_quote(session_name));
    }
    command
}

pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn interactive_shell_command(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let mut command = shell_quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

#[cfg(test)]
mod shell_command_tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn interactive_shell_command_quotes_posix_arguments() {
        let argv = vec![
            "pi".into(),
            String::new(),
            "two words".into(),
            "a'b".into(),
            "$HOME".into(),
            "semi;colon".into(),
            "@options".into(),
        ];
        assert_eq!(
            interactive_shell_command(&argv).as_deref(),
            Some("pi '' 'two words' 'a'\\''b' '$HOME' 'semi;colon' @options")
        );
    }

    #[test]
    fn api_forwarding_probe_runs_status_and_check_under_posix_sh() {
        use shepr_test_support::fixture::{self, Step};
        use std::process::Stdio;

        // The remote shepr: a fixture stand-in answering exactly the two
        // invocations the probe makes, and failing any other.
        let scratch = shepr_test_support::ScratchDir::new("api-forwarding-probe");
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
                    &["status", "client", "--json"],
                    vec![
                        Step::Print(
                            "{\"version\":\"test\",\"build_id\":\"0123456789abcdef\"}\n".into(),
                        ),
                        Step::Exit(0),
                    ],
                ),
                answers(
                    &["--session", "agents", "remote-api-bridge", "--check"],
                    vec![Step::Exit(0)],
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
        let script = posix_remote_output_command(&executable.api_bridge_check_command("agents"));
        // host-program-ok: the generated remote script is the subject, run as sshd runs it
        let mut child =
            shepr_test_support::command_in_scratch("/bin/sh", "api-forwarding-probe-sh")
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
            .write_all(script.as_bytes())
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
