use std::path::{Path, PathBuf};

/// Messages produced by installing an agent integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutput {
    /// Installation and configuration messages, in display order.
    pub messages: Vec<String>,
    /// Warnings to display after the installation messages.
    pub warnings: Vec<InstallWarning>,
}

/// A non-fatal install warning whose severity is chosen by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallWarning(String);

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
/// wording of the install and uninstall messages; the file's own format is
/// the target's business.
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

    /// The uninstall message, or `None` for a state this role never reports.
    pub(crate) fn uninstall_message(
        self,
        label: &str,
        path: &Path,
        state: UninstallState,
    ) -> Option<String> {
        use UninstallState::{Missing, Preserved, Removed, Unchanged, Updated};
        let path = path.display();
        let message = match (self, state) {
            (Self::Hook, Removed) => format!("removed {label} hook at {path}"),
            (Self::Hook, Missing) => format!("no {label} hook found at {path}"),
            (Self::Extension, Removed) => {
                format!("removed {label} integration extension at {path}")
            }
            (Self::Extension, Missing) => {
                format!("no {label} integration extension found at {path}")
            }
            (Self::Plugin, Removed) => format!("removed {label} integration plugin at {path}"),
            (Self::Plugin, Missing) => format!("no {label} integration plugin found at {path}"),
            (Self::TuiPlugin, Removed) => {
                format!("removed {label} tui integration plugin at {path}")
            }
            (Self::TuiPlugin, Missing) => {
                format!("no {label} tui integration plugin found at {path}")
            }
            (Self::HookConfig, Removed) => format!("removed {label} hook config at {path}"),
            (Self::HookConfig, Missing) => format!("no {label} hook config found at {path}"),
            (Self::TuiConfig, Updated) => format!("removed shepr {label} plugin entry from {path}"),
            (Self::Settings | Self::Hooks | Self::UpdatedHooks | Self::Config, Updated) => {
                format!("removed shepr {label} hook entries from {path}")
            }
            (Self::Settings | Self::Hooks | Self::UpdatedHooks | Self::Config, Unchanged) => {
                format!("no shepr {label} hook entries found in {path}")
            }
            (Self::Config, Preserved) => format!("left {label} config unchanged at {path}"),
            _ => return None,
        };
        Some(message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UninstallState {
    Removed,
    Missing,
    Updated,
    Unchanged,
    Preserved,
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

#[derive(Debug, Clone)]
pub(crate) struct UninstallArtifact {
    pub role: ArtifactRole,
    pub path: PathBuf,
    pub state: UninstallState,
}

#[derive(Debug, Default)]
pub(crate) struct UninstallOutcome {
    pub artifacts: Vec<UninstallArtifact>,
}

impl UninstallOutcome {
    pub(crate) fn record(&mut self, role: ArtifactRole, path: PathBuf, state: UninstallState) {
        self.artifacts.push(UninstallArtifact { role, path, state });
    }

    pub(crate) fn record_removal(&mut self, role: ArtifactRole, path: PathBuf, removed: bool) {
        let state = if removed {
            UninstallState::Removed
        } else {
            UninstallState::Missing
        };
        self.record(role, path, state);
    }

    pub(crate) fn record_update(&mut self, role: ArtifactRole, path: PathBuf, updated: bool) {
        let state = if updated {
            UninstallState::Updated
        } else {
            UninstallState::Unchanged
        };
        self.record(role, path, state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationStatus {
    pub target: crate::agent::IntegrationTarget,
    pub path: PathBuf,
    pub state: IntegrationStatusKind,
    pub installed_version: Option<u32>,
    pub expected_version: u32,
}

/// A supported target whose status could not be checked: its directory did
/// not resolve (for example, no usable home directory), or a stat on its
/// installed file failed with something other than absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationStatusError {
    pub target: crate::agent::IntegrationTarget,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationStatusKind {
    NotInstalled,
    Current,
    Outdated,
}
