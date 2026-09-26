use std::io;
use std::path::PathBuf;

pub(crate) use crate::pathutil::{expand_tilde_path, home_dir};
use crate::pty::PtyCommand;

pub(crate) const SHEPR_PANE_ID_ENV_VAR: &str = "SHEPR_PANE_ID";
pub(crate) const SHEPR_TAB_ID_ENV_VAR: &str = "SHEPR_TAB_ID";
pub(crate) const SHEPR_WORKSPACE_ID_ENV_VAR: &str = "SHEPR_WORKSPACE_ID";

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

pub(crate) fn apply_pane_base_env(cmd: &mut PtyCommand) {
    cmd.env(crate::api::SOCKET_PATH_ENV_VAR, crate::api::socket_path());
    if let Ok(executable) = crate::platform::launch_executable() {
        cmd.env("SHEPR_BIN_PATH", executable);
    }
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
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("devin"));
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
    if let Some(value) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("opencode"));
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
    use crate::test_support::IsolatedEnv;

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
}
