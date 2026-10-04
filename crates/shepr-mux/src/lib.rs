// Shared pane, workspace, and persistence model.
mod cwd;
pub mod events;
pub use cwd::UsableCwd;
/// The Git vocabulary a workspace's identity carries. `shepr-git` owns it,
/// along with the refresh that produces it.
pub mod git {
    pub use shepr_git::{AheadBehind, GitBranch, GitStatus, GitStatusKey, RefreshedStatus};

    /// A Git status the refresh answered for one workspace.
    pub type WorkspaceGitStatus = RefreshedStatus<shepr_protocol::WorkspaceId>;
}
pub mod pane;
pub mod persist;
pub mod render_signal;
pub mod terminal;
pub mod workspace;

#[cfg(test)]
mod test_support {
    pub(crate) use shepr_test_support::ScratchDir;
}
