//! Shared scratch and environment isolation for tests.
pub(crate) use shepr_test_support::{IsolatedEnv, ScratchDir};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "checks the guard restored the environment after it dropped, which no live guard can witness"
    )]
    fn isolated_env_points_home_at_scratch_and_restores_on_drop() {
        use shepr_core::env::EnvVar;
        const PROBE: &str = "SHEPR_TEST_SUPPORT_PROBE";
        let env = IsolatedEnv::new();
        assert_eq!(env.get(EnvVar::Home), Some(env.home().into_os_string()));
        assert!(env.get(EnvVar::XdgConfigHome).is_none());
        let paths = shepr_paths::AppPaths::resolve_with_config()
            .expect("isolated config and directories resolve");
        assert!(
            paths
                .config_dir()
                .expect("config paths were resolved")
                .starts_with(env.path())
        );
        assert!(paths.state_dir().starts_with(env.path()));
        assert!(paths.runtime_dir().starts_with(env.path()));
        env.set(PROBE, "set");
        drop(env);

        // No test sets this variable other than through a guard, so it is
        // gone once the guard has restored the snapshot.
        assert!(std::env::var_os(PROBE).is_none());
    }
}
