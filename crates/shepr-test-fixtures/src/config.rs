//! Config values for tests that need a `ValidatedConfig` or `AppPaths`
//! without a launch.

use std::path::Path;

use shepr_config::{AppPaths, Config, ValidatedConfig};

/// Absolute, so a config built on these paths survives the resolved-path
/// check on the wire, and identical across calls, so two test configs compare
/// equal. An unprivileged user cannot create it: a test that writes through
/// these paths fails instead of leaving files in a shared location. Tests that
/// need real directories root their paths in a `shepr_test_support::ScratchDir`.
const UNWRITABLE_ROOT: &str = "/nonexistent/shepr-test-config";

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

pub trait ValidatedConfigFixture: Sized {
    /// The default config on [`AppPathsFixture::test_default`] paths.
    fn test_default() -> Self;

    /// `config`, validated on [`AppPathsFixture::test_default`] paths.
    /// `source` is the document the values stand for; see
    /// `ValidatedConfig::from_values`. An invalid config is a broken test and
    /// panics.
    fn test_from_config(config: Config, source: Option<&str>) -> Self;

    /// As [`Self::test_from_config`], on `paths`.
    fn test_from_config_with_paths(config: Config, source: Option<&str>, paths: AppPaths) -> Self;
}

impl ValidatedConfigFixture for ValidatedConfig {
    fn test_default() -> Self {
        Self::test_from_config(Config::default(), None)
    }

    fn test_from_config(config: Config, source: Option<&str>) -> Self {
        Self::test_from_config_with_paths(config, source, AppPaths::test_default())
    }

    fn test_from_config_with_paths(config: Config, source: Option<&str>, paths: AppPaths) -> Self {
        Self::from_values(config, source, paths).expect("test config is valid")
    }
}
