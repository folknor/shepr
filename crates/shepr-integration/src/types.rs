use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallErrorKind {
    ConfigChanged,
    ConfigUnparseable,
    ConfigShape,
    ManagedBlockConflict,
    HardLinked,
    NotRegularFile,
    TooManySymlinks,
    AgentDirMissing,
    Io,
}

/// Installer failures stay typed until the tracing boundary.
#[derive(Debug)]
pub(crate) enum InstallError {
    ConfigUnparseable(String),
    ConfigShape(String),
    ManagedBlockConflict(String),
    HardLinked(String),
    NotRegularFile(shepr_platform::NotRegularFile),
    TooManySymlinks(String),
    AgentDirMissing(String),
    ConfigChanged(PathBuf),
    Io(io::Error),
    Shared(std::sync::Arc<Self>),
}

pub(crate) type InstallResult<T> = Result<T, InstallError>;

impl InstallError {
    pub(crate) fn config_unparseable(message: impl Into<String>) -> Self {
        Self::ConfigUnparseable(message.into())
    }

    pub(crate) fn config_shape(message: impl Into<String>) -> Self {
        Self::ConfigShape(message.into())
    }

    pub(crate) fn managed_block_conflict(message: impl Into<String>) -> Self {
        Self::ManagedBlockConflict(message.into())
    }

    pub(crate) fn hard_linked(message: impl Into<String>) -> Self {
        Self::HardLinked(message.into())
    }

    pub(crate) fn too_many_symlinks(message: impl Into<String>) -> Self {
        Self::TooManySymlinks(message.into())
    }

    pub(crate) fn agent_dir_missing(message: impl Into<String>) -> Self {
        Self::AgentDirMissing(message.into())
    }

    pub(crate) fn kind(&self) -> InstallErrorKind {
        match self {
            Self::ConfigUnparseable(_) => InstallErrorKind::ConfigUnparseable,
            Self::ConfigShape(_) => InstallErrorKind::ConfigShape,
            Self::ManagedBlockConflict(_) => InstallErrorKind::ManagedBlockConflict,
            Self::HardLinked(_) => InstallErrorKind::HardLinked,
            Self::NotRegularFile(_) => InstallErrorKind::NotRegularFile,
            Self::TooManySymlinks(_) => InstallErrorKind::TooManySymlinks,
            Self::AgentDirMissing(_) => InstallErrorKind::AgentDirMissing,
            Self::ConfigChanged(_) => InstallErrorKind::ConfigChanged,
            Self::Io(_) => InstallErrorKind::Io,
            Self::Shared(error) => error.kind(),
        }
    }

    pub(crate) fn io_kind(&self) -> io::ErrorKind {
        match self {
            Self::Io(error) => error.kind(),
            Self::Shared(error) => error.io_kind(),
            _ => match self.kind() {
                InstallErrorKind::ConfigChanged => io::ErrorKind::WouldBlock,
                InstallErrorKind::AgentDirMissing => io::ErrorKind::NotFound,
                _ => io::ErrorKind::InvalidData,
            },
        }
    }
}

impl From<io::Error> for InstallError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<shepr_platform::NotRegularFile> for InstallError {
    fn from(error: shepr_platform::NotRegularFile) -> Self {
        Self::NotRegularFile(error)
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigUnparseable(message)
            | Self::ConfigShape(message)
            | Self::ManagedBlockConflict(message)
            | Self::HardLinked(message)
            | Self::TooManySymlinks(message)
            | Self::AgentDirMissing(message) => formatter.write_str(message),
            Self::ConfigChanged(path) => write!(
                formatter,
                "{} changed while Shepr was preparing an update",
                path.display()
            ),
            Self::NotRegularFile(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::Shared(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotRegularFile(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Shared(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationOutdatedReason {
    Asset,
    Registration,
    AssetAndRegistration,
}

/// What a file an integration writes is to the operator. The role picks the
/// structured install log field; the file's own format is the target's
/// business.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactRole {
    /// The shepr hook script the agent runs.
    Hook,
    /// A file the agent loads from its extension directory (pi, omp).
    Extension,
    /// Agent settings holding shepr's hook entries.
    Settings,
    /// A hooks file shepr ensures its entries in.
    Hooks,
    /// A hooks file shepr rewrites on install (cursor).
    UpdatedHooks,
    /// Agent config shepr ensures its entries in.
    Config,
    /// A hook config file that is shepr's own (grok).
    HookConfig,
    /// A plugin file the agent loads from its plugin directory.
    Plugin,
    /// The opencode TUI plugin file.
    TuiPlugin,
    /// An opencode TUI config listing the TUI plugin.
    TuiConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct InstallArtifact {
    pub role: ArtifactRole,
    pub path: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct InstallOutcome {
    pub artifacts: Vec<InstallArtifact>,
    pub notices: Vec<String>,
}

impl InstallOutcome {
    pub(crate) fn with_artifact(mut self, role: ArtifactRole, path: PathBuf) -> Self {
        self.artifacts.push(InstallArtifact { role, path });
        self
    }

    pub(crate) fn with_notice(mut self, notice: String) -> Self {
        self.notices.push(notice);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntegrationStatus {
    pub target: shepr_agent::IntegrationTarget,
    pub path: PathBuf,
    pub state: IntegrationStatusKind,
    /// Which managed part needs repair when `state` is `Outdated`.
    pub outdated_reason: Option<IntegrationOutdatedReason>,
    /// Version marker from the installed asset, for diagnostics only.
    pub installed_version: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationStatusKind {
    NotInstalled,
    Current,
    Outdated,
}

#[cfg(test)]
impl InstallError {
    /// The OS error number of an I/O failure, looking through shared errors.
    pub(crate) fn raw_os_error(&self) -> Option<i32> {
        match self {
            Self::Io(error) => error.raw_os_error(),
            Self::Shared(error) => error.raw_os_error(),
            _ => None,
        }
    }
}
