mod config;
#[cfg(test)]
mod config_tests;
mod discovery;
mod status;
#[cfg(test)]
pub(super) mod test_support;

pub(crate) use self::discovery::automatic_workspace_label;
#[cfg(test)]
pub(crate) use self::status::git_status_snapshot_for_cwd;

pub use self::{
    discovery::{GitSpaceMetadata, derive_label_from_cwd, fallback_label_from_cwd},
    status::{
        GitStatusCacheEntry, GitStatusRefreshDemand, git_status_cache_key,
        git_status_snapshot_for_cwd_with_demand,
    },
};
