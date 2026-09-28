use std::io;
use std::path::PathBuf;
use std::{collections::HashMap, io::ErrorKind};

use shepr_core::env::EnvVar;
pub(crate) use shepr_core::pathutil::{expand_tilde_path, home_dir};

/// A shepr-level override for the grok hook directory that no shepr setting
/// documents and the grok CLI does not honour: a test seam, so it stays
/// outside the environment registry and is read raw (see `grok_dir`).
pub(crate) const GROK_CONFIG_DIR_TEST_SEAM: &str = "GROK_CONFIG_DIR";

#[derive(Clone, Debug)]
struct DirectoryError {
    kind: ErrorKind,
    message: String,
}

type CapturedDirectory = Result<PathBuf, DirectoryError>;

/// Agent-owned config locations resolved at the integration command boundary.
/// Install and status code receives this value and never consults the process
/// environment while it is choosing files to read or write.
#[derive(Clone, Debug)]
pub struct AgentIntegrationPaths {
    directories: HashMap<&'static str, CapturedDirectory>,
}

impl AgentIntegrationPaths {
    pub fn resolve() -> Self {
        let directories = [
            ("pi_extension", pi_extension_dir()),
            ("omp_extension", omp_extension_dir()),
            ("claude", claude_dir()),
            ("codex", codex_dir()),
            ("copilot", copilot_dir()),
            ("devin", devin_dir()),
            ("droid", droid_dir()),
            ("kimi", kimi_dir()),
            ("opencode", opencode_dir()),
            ("opencode_state", opencode_state_dir()),
            ("kilo", kilo_dir()),
            ("qodercli", qodercli_dir()),
            ("qwen", qwen_dir()),
            ("letta", letta_dir()),
            ("cursor", cursor_dir()),
            ("mastracode", mastracode_dir()),
            ("antigravity_cli", antigravity_cli_dir()),
            ("grok", grok_dir()),
        ]
        .into_iter()
        .map(|(name, result)| {
            let result = result.map_err(|error| DirectoryError {
                kind: error.kind(),
                message: error.to_string(),
            });
            (name, result)
        })
        .collect();
        Self { directories }
    }

    pub(crate) fn directory(&self, name: &'static str) -> io::Result<PathBuf> {
        match self.directories.get(name) {
            Some(Ok(path)) => Ok(path.clone()),
            Some(Err(error)) => Err(io::Error::new(error.kind, error.message.clone())),
            None => Err(io::Error::new(
                ErrorKind::NotFound,
                format!("integration directory {name} was not resolved"),
            )),
        }
    }
}

/// An XDG base directory variable under the environment policy: unset or
/// empty means the spec's default (`None`), and a relative, padded or
/// non-UTF-8 value is refused, as shepr's own launch refuses it.
pub(super) fn absolute_xdg_home(variable: EnvVar) -> io::Result<Option<PathBuf>> {
    shepr_core::env::read_path(variable).map_err(io::Error::from)
}

pub(crate) fn pi_extension_dir() -> io::Result<PathBuf> {
    Ok(
        config_dir_from_env_or_home(EnvVar::PiCodingAgentDir, &[".pi", "agent"])?
            .join("extensions"),
    )
}

pub(crate) fn omp_extension_dir() -> io::Result<PathBuf> {
    let config_dir =
        shepr_core::env::read_path(EnvVar::PiConfigDir)?.unwrap_or_else(|| ".omp".into());
    Ok(home_dir()?
        .join(config_dir)
        .join("agent")
        .join("extensions"))
}

pub(crate) fn claude_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(EnvVar::ClaudeConfigDir, &[".claude"])
}

pub(crate) fn codex_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(EnvVar::CodexHome, &[".codex"])
}

pub(crate) fn kimi_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(EnvVar::KimiCodeHome, &[".kimi-code"])
}

pub(crate) fn copilot_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(EnvVar::CopilotHome, &[".copilot"])
}

pub(crate) fn devin_dir() -> io::Result<PathBuf> {
    // Devin's config is another tool's location, under the XDG config home
    // when one is set and Devin's conventional HOME path otherwise.
    if let Some(path) = absolute_xdg_home(EnvVar::XdgConfigHome)? {
        return Ok(path.join("devin"));
    }

    Ok(home_dir()?.join(".config").join("devin"))
}

pub(crate) fn droid_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".factory"))
}

pub(crate) fn config_dir_from_env_or_home(
    env_var: EnvVar,
    home_relative_segments: &[&str],
) -> io::Result<PathBuf> {
    if let Some(value) = shepr_core::env::read_path(env_var)? {
        return expand_tilde_path(value);
    }

    let mut path = home_dir()?;
    for segment in home_relative_segments {
        path.push(segment);
    }
    Ok(path)
}

pub(crate) fn opencode_dir() -> io::Result<PathBuf> {
    if let Some(path) = absolute_xdg_home(EnvVar::XdgConfigHome)? {
        return Ok(path.join("opencode"));
    }

    Ok(home_dir()?.join(".config/opencode"))
}

pub(crate) fn opencode_state_dir() -> io::Result<PathBuf> {
    // OpenCode's state is another tool's location, under the XDG state home
    // when one is set and OpenCode's conventional HOME path otherwise.
    if let Some(path) = absolute_xdg_home(EnvVar::XdgStateHome)? {
        return Ok(path.join("opencode"));
    }

    Ok(home_dir()?.join(".local/state/opencode"))
}

pub(crate) fn kilo_dir() -> io::Result<PathBuf> {
    if let Some(path) = absolute_xdg_home(EnvVar::XdgConfigHome)? {
        return Ok(path.join("kilo"));
    }

    Ok(home_dir()?.join(".config/kilo"))
}

pub(crate) fn qodercli_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(EnvVar::QoderConfigDir, &[".qoder"])
}

pub(crate) fn qwen_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(EnvVar::QwenHome, &[".qwen"])
}

pub(crate) fn letta_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".letta"))
}

pub(crate) fn cursor_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(EnvVar::CursorConfigDir, &[".cursor"])
}

pub(crate) fn mastracode_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".mastracode"))
}

pub(crate) fn antigravity_cli_dir() -> io::Result<PathBuf> {
    // Antigravity CLI discovers global customizations (hooks.json included)
    // from ~/.gemini/config; ~/.gemini/antigravity-cli holds runtime data and
    // is never read for hooks.
    config_dir_from_env_or_home(EnvVar::AntigravityCliConfigDir, &[".gemini", "config"])
}

pub(crate) fn grok_dir() -> io::Result<PathBuf> {
    if let Some(value) = grok_config_dir_test_seam() {
        return expand_tilde_path(value);
    }
    // The grok CLI honors GROK_HOME as its config home (config.toml,
    // auth.json, hooks/); mirror it so hook installs land where grok looks.
    config_dir_from_env_or_home(EnvVar::GrokHome, &[".grok"])
}

/// `GROK_CONFIG_DIR`, the test seam that redirects grok's hook directory ahead
/// of `GROK_HOME`. Empty reads as unset.
#[expect(
    clippy::disallowed_methods,
    reason = "GROK_CONFIG_DIR is a test seam, not a shepr setting, so it stays outside the environment registry"
)]
fn grok_config_dir_test_seam() -> Option<PathBuf> {
    std::env::var_os(GROK_CONFIG_DIR_TEST_SEAM)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::IsolatedEnv;

    #[test]
    fn config_dir_env_override_expands_tilde() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::QwenHome, "~/qwen-home");
        assert_eq!(
            qwen_dir().expect("test precondition"),
            env.home().join("qwen-home")
        );
        // Empty is unset; padding is refused naming the variable.
        env.set(EnvVar::QwenHome, "");
        assert_eq!(
            qwen_dir().expect("test precondition"),
            env.home().join(".qwen")
        );
        env.set(EnvVar::QwenHome, "~/qwen-home ");
        let error = qwen_dir().expect_err("a padded override is refused");
        assert!(error.to_string().contains("QWEN_HOME"), "{error}");
    }

    #[test]
    fn opencode_state_dir_defaults_to_local_state() {
        let env = IsolatedEnv::new();
        assert_eq!(
            opencode_state_dir().expect("test precondition"),
            env.home().join(".local/state/opencode")
        );
    }

    #[test]
    fn opencode_state_dir_honors_xdg_state_home() {
        let env = IsolatedEnv::new();
        let xdg = env.path().join("state");
        env.set("XDG_STATE_HOME", &xdg);
        assert_eq!(
            opencode_state_dir().expect("test precondition"),
            xdg.join("opencode")
        );
    }

    #[test]
    fn devin_dir_ignores_empty_and_refuses_relative_xdg_config_home() {
        let env = IsolatedEnv::new();
        env.set("XDG_CONFIG_HOME", "");
        assert_eq!(
            devin_dir().expect("home fallback"),
            env.home().join(".config/devin")
        );
        env.set("XDG_CONFIG_HOME", "relative/config");
        let error = devin_dir().expect_err("a relative XDG config home is refused");
        assert!(error.to_string().contains("XDG_CONFIG_HOME"), "{error}");

        let xdg = env.path().join("config");
        env.set("XDG_CONFIG_HOME", &xdg);
        assert_eq!(devin_dir().expect("absolute XDG path"), xdg.join("devin"));
    }

    #[test]
    fn opencode_and_kilo_dirs_honor_absolute_xdg_config_home() {
        let env = IsolatedEnv::new();
        assert_eq!(
            opencode_dir().expect("home fallback"),
            env.home().join(".config/opencode")
        );
        env.set("XDG_CONFIG_HOME", "relative/config");
        assert!(kilo_dir().is_err(), "a relative XDG config home is refused");

        let xdg = env.path().join("config");
        env.set("XDG_CONFIG_HOME", &xdg);
        assert_eq!(
            opencode_dir().expect("absolute XDG path"),
            xdg.join("opencode")
        );
        assert_eq!(kilo_dir().expect("absolute XDG path"), xdg.join("kilo"));
    }

    #[test]
    fn opencode_state_dir_ignores_empty_and_refuses_relative_xdg_state_home() {
        let env = IsolatedEnv::new();
        env.set("XDG_STATE_HOME", "");
        assert_eq!(
            opencode_state_dir().expect("home fallback"),
            env.home().join(".local/state/opencode")
        );
        env.set("XDG_STATE_HOME", "relative/state");
        assert!(
            opencode_state_dir().is_err(),
            "a relative XDG state home is refused"
        );
    }
}
