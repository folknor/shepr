use std::io;
use std::path::PathBuf;
use std::{collections::HashMap, io::ErrorKind};

pub(crate) use shepr_core::pathutil::{expand_tilde_path, home_dir};

pub(crate) const PI_CODING_AGENT_DIR_ENV_VAR: &str = "PI_CODING_AGENT_DIR";
pub(crate) const OMP_CONFIG_DIR_ENV_VAR: &str = "PI_CONFIG_DIR";
pub(crate) const CLAUDE_CONFIG_DIR_ENV_VAR: &str = "CLAUDE_CONFIG_DIR";
pub(crate) const CODEX_HOME_ENV_VAR: &str = "CODEX_HOME";
pub(crate) const KIMI_CODE_HOME_ENV_VAR: &str = "KIMI_CODE_HOME";
pub(crate) const COPILOT_HOME_ENV_VAR: &str = "COPILOT_HOME";
pub(crate) const QODERCLI_CONFIG_DIR_ENV_VAR: &str = "QODER_CONFIG_DIR";
pub(crate) const QWEN_HOME_ENV_VAR: &str = "QWEN_HOME";
pub(crate) const CURSOR_CONFIG_DIR_ENV_VAR: &str = "CURSOR_CONFIG_DIR";
pub(crate) const ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR: &str = "ANTIGRAVITY_CLI_CONFIG_DIR";
pub(crate) const GROK_CONFIG_DIR_ENV_VAR: &str = "GROK_CONFIG_DIR";
/// The grok CLI's own config-home override (documented alongside
/// `$GROK_HOME/config.toml` and `$GROK_HOME/auth.json`).
pub(crate) const GROK_HOME_ENV_VAR: &str = "GROK_HOME";
pub(crate) const HERMES_HOME_ENV_VAR: &str = "HERMES_HOME";

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
            ("hermes", hermes_dir()),
            ("hermes_plugin", hermes_plugin_dir()),
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

fn absolute_xdg_home(variable: &str) -> Option<PathBuf> {
    let path = std::env::var_os(variable).map(PathBuf::from)?;
    path.is_absolute().then_some(path)
}

pub(crate) fn pi_extension_dir() -> io::Result<PathBuf> {
    Ok(
        config_dir_from_env_or_home(PI_CODING_AGENT_DIR_ENV_VAR, &[".pi", "agent"])?
            .join("extensions"),
    )
}

pub(crate) fn omp_extension_dir() -> io::Result<PathBuf> {
    if let Some(value) =
        std::env::var_os(PI_CODING_AGENT_DIR_ENV_VAR).filter(|value| !value.is_empty())
    {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("extensions"));
    }

    let config_dir = std::env::var_os(OMP_CONFIG_DIR_ENV_VAR)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".omp".into());
    Ok(home_dir()?
        .join(config_dir)
        .join("agent")
        .join("extensions"))
}

pub(crate) fn claude_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CLAUDE_CONFIG_DIR_ENV_VAR, &[".claude"])
}

pub(crate) fn codex_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CODEX_HOME_ENV_VAR, &[".codex"])
}

pub(crate) fn kimi_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(KIMI_CODE_HOME_ENV_VAR, &[".kimi-code"])
}

pub(crate) fn copilot_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(COPILOT_HOME_ENV_VAR, &[".copilot"])
}

pub(crate) fn devin_dir() -> io::Result<PathBuf> {
    // Devin's config is another tool's location. Ignore invalid XDG values
    // per the base-directory spec and use Devin's conventional HOME path.
    if let Some(path) = absolute_xdg_home("XDG_CONFIG_HOME") {
        return Ok(path.join("devin"));
    }

    Ok(home_dir()?.join(".config").join("devin"))
}

pub(crate) fn droid_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".factory"))
}

pub(crate) fn config_dir_from_env_or_home(
    env_var: &str,
    home_relative_segments: &[&str],
) -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value));
    }

    let mut path = home_dir()?;
    for segment in home_relative_segments {
        path.push(segment);
    }
    Ok(path)
}

pub(crate) fn opencode_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".config/opencode"))
}

pub(crate) fn opencode_state_dir() -> io::Result<PathBuf> {
    // OpenCode's state is another tool's location. Ignore invalid XDG values
    // per the base-directory spec and use OpenCode's conventional HOME path.
    if let Some(path) = absolute_xdg_home("XDG_STATE_HOME") {
        return Ok(path.join("opencode"));
    }

    Ok(home_dir()?.join(".local/state/opencode"))
}

pub(crate) fn kilo_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".config/kilo"))
}

pub(crate) fn hermes_dir() -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os(HERMES_HOME_ENV_VAR).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value));
    }

    Ok(home_dir()?.join(".hermes"))
}

pub(crate) fn hermes_plugin_dir() -> io::Result<PathBuf> {
    Ok(hermes_dir()?
        .join("plugins")
        .join(super::HERMES_PLUGIN_INSTALL_NAME))
}

pub(crate) fn qodercli_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(QODERCLI_CONFIG_DIR_ENV_VAR, &[".qoder"])
}

pub(crate) fn qwen_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(QWEN_HOME_ENV_VAR, &[".qwen"])
}

pub(crate) fn letta_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".letta"))
}

pub(crate) fn cursor_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CURSOR_CONFIG_DIR_ENV_VAR, &[".cursor"])
}

pub(crate) fn mastracode_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".mastracode"))
}

pub(crate) fn antigravity_cli_dir() -> io::Result<PathBuf> {
    // Antigravity CLI discovers global customizations (hooks.json included)
    // from ~/.gemini/config; ~/.gemini/antigravity-cli holds runtime data and
    // is never read for hooks.
    config_dir_from_env_or_home(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &[".gemini", "config"])
}

pub(crate) fn grok_dir() -> io::Result<PathBuf> {
    // GROK_CONFIG_DIR is a shepr-level override only (primarily a test
    // seam); the grok CLI does not honor it, so it stays first and explicit.
    if let Some(value) = std::env::var_os(GROK_CONFIG_DIR_ENV_VAR).filter(|value| !value.is_empty())
    {
        return expand_tilde_path(PathBuf::from(value));
    }
    // The grok CLI honors GROK_HOME as its config home (config.toml,
    // auth.json, hooks/); mirror it so hook installs land where grok looks.
    config_dir_from_env_or_home(GROK_HOME_ENV_VAR, &[".grok"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::IsolatedEnv;

    #[test]
    fn config_dir_env_override_expands_tilde() {
        let env = IsolatedEnv::new();
        env.set(QWEN_HOME_ENV_VAR, "~/qwen-home");
        assert_eq!(
            qwen_dir().expect("test precondition"),
            env.home().join("qwen-home")
        );
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
    fn devin_dir_ignores_empty_and_relative_xdg_config_home() {
        let env = IsolatedEnv::new();
        for invalid in ["", "relative/config"] {
            env.set("XDG_CONFIG_HOME", invalid);
            assert_eq!(
                devin_dir().expect("home fallback"),
                env.home().join(".config/devin")
            );
        }

        let xdg = env.path().join("config");
        env.set("XDG_CONFIG_HOME", &xdg);
        assert_eq!(devin_dir().expect("absolute XDG path"), xdg.join("devin"));
    }

    #[test]
    fn opencode_state_dir_ignores_empty_and_relative_xdg_state_home() {
        let env = IsolatedEnv::new();
        for invalid in ["", "relative/state"] {
            env.set("XDG_STATE_HOME", invalid);
            assert_eq!(
                opencode_state_dir().expect("home fallback"),
                env.home().join(".local/state/opencode")
            );
        }
    }
}
