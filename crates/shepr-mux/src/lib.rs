// Shared pane, workspace, and persistence model.
pub mod events;
pub mod git;
pub mod pane;
pub mod persist;
pub mod render_signal;
pub mod terminal;
pub mod workspace;

#[cfg(test)]
mod test_support {
    pub(crate) use shepr_test_support::ScratchDir;
}
