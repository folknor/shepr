// The test-api feature exposes fixtures to root tests while this crate is built without cfg(test).
#![cfg_attr(feature = "test-api", allow(dead_code))]
pub mod app;
pub mod server;
mod ui;

#[cfg(any(test, feature = "test-api"))]
mod test_support {
    #[cfg(test)]
    pub(crate) use shepr_test_support::IsolatedEnv;
    pub(crate) use shepr_test_support::ScratchDir;
}
