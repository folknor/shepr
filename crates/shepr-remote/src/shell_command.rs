use crate::failure::{REMAPPED_REMOTE_255_EXIT_CODE, SSH_OWN_FAILURE_EXIT_CODE};

/// The line a remote command's wrapper prints before its own output, so the
/// local side can discard whatever the account shell printed at startup.
pub(crate) const REMOTE_OUTPUT_READY_MARKER: &str = "shepr-remote-output-ready";

/// A script interpreted by the explicitly selected POSIX shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PosixScript(String);

impl PosixScript {
    pub(crate) fn new(script: impl Into<String>) -> Self {
        Self(script.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A command string handed to sshd's configured account shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountShellCommand(String);

impl AccountShellCommand {
    /// The account shell parses only this plain external command; the POSIX
    /// script itself travels over stdin and is interpreted by `/bin/sh`.
    pub(crate) fn posix_script_stdin() -> Self {
        Self("/bin/sh -s".to_owned())
    }

    /// Marks text that is already encoded for sshd's account-shell command
    /// boundary, such as the remote executable's bridge command.
    pub(crate) fn from_account_shell_text(command: impl Into<String>) -> Self {
        Self(command.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
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
pub(crate) fn posix_remote_output_command(command: &PosixScript) -> PosixScript {
    let command = command.as_str().trim_end();
    PosixScript::new(format!(
        "echo; echo {REMOTE_OUTPUT_READY_MARKER}; {command}; shepr_exit_status=$?; if [ $shepr_exit_status -eq {SSH_OWN_FAILURE_EXIT_CODE} ]; then exit {REMAPPED_REMOTE_255_EXIT_CODE}; fi; exit $shepr_exit_status"
    ))
}

/// Runs a POSIX script under `/bin/sh` regardless of the remote account shell.
pub(crate) fn posix_shell_command(script: &PosixScript) -> AccountShellCommand {
    AccountShellCommand::from_account_shell_text(format!(
        "/bin/sh -c {}",
        shell_quote(script.as_str())
    ))
}

pub(crate) fn shell_quote(value: &str) -> String {
    shepr_core::shell_quote::quote(value)
}

#[cfg(test)]
mod tests;
