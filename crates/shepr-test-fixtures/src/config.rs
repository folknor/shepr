//! Config values for tests that need a `ValidatedClientConfig`,
//! `ValidatedServerConfig` or `AppPaths` without a launch.

use std::path::Path;

use shepr_config::{ClientConfig, ServerConfig, ValidatedClientConfig, ValidatedServerConfig};
use shepr_paths::AppPaths;

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
        Self::rooted_at(root, Some(root), None).expect("the unwritable root is short")
    }

    fn test_at(root: &Path) -> Self {
        Self::rooted_at(root, None, None).expect("test roots fit the server socket")
    }
}

pub trait ValidatedClientConfigFixture: Sized {
    /// The default config on [`AppPathsFixture::test_default`] paths.
    fn test_default() -> Self;

    /// `config`, validated on [`AppPathsFixture::test_default`] paths.
    /// `source` is the document the values stand for; see
    /// `ValidatedClientConfig::validate`. An invalid config is a broken test and
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
        Self::validate(&config, source, paths).expect("test config is valid")
    }
}

pub trait ValidatedServerConfigFixture: Sized {
    /// The default config on [`AppPathsFixture::test_default`] paths.
    fn test_default() -> Self;

    /// `config`, validated on [`AppPathsFixture::test_default`] paths. An
    /// invalid config is a broken test and panics.
    fn test_from_config(config: ServerConfig) -> Self;

    /// As [`Self::test_from_config`], on `paths`.
    fn test_from_config_with_paths(config: ServerConfig, paths: AppPaths) -> Self;
}

impl ValidatedServerConfigFixture for ValidatedServerConfig {
    fn test_default() -> Self {
        Self::test_from_config(ServerConfig::default())
    }

    fn test_from_config(config: ServerConfig) -> Self {
        Self::test_from_config_with_paths(config, AppPaths::test_default())
    }

    fn test_from_config_with_paths(mut config: ServerConfig, paths: AppPaths) -> Self {
        // `validate` reads `SHELL` and `PATH` even with an explicit shell.
        // This fixed absolute path makes both values irrelevant for default
        // fixtures; callers testing a relative shell must isolate their env.
        // Do not acquire `IsolatedEnv` here: callers may already hold its
        // non-reentrant process-environment lock while building a fixture.
        if config
            .terminal
            .default_shell
            .as_deref()
            .is_none_or(|shell| shell.trim().is_empty())
        {
            config.terminal.default_shell = Some(FIXTURE_SHELL.to_owned());
        }
        Self::validate(&config, paths).expect("test config is valid")
    }
}
