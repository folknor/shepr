use std::path::PathBuf;
use std::time::Instant;

use shepr_agent::detect::AgentState;
use shepr_protocol::TerminalId;

use shepr_agent::ownership::AgentOwnership;
pub use shepr_agent::ownership::{EffectiveStateChange, HookAuthority, HookClockSample};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalTitleChange {
    pub raw_changed: bool,
    pub stripped_changed: bool,
}

/// The deferred resume belongs to its terminal, including the command held
/// while its shell is launching. Removing the terminal removes the whole state.
#[derive(Debug, Clone, Default)]
pub enum AgentResumeState {
    #[default]
    None,
    Planned(shepr_agent::agent::resume::AgentResumePlan),
    Launching {
        plan: shepr_agent::agent::resume::AgentResumePlan,
        command: Option<bytes::Bytes>,
    },
}

impl AgentResumeState {
    pub fn is_pending(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn is_launching(&self) -> bool {
        matches!(self, Self::Launching { .. })
    }

    pub fn candidate(
        &self,
        has_runtime: bool,
    ) -> Option<&shepr_agent::agent::resume::AgentResumePlan> {
        match self {
            Self::Planned(plan) if !has_runtime => Some(plan),
            _ => None,
        }
    }

    pub fn begin_launch(&mut self, command: bytes::Bytes) {
        if let Self::Planned(plan) = self {
            *self = Self::Launching {
                plan: plan.clone(),
                command: Some(command),
            };
        }
    }

    pub fn take_command(&mut self) -> Option<bytes::Bytes> {
        match self {
            Self::Launching { command, .. } => command.take(),
            _ => None,
        }
    }
}

/// Why a saved pane has no running shell. The pane surface renders its
/// `guidance` and `cause`; detect requests include its `Display` text in the
/// existing API error message when a pane has no runtime. Causes are stored as
/// strings when the failure is recorded, so both presentations can borrow
/// them instead of retaining an OS error object.
#[derive(Debug)]
pub enum RestoreFailure {
    DirectoryUnavailable {
        path: PathBuf,
    },
    DirectoryUnreadable {
        path: PathBuf,
        error: String,
    },
    ShellStartFailed {
        error: String,
    },
    /// The saved agent's resume cannot be issued at all (no command to run,
    /// the pane gone from under the attempt), whatever the directory and shell.
    ResumeUnavailable {
        reason: String,
    },
}

impl RestoreFailure {
    pub fn resume_unavailable(reason: impl Into<String>) -> Self {
        Self::ResumeUnavailable {
            reason: reason.into(),
        }
    }

    pub fn directory_unreadable(path: PathBuf, error: &std::io::Error) -> Self {
        Self::DirectoryUnreadable {
            path,
            error: error.to_string(),
        }
    }

    pub fn shell_start_failed(error: &std::io::Error) -> Self {
        Self::ShellStartFailed {
            error: error.to_string(),
        }
    }

    /// What the operator should do about the failure.
    pub fn guidance(&self) -> &'static str {
        match self {
            Self::DirectoryUnavailable { .. } => {
                "Saved directory is unavailable. Restore the directory and restart this session."
            }
            Self::DirectoryUnreadable { .. } => {
                "Saved directory cannot be read. Fix its access and restart this session."
            }
            Self::ShellStartFailed { .. } => {
                "Could not start the saved shell. Fix the shell configuration and restart this session."
            }
            Self::ResumeUnavailable { .. } => {
                "Could not resume the saved agent. Restart this session."
            }
        }
    }

    /// The error behind the failure, when there is one.
    pub fn cause(&self) -> Option<&str> {
        match self {
            Self::DirectoryUnavailable { .. } => None,
            Self::DirectoryUnreadable { error, .. } | Self::ShellStartFailed { error } => {
                Some(error)
            }
            Self::ResumeUnavailable { reason } => Some(reason),
        }
    }
}

impl std::fmt::Display for RestoreFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.guidance())?;
        match self {
            Self::DirectoryUnavailable { path } | Self::DirectoryUnreadable { path, .. } => {
                write!(formatter, " Directory: {}.", path.display())?;
            }
            Self::ShellStartFailed { .. } | Self::ResumeUnavailable { .. } => {}
        }
        if let Some(cause) = self.cause() {
            write!(formatter, " Error: {cause}")?;
        }
        Ok(())
    }
}

/// Pure state for a server-owned terminal: identity, cwd, labels, restore
/// state and the pane's [`AgentOwnership`].
///
/// One-to-one with a pane-backed PTY. Agent arbitration lives in
/// `shepr_agent::ownership`; this type only holds the machine.
pub struct TerminalState {
    pub id: TerminalId,
    cwd: PathBuf,
    terminal_title: Option<String>,
    manual_label: Option<String>,
    ownership: AgentOwnership,
    agent_resume: AgentResumeState,
    restore_error: Option<RestoreFailure>,
}

impl TerminalState {
    pub fn ownership(&self) -> &AgentOwnership {
        &self.ownership
    }
    pub fn ownership_mut(&mut self) -> &mut AgentOwnership {
        &mut self.ownership
    }
    pub fn terminal_title(&self) -> Option<&str> {
        self.terminal_title.as_deref()
    }
    pub fn manual_label(&self) -> Option<&str> {
        self.manual_label.as_deref()
    }
    pub fn agent_resume(&self) -> &AgentResumeState {
        &self.agent_resume
    }
    pub fn plan_agent_resume(&mut self, plan: shepr_agent::agent::resume::AgentResumePlan) {
        self.agent_resume = AgentResumeState::Planned(plan);
    }
    pub fn begin_agent_resume_launch(&mut self, command: bytes::Bytes) {
        self.agent_resume.begin_launch(command);
    }
    pub fn take_agent_resume_command(&mut self) -> Option<bytes::Bytes> {
        self.agent_resume.take_command()
    }
    pub fn clear_agent_resume(&mut self) {
        self.agent_resume = AgentResumeState::None;
    }
    pub fn restore_error(&self) -> Option<&RestoreFailure> {
        self.restore_error.as_ref()
    }
    pub fn record_start_failure(&mut self, failure: RestoreFailure) {
        self.restore_error = Some(failure);
    }
}

mod detection;
mod hooks;
mod init;
mod names;
mod presentation;
mod sessions;
