use super::*;

use std::io;

pub(super) const REMOTE_OUTPUT_READY_MARKER: &str = "shepr-remote-output-ready:1";

pub(crate) fn run_remote(
    remote: RemoteLaunch,
    settings: super::SavedSshSettings,
    paths: &shepr_config::AppPaths,
) -> io::Result<()> {
    let session_name = paths.session_id().display_name().to_owned();
    let local_socket = local_forward_socket_path(&remote.target, &session_name);
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
    );
    let prepared_remote = prepare_remote_shepr(&remote_ssh)?;
    ensure_remote_server_ready(&remote_ssh, &prepared_remote.remote_shepr)?;

    let _bridge = SshStdioBridge::start(
        remote.target,
        &prepared_remote.remote_shepr,
        local_socket.clone(),
        &session_name,
        remote_ssh.options(),
        false,
    )?;

    run_client_process(&local_socket, &reattach_command, remote.keybindings)
}

pub(crate) fn check_saved_ssh(
    paths: &shepr_config::AppPaths,
    target: &SshTarget,
    session: &str,
    settings: super::SavedSshSettings,
) -> io::Result<()> {
    crate::session::validate_name(session)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut ssh =
        RemoteSsh::new_noninteractive_with(target.clone(), settings.manage_ssh_config, paths);
    ssh.set_session_name(session.to_owned());
    let remote = find_installed_remote_shepr(&ssh)?;
    match remote_server_status(&ssh, &remote)? {
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

pub(crate) fn prepare_saved_ssh(
    paths: &shepr_config::AppPaths,
    target: &SshTarget,
    session_name: &str,
    settings: super::SavedSshSettings,
) -> io::Result<RemoteExecutable> {
    crate::session::validate_name(session_name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let ssh = RemoteSsh::new(
        target.clone(),
        settings.manage_ssh_config,
        session_name.to_owned(),
        paths,
    );
    let prepared = prepare_remote_shepr(&ssh)?;
    ensure_remote_server_ready(&ssh, &prepared.remote_shepr)?;

    // The bridge already owns daemon startup. EOF closes only this temporary attachment,
    // leaving the named server running even when no local TUI is open yet.
    let command = prepared.remote_shepr.saved_bridge_command(session_name);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server startup failed", &output));
    }
    match remote_server_status(&ssh, &prepared.remote_shepr)? {
        RemoteServerStatus::Running {
            detached_server_daemon: true,
            ..
        } => Ok(prepared.remote_shepr.clone()),
        _ => Err(io::Error::other(
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
        format!(
            "test -x {} && {} </dev/null",
            self.quoted(),
            self.command(&["--session", session_name, "remote-api-bridge", "--check"])
        )
    }

    pub(super) fn bridge_command(&self, session_name: &str, idle_timeout: bool) -> String {
        let command = if idle_timeout {
            &["remote-client-bridge", "--idle-timeout-v1"][..]
        } else {
            &["remote-client-bridge"][..]
        };
        let args = Self::session_args(session_name, command);
        // sshd hands this string to the user's login shell, which need not be POSIX
        // (xonsh, fish, nushell). Run the script under /bin/sh, as the API bridge does
        // (discovery feeds its script to `/bin/sh -s` instead), so the login shell only
        // has to launch one quoted command.
        posix_shell_command(&posix_remote_output_command(&format!(
            "exec {}",
            self.command(&args)
        )))
    }

    pub(super) fn saved_bridge_command(&self, session_name: &str) -> String {
        let args = Self::session_args(session_name, &["remote-client-bridge"]);
        format!("exec {} </dev/null", self.command(&args))
    }
}

/// Prefixes `command` with the output-ready marker line (preceded by a newline, so
/// the marker starts a line of its own after any login banner).
///
/// The prefix is deliberately plain words with no quotes or newlines. For a plain
/// `command` such as the client bridge's `exec <path> ...`, the wrapped result of
/// [`posix_shell_command`] reaches a non-POSIX login shell as `/bin/sh -c` plus one
/// single-quoted argument with nothing inside it to escape.
pub(super) fn posix_remote_output_command(command: &str) -> String {
    format!("echo; echo {REMOTE_OUTPUT_READY_MARKER}; {command}")
}

/// Runs a POSIX script under `/bin/sh` regardless of the remote login shell.
pub(super) fn posix_shell_command(script: &str) -> String {
    format!("/bin/sh -c {}", shell_quote(script))
}

pub(super) struct PreparedRemoteShepr {
    pub(super) remote_shepr: RemoteExecutable,
}

pub(in crate::remote) const STALE_API_METADATA: &str = "shepr-machine-metadata-stale-v1";

pub(in crate::remote) fn cached_remote_api_command(
    executable: &RemoteExecutable,
    session: &str,
) -> String {
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
        "if {path} --session {session} remote-api-bridge --check </dev/null >/dev/null 2>&1; then {}; else echo {STALE_API_METADATA} >&2; exit 78; fi",
        posix_remote_output_command(&format!(
            "exec {path} --session {session} remote-api-bridge"
        )),
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

pub(crate) fn shell_quote(value: &str) -> String {
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

pub(crate) fn interactive_shell_command(argv: &[String]) -> Option<String> {
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
}
