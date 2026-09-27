use std::path::PathBuf;

mod config;
#[cfg(test)]
mod config_tests;
mod discovery;
mod status;
#[cfg(test)]
pub mod test_support;

use self::discovery::automatic_workspace_label;
#[cfg(test)]
pub use self::status::git_status_snapshot_for_cwd;

pub use self::{
    discovery::{GitSpaceMetadata, derive_label_from_cwd, fallback_label_from_cwd},
    status::{
        GitStatusCacheEntry, GitStatusRefreshDemand, git_status_cache_key,
        git_status_snapshot_for_cwd_with_demand,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AheadBehind {
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatus {
    pub workspace_id: String,
    pub resolved_identity_cwd: PathBuf,
    pub status_cache_key: PathBuf,
    pub demand: GitStatusRefreshDemand,
    pub auto_label: String,
    pub branch: Option<String>,
    pub ahead_behind: Option<AheadBehind>,
    pub space: Option<GitSpaceMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatusSnapshot {
    pub auto_label: String,
    pub branch: Option<String>,
    pub ahead_behind: Option<AheadBehind>,
    pub space: Option<GitSpaceMetadata>,
}

impl WorkspaceGitStatusSnapshot {
    pub fn into_workspace_status(
        self,
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
        status_cache_key: PathBuf,
        demand: GitStatusRefreshDemand,
    ) -> WorkspaceGitStatus {
        let auto_label = self
            .space
            .as_ref()
            .map(|space| automatic_workspace_label(&resolved_identity_cwd, &space.repo_root))
            .unwrap_or_else(|| fallback_label_from_cwd(&resolved_identity_cwd));
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd,
            status_cache_key,
            demand,
            auto_label,
            branch: self.branch,
            ahead_behind: self.ahead_behind,
            space: self.space,
        }
    }
}
