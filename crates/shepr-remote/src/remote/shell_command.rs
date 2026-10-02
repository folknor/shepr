use crate::machine::RemoteExecutable;

/// A script interpreted by the explicitly selected POSIX shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PosixScript(String);

impl PosixScript {
    pub(super) fn new(script: impl Into<String>) -> Self {
        Self(script.into())
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A command string handed to sshd's configured account shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AccountShellCommand(String);

impl AccountShellCommand {
    /// The account shell parses only this plain external command; the POSIX
    /// script itself travels over stdin and is interpreted by `/bin/sh`.
    pub(super) fn posix_script_stdin() -> Self {
        Self("/bin/sh -s".to_owned())
    }

    /// Marks text that is already encoded for sshd's account-shell command
    /// boundary, such as the remote executable's bridge command.
    pub(super) fn from_account_shell_text(command: impl Into<String>) -> Self {
        Self(command.into())
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl RemoteExecutable {
    /// The remote CLI invocation as a script for `RemoteSsh::sh_output`.
    pub(super) fn command_as_posix_script(&self, arguments: &[&str]) -> PosixScript {
        PosixScript::new(self.command(arguments))
    }

    /// The bridge launch as the command sshd hands the account shell.
    pub(super) fn bridge_command_as_account_shell(&self) -> AccountShellCommand {
        AccountShellCommand::from_account_shell_text(self.bridge_command())
    }
}
