// The test-api feature exposes fixtures to root tests while this crate is built without cfg(test).
#![cfg_attr(feature = "test-api", allow(dead_code))]
pub mod app;
mod events;
mod git;
pub mod pane;
mod persist;
mod render_signal;
pub mod server;
mod terminal;
mod ui;
mod workspace;

pub(crate) const SHEPR_ENV_VAR: &str = "SHEPR_ENV";
pub(crate) const SHEPR_ENV_VALUE: &str = "1";

#[cfg(any(test, feature = "test-api"))]
mod test_support {
    #[cfg(test)]
    pub(crate) use shepr_test_support::IsolatedEnv;
    pub(crate) use shepr_test_support::ScratchDir;
}
