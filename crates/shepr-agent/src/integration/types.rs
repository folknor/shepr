use std::io;
use std::path::{Path, PathBuf};

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

#[derive(Debug)]
pub(crate) struct InstallIssue {
    kind: InstallErrorKind,
    message: String,
}

impl InstallIssue {
    pub(crate) fn io_error(kind: InstallErrorKind, message: impl Into<String>) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            Self {
                kind,
                message: message.into(),
            },
        )
    }
}

impl std::fmt::Display for InstallIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InstallIssue {}

#[derive(Debug)]
pub(crate) enum InstallError {
    ConfigChanged { source: io::Error },
    ConfigUnparseable { source: io::Error },
    ConfigShape { source: io::Error },
    ManagedBlockConflict { source: io::Error },
    HardLinked { source: io::Error },
    NotRegularFile { source: io::Error },
    TooManySymlinks { source: io::Error },
    AgentDirMissing { source: io::Error },
    Io { source: io::Error },
}

impl InstallError {
    pub(crate) fn kind(&self) -> InstallErrorKind {
        match self {
            Self::ConfigChanged { .. } => InstallErrorKind::ConfigChanged,
            Self::ConfigUnparseable { .. } => InstallErrorKind::ConfigUnparseable,
            Self::ConfigShape { .. } => InstallErrorKind::ConfigShape,
            Self::ManagedBlockConflict { .. } => InstallErrorKind::ManagedBlockConflict,
            Self::HardLinked { .. } => InstallErrorKind::HardLinked,
            Self::NotRegularFile { .. } => InstallErrorKind::NotRegularFile,
            Self::TooManySymlinks { .. } => InstallErrorKind::TooManySymlinks,
            Self::AgentDirMissing { .. } => InstallErrorKind::AgentDirMissing,
            Self::Io { .. } => InstallErrorKind::Io,
        }
    }

    fn source(&self) -> &io::Error {
        match self {
            Self::ConfigChanged { source }
            | Self::ConfigUnparseable { source }
            | Self::ConfigShape { source }
            | Self::ManagedBlockConflict { source }
            | Self::HardLinked { source }
            | Self::NotRegularFile { source }
            | Self::TooManySymlinks { source }
            | Self::AgentDirMissing { source }
            | Self::Io { source } => source,
        }
    }
}

impl From<io::Error> for InstallError {
    fn from(source: io::Error) -> Self {
        if super::config_file::is_config_changed(&source) {
            return Self::ConfigChanged { source };
        }

        let kind = source
            .get_ref()
            .and_then(|cause| cause.downcast_ref::<InstallIssue>())
            .map(|issue| issue.kind)
            .or_else(|| {
                source
                    .get_ref()
                    .and_then(|cause| cause.downcast_ref::<super::file_ops::NotRegularFile>())
                    .map(|_| InstallErrorKind::NotRegularFile)
            });

        match kind {
            Some(InstallErrorKind::ConfigUnparseable) => Self::ConfigUnparseable { source },
            Some(InstallErrorKind::ConfigShape) => Self::ConfigShape { source },
            Some(InstallErrorKind::ManagedBlockConflict) => Self::ManagedBlockConflict { source },
            Some(InstallErrorKind::HardLinked) => Self::HardLinked { source },
            Some(InstallErrorKind::NotRegularFile) => Self::NotRegularFile { source },
            Some(InstallErrorKind::TooManySymlinks) => Self::TooManySymlinks { source },
            Some(InstallErrorKind::AgentDirMissing) => Self::AgentDirMissing { source },
            Some(InstallErrorKind::ConfigChanged) => Self::ConfigChanged { source },
            Some(InstallErrorKind::Io) | None => Self::Io { source },
        }
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source().fmt(formatter)
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationOutdatedReason {
    Asset,
    Registration,
    AssetAndRegistration,
}

/// Messages produced by installing an agent integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallOutput {
    /// Installation and configuration messages, in display order.
    pub messages: Vec<String>,
}

/// What a file an integration writes is to the operator. The role picks the
/// wording of the install message; the file's own format is the target's
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

impl ArtifactRole {
    pub(crate) fn install_message(self, label: &str, path: &Path) -> String {
        let path = path.display();
        match self {
            Self::Hook => format!("installed {label} integration hook to {path}"),
            Self::Extension => format!("installed {label} integration to {path}"),
            Self::Settings => format!("ensured {label} settings at {path}"),
            Self::Hooks => format!("ensured {label} hooks at {path}"),
            Self::UpdatedHooks => format!("updated {label} hooks at {path}"),
            Self::Config => format!("ensured {label} config at {path}"),
            Self::HookConfig => format!("registered {label} hook config at {path}"),
            Self::Plugin => format!("installed {label} integration plugin to {path}"),
            Self::TuiPlugin => format!("installed {label} tui integration plugin to {path}"),
            Self::TuiConfig => format!("ensured {label} tui plugin config at {path}"),
        }
    }
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
    pub target: crate::agent::IntegrationTarget,
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
mod tests {
    use super::*;

    #[test]
    fn install_failures_keep_their_category_at_the_log_boundary() {
        for kind in [
            InstallErrorKind::ConfigChanged,
            InstallErrorKind::ConfigUnparseable,
            InstallErrorKind::ConfigShape,
            InstallErrorKind::ManagedBlockConflict,
            InstallErrorKind::HardLinked,
            InstallErrorKind::NotRegularFile,
            InstallErrorKind::TooManySymlinks,
            InstallErrorKind::AgentDirMissing,
            InstallErrorKind::Io,
        ] {
            let error = InstallError::from(InstallIssue::io_error(kind, "settings failure"));
            assert_eq!(error.kind(), kind);
        }
        let error = InstallError::from(InstallIssue::io_error(
            InstallErrorKind::ConfigShape,
            "settings must be an object",
        ));
        assert_eq!(error.to_string(), "settings must be an object");
    }
}
