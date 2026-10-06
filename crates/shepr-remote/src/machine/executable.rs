use std::fmt;
use std::path::Path;

use crate::args::RemoteCliCommand;
use crate::host::BridgeMode;
use crate::limits::MAX_REMOTE_EXECUTABLE_BYTES;
use crate::shell_command::{
    AccountShellCommand, PosixScript, posix_remote_output_command, posix_shell_command,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteExecutableError {
    NotAbsolute,
    TooLong,
    ContainsControlCharacters,
    NeedsShellQuoting,
}

impl fmt::Display for RemoteExecutableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAbsolute => {
                formatter.write_str("remote Shepr executable path must be absolute")
            }
            Self::TooLong => write!(
                formatter,
                "remote Shepr executable path must be at most {MAX_REMOTE_EXECUTABLE_BYTES} bytes"
            ),
            Self::ContainsControlCharacters => formatter
                .write_str("remote Shepr executable path must not contain control characters"),
            Self::NeedsShellQuoting => formatter.write_str(
                "remote Shepr executable path must contain only unquoted shell-safe characters",
            ),
        }
    }
}

impl std::error::Error for RemoteExecutableError {}

/// A checked absolute path to the Shepr executable on a remote machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteExecutable(String);

impl RemoteExecutable {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, RemoteExecutableError> {
        let value = value.into();
        if !Path::new(&value).is_absolute() {
            return Err(RemoteExecutableError::NotAbsolute);
        }
        if value.len() > MAX_REMOTE_EXECUTABLE_BYTES {
            return Err(RemoteExecutableError::TooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(RemoteExecutableError::ContainsControlCharacters);
        }
        // The command is nested inside /bin/sh -c and then parsed by the remote
        // account shell. Keep paths as plain shell words because nested quote
        // escaping is not reliable across the non-POSIX account shells we support.
        if !Self::is_shell_plain_word(&value) {
            return Err(RemoteExecutableError::NeedsShellQuoting);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// This checked path can be placed in an account-shell command without quoting.
    pub(crate) fn shell_word(&self) -> &str {
        self.as_str()
    }

    fn is_shell_plain_word(value: &str) -> bool {
        shepr_core::shell_quote::is_plain_word(value)
    }

    /// The remote CLI invocation as a script for `RemoteSsh::sh_output`.
    pub(crate) fn command(&self, args: &[&str]) -> PosixScript {
        let arguments = shepr_core::shell_quote::join_argv(args.iter().copied());
        PosixScript::new(if arguments.is_empty() {
            self.shell_word().to_owned()
        } else {
            format!("{} {arguments}", self.shell_word())
        })
    }

    pub(crate) fn status_client_command(&self) -> PosixScript {
        let args = RemoteCliCommand::ClientStatus.args();
        self.command(&args)
    }

    /// The bridge launch as the command sshd hands the account shell.
    pub(crate) fn bridge_command(&self, mode: BridgeMode) -> AccountShellCommand {
        self.account_shell_command(RemoteCliCommand::client_bridge(mode))
    }

    /// The remote wait for a server, as the command sshd hands the account
    /// shell.
    pub(crate) fn wait_for_server_command(&self) -> AccountShellCommand {
        self.account_shell_command(RemoteCliCommand::WaitForServer)
    }

    fn account_shell_command(&self, command: RemoteCliCommand<'_>) -> AccountShellCommand {
        let args = command.args();
        // sshd hands this string to the user's account shell, which need not be POSIX
        // (xonsh, fish, nushell). Run the script under /bin/sh (discovery feeds its
        // script to `/bin/sh -s` instead), so the account shell only has to launch one
        // quoted command.
        posix_shell_command(&posix_remote_output_command(&self.command(&args)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_executable_rejects_non_absolute_control_and_oversized_paths() {
        assert_eq!(
            RemoteExecutable::parse("relative/shepr"),
            Err(RemoteExecutableError::NotAbsolute)
        );
        assert_eq!(
            RemoteExecutable::parse("/home/user/shepr\nmalformed"),
            Err(RemoteExecutableError::ContainsControlCharacters)
        );
        let oversized = format!("/{}", "x".repeat(MAX_REMOTE_EXECUTABLE_BYTES));
        assert_eq!(
            RemoteExecutable::parse(oversized),
            Err(RemoteExecutableError::TooLong)
        );
    }
}
