//! Git status as one subsystem: checkout discovery, the command runner with
//! its environment and deadline policy, config dependency tracking, the
//! status computation, the refresh algorithm and the cache it reads and
//! commits to, and the long-lived worker thread that owns that cache.
//!
//! Callers hand the worker targets (a cwd, the checkout key they last
//! admitted for it, and an owner value of their own) and get back one status
//! per target with the read errors the refresh saw first. Nothing here knows
//! what an owner is or how often a refresh is due: associating results with
//! their owners and scheduling refreshes stay with the caller.

use std::path::PathBuf;

mod config;
mod discovery;
mod identity;
mod limits;
mod refresh;
mod runner;
mod status;
mod worker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefBackend {
    Files,
    Reftable,
}

pub use self::{
    discovery::{discover_checkout_root, fallback_label_from_cwd},
    refresh::{GitRefresher, RefreshOutcome, RefreshTarget, RefreshedStatus},
    worker::GitStatusWorker,
};
pub use runner::{GitCommandError, run_git};

/// Discovery distinguishes a checkout from a cwd for which no repository was found.
/// A caller that has admitted no status for a cwd has no key for it, and the
/// refresh discovers one.
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

/// A Git command or repository read failed while deriving a cwd's Git status.
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
pub enum GitBranch {
    OutsideRepository,
    Detached,
    Named(String),
    ReadFailed,
}

impl GitBranch {
    /// The branch text drawn by consumers that do not display read failures.
    pub fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name),
            Self::OutsideRepository | Self::Detached | Self::ReadFailed => None,
        }
    }
}

/// One cwd's Git answer: the key of the checkout it was read under, the
/// automatic label derived from the cwd and that checkout, the HEAD outcome
/// and the ahead/behind counts against the upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatus {
    pub cwd: PathBuf,
    pub key: GitStatusKey,
    pub label: String,
    pub branch: GitBranch,
    pub ahead_behind: Option<AheadBehind>,
}

/// A checkout's status before it is bound to one cwd: every cwd in the same
/// checkout shares it, and each derives its own label from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusSnapshot {
    pub repo_root: Option<PathBuf>,
    pub branch: GitBranch,
    pub ahead_behind: Option<AheadBehind>,
}

impl GitStatusSnapshot {
    pub fn into_status(self, cwd: PathBuf, key: GitStatusKey) -> GitStatus {
        let label = match self.repo_root.as_deref() {
            Some(repo_root) => {
                shepr_core::workspace_label::workspace_label_from_cwd(&cwd, Some(repo_root), None)
            }
            None => fallback_label_from_cwd(&cwd),
        };
        GitStatus {
            cwd,
            key,
            label,
            branch: self.branch,
            ahead_behind: self.ahead_behind,
        }
    }
}

#[cfg(test)]
mod config_tests;

#[cfg(test)]
mod test_support;
