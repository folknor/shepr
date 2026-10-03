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

/// Discovery distinguishes a checkout from a cwd for which no repository was found.
/// Undiscovered workspaces have no key until the worker supplies one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GitStatusKey {
    Checkout(PathBuf),
    Outside(PathBuf),
}

impl GitStatusKey {
    pub fn as_path(&self) -> &std::path::Path {
        match self {
            Self::Checkout(path) | Self::Outside(path) => path,
        }
    }
}

/// The OS identity of an I/O failure, retained across cache clones.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GitIoError {
    kind: std::io::ErrorKind,
    errno: Option<i32>,
}

impl From<&std::io::Error> for GitIoError {
    fn from(error: &std::io::Error) -> Self {
        Self {
            kind: error.kind(),
            errno: error.raw_os_error(),
        }
    }
}

impl std::fmt::Display for GitIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.errno {
            Some(errno) => write!(f, "{}", std::io::Error::from_raw_os_error(errno)),
            None => write!(f, "{}", std::io::Error::from(self.kind)),
        }
    }
}

/// Structured causes retain error identity without depending on diagnostic prose.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileReadReason {
    Io(GitIoError),
    ReadLimit { bytes: usize },
    InvalidGitfile,
    InvalidRefName,
    InvalidObjectId,
    PackedRefLineTooLarge,
    ConfigWithoutParent,
    InvalidConfigName,
    ConfigMetadataUnavailable,
}

impl From<&std::io::Error> for FileReadReason {
    fn from(error: &std::io::Error) -> Self {
        Self::Io(GitIoError::from(error))
    }
}

impl std::fmt::Display for FileReadReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::ReadLimit { bytes } => write!(f, "file exceeds the {bytes}-byte read limit"),
            Self::InvalidGitfile => f.write_str("gitfile has no gitdir target"),
            Self::InvalidRefName => f.write_str("invalid ref name"),
            Self::InvalidObjectId => f.write_str("not a complete object ID"),
            Self::PackedRefLineTooLarge => f.write_str("packed ref line is too large"),
            Self::ConfigWithoutParent => f.write_str("config has no parent"),
            Self::InvalidConfigName => f.write_str("invalid config name"),
            Self::ConfigMetadataUnavailable => {
                f.write_str("repository config metadata is unavailable")
            }
        }
    }
}

/// An environment refusal with stable hashing for read-error deduplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitConfigEnvironmentError(pub shepr_core::env::EnvError);

impl std::hash::Hash for GitConfigEnvironmentError {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        use shepr_core::env::EnvRefusal;
        self.0.var.hash(state);
        std::mem::discriminant(&self.0.refusal).hash(state);
        match &self.0.refusal {
            EnvRefusal::NotUtf8 | EnvRefusal::EmptySelector => {}
            EnvRefusal::SurroundingWhitespace(value)
            | EnvRefusal::NotAFlag(value)
            | EnvRefusal::NotAbsolute(value) => value.hash(state),
        }
    }
}

/// A Git command or repository read failed while deriving workspace information.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GitReadError {
    /// The Git executable could not be started.
    Spawn { cwd: PathBuf, reason: GitIoError },
    /// Git did not finish before the probe deadline.
    TimedOut {
        cwd: PathBuf,
        arguments: Vec<String>,
    },
    /// Git finished unsuccessfully for a probe that requires success.
    CommandFailed {
        cwd: PathBuf,
        arguments: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    /// The Git child could not be polled, waited for, or killed.
    Process { cwd: PathBuf, reason: GitIoError },
    /// Git returned output that was not UTF-8.
    InvalidUtf8 {
        cwd: PathBuf,
        arguments: Vec<String>,
    },
    /// Git returned text that did not match the requested output format.
    InvalidOutput {
        cwd: PathBuf,
        arguments: Vec<String>,
        output: String,
    },
    /// Git's config environment is refused, incomplete, or invalid.
    ConfigEnvironment { error: GitConfigEnvironmentError },
    /// A repository file could not be read or was not safe to trust.
    FileRead {
        path: PathBuf,
        reason: FileReadReason,
    },
}

impl std::fmt::Display for GitReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { cwd, reason } => {
                write!(
                    formatter,
                    "could not start git in {}: {reason}",
                    cwd.display()
                )
            }
            Self::TimedOut { cwd, arguments } => {
                write!(
                    formatter,
                    "git {arguments:?} timed out in {}",
                    cwd.display()
                )
            }
            Self::CommandFailed {
                cwd,
                arguments,
                status,
                stderr,
            } => write!(
                formatter,
                "git {arguments:?} failed in {} with status {status:?}: {}",
                cwd.display(),
                stderr.trim()
            ),
            Self::Process { cwd, reason } => {
                write!(
                    formatter,
                    "git process failed in {}: {reason}",
                    cwd.display()
                )
            }
            Self::InvalidUtf8 { cwd, arguments } => write!(
                formatter,
                "git {arguments:?} returned non-UTF-8 output in {}",
                cwd.display()
            ),
            Self::InvalidOutput {
                cwd,
                arguments,
                output,
            } => write!(
                formatter,
                "git {arguments:?} returned unexpected output in {}: {output:?}",
                cwd.display()
            ),
            Self::ConfigEnvironment { error } => {
                write!(formatter, "Git config environment is invalid: {}", error.0)
            }
            Self::FileRead { path, reason } => {
                write!(formatter, "could not read {}: {reason}", path.display())
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
    pub workspace_id: shepr_protocol::WorkspaceId,
    pub resolved_identity_cwd: PathBuf,
    pub status_cache_key: GitStatusKey,
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
        workspace_id: shepr_protocol::WorkspaceId,
        resolved_identity_cwd: PathBuf,
        status_cache_key: GitStatusKey,
    ) -> WorkspaceGitStatus {
        let auto_label = match self.repo_root.as_deref() {
            Some(repo_root) => shepr_core::workspace_label::workspace_label_from_cwd(
                &resolved_identity_cwd,
                Some(repo_root),
                None,
            ),
            None => fallback_label_from_cwd(&resolved_identity_cwd),
        };
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
