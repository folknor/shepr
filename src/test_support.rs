//! Shared scratch and environment isolation for tests.
pub(crate) use shepr_test_support::{IsolatedEnv, ScratchDir};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_env_points_home_at_scratch_and_restores_on_drop() {
        const PROBE: &str = "SHEPR_TEST_SUPPORT_PROBE";
        let env = IsolatedEnv::new();
        assert_eq!(std::env::var_os("HOME"), Some(env.home().into_os_string()));
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_none());
        let paths = crate::config::AppPaths::resolve().expect("isolated directories resolve");
        assert!(paths.config_dir().starts_with(env.path()));
        assert!(paths.state_dir().starts_with(env.path()));
        assert!(paths.runtime_dir().starts_with(env.path()));
        env.set(PROBE, "set");
        let scratch = env.path().to_path_buf();
        drop(env);

        assert!(!scratch.exists());
        // No test sets this variable other than through a guard, so it is
        // gone once the guard has restored the snapshot.
        assert!(std::env::var_os(PROBE).is_none());
    }
}
