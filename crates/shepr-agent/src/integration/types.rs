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
        let error_kind = match kind {
            InstallErrorKind::ConfigChanged => io::ErrorKind::WouldBlock,
            InstallErrorKind::ConfigUnparseable
            | InstallErrorKind::ConfigShape
            | InstallErrorKind::ManagedBlockConflict
            | InstallErrorKind::HardLinked
            | InstallErrorKind::NotRegularFile
            | InstallErrorKind::TooManySymlinks => io::ErrorKind::InvalidData,
            InstallErrorKind::AgentDirMissing => io::ErrorKind::NotFound,
            InstallErrorKind::Io => io::ErrorKind::Other,
        };
        io::Error::new(
            error_kind,
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
pub(crate) struct InstallError {
    kind: InstallErrorKind,
    source: io::Error,
}

impl InstallError {
    pub(crate) fn kind(&self) -> InstallErrorKind {
        self.kind
    }

    fn source(&self) -> &io::Error {
        &self.source
    }
}

impl From<io::Error> for InstallError {
    fn from(source: io::Error) -> Self {
        if super::config_file::is_config_changed(&source) {
            return Self {
                kind: InstallErrorKind::ConfigChanged,
                source,
            };
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

        Self {
            kind: kind.unwrap_or(InstallErrorKind::Io),
            source,
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
            let raw = InstallIssue::io_error(kind, "settings failure");
            let expected_kind = match kind {
                InstallErrorKind::ConfigChanged => io::ErrorKind::WouldBlock,
                InstallErrorKind::ConfigUnparseable
                | InstallErrorKind::ConfigShape
                | InstallErrorKind::ManagedBlockConflict
                | InstallErrorKind::HardLinked
                | InstallErrorKind::NotRegularFile
                | InstallErrorKind::TooManySymlinks => io::ErrorKind::InvalidData,
                InstallErrorKind::AgentDirMissing => io::ErrorKind::NotFound,
                InstallErrorKind::Io => io::ErrorKind::Other,
            };
            assert_eq!(raw.kind(), expected_kind);
            let error = InstallError::from(raw);
            assert_eq!(error.kind(), kind);
            assert_eq!(error.source().kind(), expected_kind);
        }
        let error = InstallError::from(InstallIssue::io_error(
            InstallErrorKind::ConfigShape,
            "settings must be an object",
        ));
        assert_eq!(error.to_string(), "settings must be an object");
    }
}
