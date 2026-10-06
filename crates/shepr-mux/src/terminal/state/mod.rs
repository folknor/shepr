use std::path::PathBuf;
use std::time::Instant;

use crate::Label;
use shepr_agent::AgentState;

use shepr_detect::ownership::AgentOwnership;
pub use shepr_detect::ownership::{EffectiveStateChange, HookAuthority};

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
    Planned(shepr_agent::resume::AgentResumePlan),
    Launching {
        plan: shepr_agent::resume::AgentResumePlan,
        command: Option<bytes::Bytes>,
    },
}

impl AgentResumeState {
    pub fn plan(&self) -> Option<&shepr_agent::resume::AgentResumePlan> {
        match self {
            Self::Planned(plan) | Self::Launching { plan, .. } => Some(plan),
            Self::None => None,
        }
    }

    pub fn is_pending(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn is_launching(&self) -> bool {
        matches!(self, Self::Launching { .. })
    }

    pub fn candidate(&self, has_runtime: bool) -> Option<&shepr_agent::resume::AgentResumePlan> {
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

/// Why a pane has no running shell, whether newly opened or restored: its
/// start failed, or a deferred agent resume failed and its shell was ended.
/// The pane surface renders its `guidance` and `cause`; detect requests
/// include its `Display` text in the existing API error message when a pane
/// has no runtime. OS causes retain their errno and error kind until the
/// presentation boundary.
#[derive(Debug)]
pub enum PaneStartFailure {
    DirectoryUnavailable {
        path: PathBuf,
        error: std::io::Error,
    },
    DirectoryUnreadable {
        path: PathBuf,
        error: std::io::Error,
    },
    ShellStartFailed {
        program: Option<PathBuf>,
        error: std::io::Error,
    },
    /// The launch's status channel failed while its child lived, so the
    /// child could not be observed and was ended.
    LaunchUnobservable { error: std::io::Error },
    /// The shell launch was unconfirmed or its resume command could not be sent.
    ResumeUnavailable { reason: ResumeUnavailableReason },
    /// A failed resume retains the validated plan for manual recovery.
    ResumeFailed {
        plan: shepr_agent::resume::AgentResumePlan,
        failure: Box<PaneStartFailure>,
    },
}

/// Why an agent session's resume attempt could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeUnavailableReason {
    ShellLaunchUnconfirmed,
    CommandSendFailed,
}

impl ResumeUnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ShellLaunchUnconfirmed => {
                "the shell for the resume did not confirm that it started"
            }
            Self::CommandSendFailed => "the resume command could not be sent to the shell",
        }
    }
}

impl PaneStartFailure {
    pub fn resume_unavailable(reason: ResumeUnavailableReason) -> Self {
        Self::ResumeUnavailable { reason }
    }

    pub fn directory_unreadable(path: PathBuf, error: &std::io::Error) -> Self {
        Self::DirectoryUnreadable {
            path,
            error: copy_io_error(error),
        }
    }

    pub fn shell_start_failed(error: &std::io::Error) -> Self {
        Self::ShellStartFailed {
            program: None,
            error: copy_io_error(error),
        }
    }

    pub fn launch_unobservable(error: &std::io::Error) -> Self {
        Self::LaunchUnobservable {
            error: copy_io_error(error),
        }
    }

    /// What the operator should do about the failure.
    /// Failed resumes carry their manual command in `cause`.
    /// This layer has neither the build profile nor selected socket,
    /// so it names pane actions rather than inventing a restart command.
    pub fn guidance(&self) -> &'static str {
        match self {
            Self::DirectoryUnavailable { .. } => {
                "Pane directory is unavailable. Restore the directory, then close this pane and open a new one."
            }
            Self::DirectoryUnreadable { .. } => {
                "Pane directory cannot be read. Fix its access, then close this pane and open a new one."
            }
            Self::ShellStartFailed { .. } => {
                "Could not start the pane shell. Check the shell executable and [terminal].default_shell in server.toml. Config changes take effect at the next server launch."
            }
            Self::LaunchUnobservable { .. } => {
                "Could not confirm that the pane shell started, so it was stopped. Close this pane and open a new one."
            }
            Self::ResumeUnavailable { .. } | Self::ResumeFailed { .. } => {
                "Could not resume the saved agent. Open a new pane and resume it with the agent's own resume command."
            }
        }
    }

    /// The error behind the failure, when there is one.
    pub fn cause(&self) -> Option<std::borrow::Cow<'_, str>> {
        match self {
            Self::DirectoryUnavailable { error, .. }
            | Self::DirectoryUnreadable { error, .. }
            | Self::LaunchUnobservable { error }
            | Self::ShellStartFailed {
                error,
                program: None,
            } => Some(error.to_string().into()),
            Self::ShellStartFailed {
                program: Some(program),
                error,
            } => Some(format!("{}: {error}", program.display()).into()),
            Self::ResumeUnavailable { reason } => Some(reason.as_str().into()),
            Self::ResumeFailed { plan, failure } => Some(
                format!(
                    "{failure} Agent: {}. Session: {}. Manual resume: {}",
                    plan.agent().label(),
                    plan.key().session_ref().value_str(),
                    plan.to_shell_command(),
                )
                .into(),
            ),
        }
    }
}

impl std::fmt::Display for PaneStartFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.guidance())?;
        match self {
            Self::DirectoryUnavailable { path, .. } | Self::DirectoryUnreadable { path, .. } => {
                write!(formatter, " Directory: {}.", path.display())?;
            }
            Self::ShellStartFailed { .. }
            | Self::LaunchUnobservable { .. }
            | Self::ResumeUnavailable { .. }
            | Self::ResumeFailed { .. } => {}
        }
        if let Some(cause) = self.cause() {
            write!(formatter, " Error: {cause}")?;
        }
        Ok(())
    }
}

// An io::Error may come from preparation before a child exists, where there
// is no errno. Preserve its kind and diagnostic in that case too.
fn copy_io_error(error: &std::io::Error) -> std::io::Error {
    match error.raw_os_error() {
        Some(errno) => std::io::Error::from_raw_os_error(errno),
        None => std::io::Error::new(error.kind(), error.to_string()),
    }
}

/// Pure state for a server-owned terminal: cwd, labels, restore state and the
/// pane's [`AgentOwnership`].
///
/// One-to-one with a pane-backed PTY, and identified by its pane. Agent
/// arbitration lives in `shepr_detect::ownership`; this type only holds the
/// machine.
pub struct TerminalState {
    cwd: shepr_core::absolute_path::AbsolutePath,
    terminal_title: Option<String>,
    manual_label: Option<Label>,
    ownership: AgentOwnership,
    agent_resume: AgentResumeState,
    start_failure: Option<PaneStartFailure>,
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
        self.manual_label.as_ref().map(Label::as_str)
    }
    pub fn manual_label_value(&self) -> Option<&Label> {
        self.manual_label.as_ref()
    }
    pub fn agent_resume(&self) -> &AgentResumeState {
        &self.agent_resume
    }
    /// Fixture seam for seeding a pending plan in dependent crates' tests.
    /// Production restore uses the consuming constructor before scheduling;
    /// injecting a plan after the server schedule retires will not run it.
    pub fn plan_agent_resume(&mut self, plan: shepr_agent::resume::AgentResumePlan) {
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
    /// Why the pane's shell could not start, for fresh launches and restored
    /// panes alike.
    pub fn start_failure(&self) -> Option<&PaneStartFailure> {
        self.start_failure.as_ref()
    }
    pub fn record_start_failure(&mut self, failure: PaneStartFailure) {
        self.start_failure = Some(failure);
    }
}

mod init;
mod labels;
mod resume;
mod titles;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod start_failure_tests {
    #[test]
    fn resume_failure_guidance_names_a_pane_action() {
        let failure = super::PaneStartFailure::resume_unavailable(
            super::ResumeUnavailableReason::CommandSendFailed,
        );
        assert!(failure.guidance().contains("Open a new pane"));
        assert!(!failure.guidance().contains("restart this session"));
    }

    use super::PaneStartFailure;

    #[test]
    fn shell_failure_keeps_errno_and_program_until_presentation() {
        let failure = PaneStartFailure::ShellStartFailed {
            program: Some("/missing-shell".into()),
            error: std::io::Error::from_raw_os_error(libc::ENOENT),
        };
        let PaneStartFailure::ShellStartFailed { error, .. } = &failure else {
            panic!("shell failure");
        };
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
        assert!(
            failure
                .cause()
                .expect("OS cause")
                .starts_with("/missing-shell: ")
        );
    }

    #[test]
    fn preparation_errors_keep_their_kind_without_an_errno() {
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "preparation denied");
        let failure = PaneStartFailure::shell_start_failed(&error);
        let PaneStartFailure::ShellStartFailed { program, error } = failure else {
            panic!("shell failure");
        };
        assert!(program.is_none());
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(error.raw_os_error(), None);
        assert_eq!(error.to_string(), "preparation denied");
    }
}
