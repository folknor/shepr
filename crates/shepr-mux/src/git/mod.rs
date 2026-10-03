use std::path::PathBuf;

mod config;
mod discovery;
mod identity;
mod runner;
mod status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefBackend {
    Files,
    Reftable,
}

pub use self::{
    discovery::{discover_checkout_root, fallback_label_from_cwd},
    status::{
        AheadBehindState, GitStatusCache, GitStatusCacheEntry, GitStatusCacheView,
        GitStatusDiscovery, git_status_cache_key, git_status_discovery,
        git_status_snapshot_for_cwd, git_status_snapshot_for_discovery,
    },
};
pub use runner::{GitCommandError, run_git};

/// A Git command or repository read failed while deriving workspace information.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GitReadError {
    /// The Git executable could not be started.
    Spawn { cwd: PathBuf, message: String },
    /// Git did not finish before the probe deadline.
    TimedOut { cwd: PathBuf, arguments: String },
    /// Git finished unsuccessfully for a probe that requires success.
    CommandFailed {
        cwd: PathBuf,
        arguments: String,
        status: Option<i32>,
        stderr: String,
    },
    /// The Git child could not be polled, waited for, or killed.
    Process { cwd: PathBuf, message: String },
    /// Git returned output that was not UTF-8.
    InvalidUtf8 { cwd: PathBuf, arguments: String },
    /// Git returned text that did not match the requested output format.
    InvalidOutput {
        cwd: PathBuf,
        arguments: String,
        output: String,
    },
    /// Git's config environment is refused, incomplete, or invalid.
    ConfigEnvironment { message: String },
    /// A repository file could not be read or was not safe to trust.
    FileRead { path: PathBuf, message: String },
}

impl std::fmt::Display for GitReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { cwd, message } => {
                write!(
                    formatter,
                    "could not start git in {}: {message}",
                    cwd.display()
                )
            }
            Self::TimedOut { cwd, arguments } => {
                write!(formatter, "git {arguments} timed out in {}", cwd.display())
            }
            Self::CommandFailed {
                cwd,
                arguments,
                status,
                stderr,
            } => write!(
                formatter,
                "git {arguments} failed in {} with status {status:?}: {}",
                cwd.display(),
                stderr.trim()
            ),
            Self::Process { cwd, message } => {
                write!(
                    formatter,
                    "git process failed in {}: {message}",
                    cwd.display()
                )
            }
            Self::InvalidUtf8 { cwd, arguments } => write!(
                formatter,
                "git {arguments} returned non-UTF-8 output in {}",
                cwd.display()
            ),
            Self::InvalidOutput {
                cwd,
                arguments,
                output,
            } => write!(
                formatter,
                "git {arguments} returned unexpected output in {}: {output:?}",
                cwd.display()
            ),
            Self::ConfigEnvironment { message } => {
                write!(formatter, "Git config environment is invalid: {message}")
            }
            Self::FileRead { path, message } => {
                write!(formatter, "could not read {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for GitReadError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AheadBehind {
    pub ahead: usize,
    pub behind: usize,
}

/// The result of reading HEAD, distinct from whether a repository was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceBranch {
    OutsideRepository,
    Detached,
    Named(String),
    ReadFailed,
}

impl WorkspaceBranch {
    /// The branch text drawn by consumers that do not display read failures.
    pub fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name),
            Self::OutsideRepository | Self::Detached | Self::ReadFailed => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatus {
    pub workspace_id: String,
    pub resolved_identity_cwd: PathBuf,
    pub status_cache_key: PathBuf,
    pub auto_label: String,
    pub branch: WorkspaceBranch,
    pub ahead_behind: Option<AheadBehind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatusSnapshot {
    pub repo_root: Option<PathBuf>,
    pub branch: WorkspaceBranch,
    pub ahead_behind: Option<AheadBehind>,
}

impl WorkspaceGitStatusSnapshot {
    pub fn into_workspace_status(
        self,
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
        status_cache_key: PathBuf,
    ) -> WorkspaceGitStatus {
        let home = if self.repo_root.is_none() {
            shepr_core::pathutil::home_dir().ok()
        } else {
            None
        };
        let auto_label = shepr_core::workspace_label::workspace_label_from_cwd(
            &resolved_identity_cwd,
            self.repo_root.as_deref(),
            home.as_deref(),
        );
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd,
            status_cache_key,
            auto_label,
            branch: self.branch,
            ahead_behind: self.ahead_behind,
        }
    }
}

#[cfg(test)]
mod config_tests;

#[cfg(test)]
pub mod test_support;
