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

/// The local server's name in fixture client configs that leave
/// `local.label` unset. No fixture machine uses it.
pub const FIXTURE_LOCAL_LABEL: &str = "Desk";

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
        mut config: ClientConfig,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Self {
        // Unset, the local server is named after this host, whose name varies
        // and can be long enough to change how a test's sidebar truncates. A
        // fixed name keeps fixture configs alike on every host.
        if config.local.label.is_none() {
            config.local.label = Some(
                shepr_config::MachineLabel::parse(FIXTURE_LOCAL_LABEL)
                    .expect("the fixture local label is valid"),
            );
        }
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
        // A config fixture needs a shell-shaped value, but it must not borrow
        // the host's shell just to validate. This absent path is a sentinel:
        // app test constructors replace it with a repo-built shell fixture,
        // and a test that launches directly from this config must supply one.
        let default_shell = if config
            .terminal
            .default_shell
            .as_deref()
            .is_none_or(|shell| shell.trim().is_empty())
        {
            config.terminal.default_shell = None;
            Some(
                shepr_core::shell::ResolvedShell::validate(
                    Path::new(UNWRITABLE_ROOT).join("sh"),
                    |_| Ok(()),
                )
                .expect("the fixture shell sentinel is absolute"),
            )
        } else {
            None
        };
        match default_shell {
            Some(shell) => Self::validate_for_test(&config, paths, shell),
            None => Self::validate(&config, paths),
        }
        .expect("test config is valid")
    }
}
