use std::path::{Path, PathBuf};

/// Messages produced by installing an agent integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallOutput {
    /// Installation and configuration messages, in display order.
    pub messages: Vec<String>,
    /// Warnings to display after the installation messages.
    pub warnings: Vec<InstallWarning>,
}

/// A non-fatal install warning whose severity is chosen by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallWarning(String);

impl InstallWarning {
    pub(crate) fn new(message: String) -> Self {
        Self(message)
    }
}

impl std::fmt::Display for InstallWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
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
    pub installed_version: Option<u32>,
    pub expected_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationStatusKind {
    NotInstalled,
    Current,
    Outdated,
}
