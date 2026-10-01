//! Config values for tests that need a `ValidatedClientConfig`,
//! `ValidatedServerConfig` or `AppPaths` without a launch.

use std::path::Path;

use shepr_config::{
    AppPaths, ClientConfig, ServerConfig, ValidatedClientConfig, ValidatedServerConfig,
};

/// Absolute, so a config built on these paths survives the resolved-path
/// check on the wire, and identical across calls, so two test configs compare
/// equal. An unprivileged user cannot create it: a test that writes through
/// these paths fails instead of leaving files in a shared location. Tests that
/// need real directories root their paths in a `shepr_test_support::ScratchDir`.
const UNWRITABLE_ROOT: &str = "/nonexistent/shepr-test-config";

/// The pane shell fixture configs name when a test leaves it unset. Launch
/// validation only inspects it (it must exist and be executable); no fixture
/// config runs it.
const FIXTURE_SHELL: &str = "/bin/sh";

pub trait AppPathsFixture: Sized {
    /// Paths under [`UNWRITABLE_ROOT`], which is also the home directory.
    fn test_default() -> Self;

    /// Paths under `root`, with no home or current directory.
    fn test_at(root: &Path) -> Self;
}

impl AppPathsFixture for AppPaths {
    fn test_default() -> Self {
        let root = Path::new(UNWRITABLE_ROOT);
        Self::rooted_at(root, Some(root), None)
    }

    fn test_at(root: &Path) -> Self {
        Self::rooted_at(root, None, None)
    }
}

pub trait ValidatedClientConfigFixture: Sized {
    /// The default config on [`AppPathsFixture::test_default`] paths.
    fn test_default() -> Self;

    /// `config`, validated on [`AppPathsFixture::test_default`] paths.
    /// `source` is the document the values stand for; see
    /// `ValidatedClientConfig::from_values`. An invalid config is a broken test and
    /// panics.
    fn test_from_config(config: ClientConfig, source: Option<&str>) -> Self;

    /// As [`Self::test_from_config`], on `paths`.
    fn test_from_config_with_paths(
        config: ClientConfig,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Self;
}

impl ValidatedClientConfigFixture for ValidatedClientConfig {
    fn test_default() -> Self {
        Self::test_from_config(ClientConfig::default(), None)
    }

    fn test_from_config(config: ClientConfig, source: Option<&str>) -> Self {
        Self::test_from_config_with_paths(config, source, AppPaths::test_default())
    }

    fn test_from_config_with_paths(
        config: ClientConfig,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Self {
        Self::from_values(config, source, paths).expect("test config is valid")
    }
}

pub trait ValidatedServerConfigFixture: Sized {
    /// The default config on [`AppPathsFixture::test_default`] paths.
    fn test_default() -> Self;

    /// `config`, validated on [`AppPathsFixture::test_default`] paths.
    /// `source` is the document the values stand for; see
    /// `ValidatedServerConfig::from_values`. An invalid config is a broken test and
    /// panics.
    fn test_from_config(config: ServerConfig, source: Option<&str>) -> Self;

    /// As [`Self::test_from_config`], on `paths`.
    fn test_from_config_with_paths(
        config: ServerConfig,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Self;
}

impl ValidatedServerConfigFixture for ValidatedServerConfig {
    fn test_default() -> Self {
        Self::test_from_config(ServerConfig::default(), None)
    }

    fn test_from_config(config: ServerConfig, source: Option<&str>) -> Self {
        Self::test_from_config_with_paths(config, source, AppPaths::test_default())
    }

    fn test_from_config_with_paths(
        mut config: ServerConfig,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Self {
        // `from_values` reads `SHELL` and `PATH` even with an explicit shell.
        // This fixed absolute path makes both values irrelevant for default
        // fixtures; callers testing a relative shell must isolate their env.
        // Do not acquire `IsolatedEnv` here: callers may already hold its
        // non-reentrant process-environment lock while building a fixture.
        if config.terminal.default_shell.trim().is_empty() {
            config.terminal.default_shell = FIXTURE_SHELL.to_owned();
        }
        Self::from_values(config, source, paths).expect("test config is valid")
    }
}
