use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::{collections::HashMap, io::ErrorKind};

use shepr_core::env::EnvVar;

#[derive(Clone, Debug)]
struct DirectoryError {
    kind: ErrorKind,
    message: String,
}

type CapturedDirectory = Result<PathBuf, DirectoryError>;
type CapturedEnvPath = Result<Option<PathBuf>, DirectoryError>;

const INTEGRATION_PATH_ENV_VARS: &[EnvVar] = &[
    EnvVar::Home,
    EnvVar::XdgConfigHome,
    EnvVar::XdgStateHome,
    EnvVar::PiCodingAgentDir,
    EnvVar::PiConfigDir,
    EnvVar::ClaudeConfigDir,
    EnvVar::CodexHome,
    EnvVar::KimiCodeHome,
    EnvVar::CopilotHome,
    EnvVar::CursorConfigDir,
    EnvVar::AntigravityCliConfigDir,
    EnvVar::GrokHome,
];

#[derive(Clone, Debug)]
struct IntegrationEnvironment {
    paths: HashMap<EnvVar, CapturedEnvPath>,
}

impl IntegrationEnvironment {
    fn capture(read_path: impl Fn(EnvVar) -> io::Result<Option<PathBuf>>) -> Self {
        let paths = INTEGRATION_PATH_ENV_VARS
            .iter()
            .copied()
            .map(|variable| {
                let value = read_path(variable).map_err(|error| DirectoryError {
                    kind: error.kind(),
                    message: error.to_string(),
                });
                (variable, value)
            })
            .collect();
        Self { paths }
    }

    fn path(&self, variable: EnvVar) -> io::Result<Option<PathBuf>> {
        match self.paths.get(&variable) {
            Some(Ok(path)) => Ok(path.clone()),
            Some(Err(error)) => Err(io::Error::new(error.kind, error.message.clone())),
            None => Err(io::Error::new(
                ErrorKind::NotFound,
                format!("integration environment variable {variable} was not resolved"),
            )),
        }
    }

    fn home_dir(&self) -> io::Result<PathBuf> {
        self.path(EnvVar::Home)?
            .ok_or_else(shepr_core::pathutil::missing_home_error)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DirectoryKey {
    PiExtension,
    OmpExtension,
    Claude,
    Codex,
    Copilot,
    Devin,
    Droid,
    Kimi,
    Opencode,
    OpencodeState,
    Kilo,
    Cursor,
    Mastracode,
    AntigravityCli,
    Grok,
}

/// Agent-owned config locations, resolved once by the caller that starts an
/// install (the server, at launch). Install and status code receives this
/// value and never consults the process environment while it is choosing
/// files to read or write.
///
/// The environment read is the server's, not that of the agents in its panes.
/// A server started over SSH runs under a non-interactive shell, which reads
/// only files like `~/.zshenv`, while panes run interactive login shells. An
/// override such as `CLAUDE_CONFIG_DIR` exported only in an interactive rc
/// file therefore reaches the agent but not the server, which then installs
/// into the default directory. Such an override has to be exported where
/// non-interactive shells read it. Reading each detected agent's own
/// environment instead was considered and not built: shepr's owner sets none
/// of these overrides.
#[derive(Clone, Debug)]
pub struct AgentIntegrationPaths {
    directories: HashMap<DirectoryKey, CapturedDirectory>,
    config_update_lock_dir: CapturedDirectory,
}

impl AgentIntegrationPaths {
    pub fn resolve() -> Self {
        Self::resolve_with(|variable| shepr_core::env::read_path(variable).map_err(io::Error::from))
    }

    fn resolve_with(read_path: impl Fn(EnvVar) -> io::Result<Option<PathBuf>>) -> Self {
        let environment = IntegrationEnvironment::capture(read_path);
        let config_update_lock_dir =
            resolve_config_update_lock_dir(&environment).map_err(|error| DirectoryError {
                kind: error.kind(),
                message: error.to_string(),
            });
        let directories = [
            (DirectoryKey::PiExtension, pi_extension_dir(&environment)),
            (DirectoryKey::OmpExtension, omp_extension_dir(&environment)),
            (DirectoryKey::Claude, claude_dir(&environment)),
            (DirectoryKey::Codex, codex_dir(&environment)),
            (DirectoryKey::Copilot, copilot_dir(&environment)),
            (DirectoryKey::Devin, devin_dir(&environment)),
            (DirectoryKey::Droid, droid_dir(&environment)),
            (DirectoryKey::Kimi, kimi_dir(&environment)),
            (DirectoryKey::Opencode, opencode_dir(&environment)),
            (
                DirectoryKey::OpencodeState,
                opencode_state_dir(&environment),
            ),
            (DirectoryKey::Kilo, kilo_dir(&environment)),
            (DirectoryKey::Cursor, cursor_dir(&environment)),
            (DirectoryKey::Mastracode, mastracode_dir(&environment)),
            (
                DirectoryKey::AntigravityCli,
                antigravity_cli_dir(&environment),
            ),
            (DirectoryKey::Grok, grok_dir(&environment)),
        ]
        .into_iter()
        .map(|(key, result)| {
            let result = result.map_err(|error| DirectoryError {
                kind: error.kind(),
                message: error.to_string(),
            });
            (key, result)
        })
        .collect();
        Self {
            directories,
            config_update_lock_dir,
        }
    }

    pub(crate) fn directory(&self, key: DirectoryKey) -> io::Result<PathBuf> {
        match self.directories.get(&key) {
            Some(Ok(path)) => Ok(path.clone()),
            Some(Err(error)) => Err(io::Error::new(error.kind, error.message.clone())),
            None => Err(io::Error::new(
                ErrorKind::NotFound,
                format!("integration directory {key:?} was not resolved"),
            )),
        }
    }

    pub(crate) fn config_update_lock_dir(&self) -> io::Result<PathBuf> {
        match &self.config_update_lock_dir {
            Ok(path) => Ok(path.clone()),
            Err(error) => Err(io::Error::new(error.kind, error.message.clone())),
        }
    }
}

fn resolve_config_update_lock_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    // Agent configs are shared by dev and release builds. Capture the same
    // lock root as the agent paths so every operation in this launch uses one
    // stable environment snapshot.
    let state_home = match environment.path(EnvVar::XdgStateHome)? {
        Some(path) => path,
        None => environment.home_dir()?.join(".local/state"),
    };
    Ok(state_home.join("shepr").join("integration-locks"))
}

fn pi_extension_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(
        config_dir_from_env_or_home(environment, EnvVar::PiCodingAgentDir, &[".pi", "agent"])?
            .join("extensions"),
    )
}

fn omp_extension_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    let config_dir = environment
        .path(EnvVar::PiConfigDir)?
        .unwrap_or_else(|| ".omp".into());
    let config_dir = expand_tilde_path_with_environment(config_dir, environment)?;
    let config_dir = if config_dir.is_absolute() {
        config_dir
    } else {
        environment.home_dir()?.join(config_dir)
    };
    Ok(config_dir.join("agent").join("extensions"))
}

fn claude_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, EnvVar::ClaudeConfigDir, &[".claude"])
}

fn codex_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, EnvVar::CodexHome, &[".codex"])
}

fn kimi_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, EnvVar::KimiCodeHome, &[".kimi-code"])
}

fn copilot_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, EnvVar::CopilotHome, &[".copilot"])
}

fn devin_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    // Devin's config is another tool's location, under the XDG config home
    // when one is set and Devin's conventional HOME path otherwise.
    if let Some(path) = environment.path(EnvVar::XdgConfigHome)? {
        return Ok(path.join("devin"));
    }

    Ok(environment.home_dir()?.join(".config").join("devin"))
}

fn droid_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(environment.home_dir()?.join(".factory"))
}

fn config_dir_from_env_or_home(
    environment: &IntegrationEnvironment,
    env_var: EnvVar,
    home_relative_segments: &[&str],
) -> io::Result<PathBuf> {
    if let Some(value) = environment.path(env_var)? {
        return expand_tilde_path_with_environment(value, environment);
    }

    let mut path = environment.home_dir()?;
    for segment in home_relative_segments {
        path.push(segment);
    }
    Ok(path)
}

fn opencode_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    if let Some(path) = environment.path(EnvVar::XdgConfigHome)? {
        return Ok(path.join("opencode"));
    }

    Ok(environment.home_dir()?.join(".config/opencode"))
}

fn opencode_state_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    // OpenCode's state is another tool's location, under the XDG state home
    // when one is set and OpenCode's conventional HOME path otherwise.
    if let Some(path) = environment.path(EnvVar::XdgStateHome)? {
        return Ok(path.join("opencode"));
    }

    Ok(environment.home_dir()?.join(".local/state/opencode"))
}

fn kilo_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    if let Some(path) = environment.path(EnvVar::XdgConfigHome)? {
        return Ok(path.join("kilo"));
    }

    Ok(environment.home_dir()?.join(".config/kilo"))
}

fn cursor_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, EnvVar::CursorConfigDir, &[".cursor"])
}

fn mastracode_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(environment.home_dir()?.join(".mastracode"))
}

fn antigravity_cli_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    // Antigravity CLI discovers global customizations (hooks.json included)
    // from ~/.gemini/config; ~/.gemini/antigravity-cli holds runtime data and
    // is never read for hooks.
    config_dir_from_env_or_home(
        environment,
        EnvVar::AntigravityCliConfigDir,
        &[".gemini", "config"],
    )
}

fn grok_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    // The grok CLI honors GROK_HOME as its config home (config.toml,
    // auth.json, hooks/); mirror it so hook installs land where grok looks.
    config_dir_from_env_or_home(environment, EnvVar::GrokHome, &[".grok"])
}

fn expand_tilde_path_with_environment(
    value: PathBuf,
    environment: &IntegrationEnvironment,
) -> io::Result<PathBuf> {
    let bytes = value.as_os_str().as_bytes();
    let needs_home = bytes == b"~" || bytes.starts_with(b"~/");
    let home = if needs_home {
        Some(environment.home_dir()?)
    } else {
        None
    };
    shepr_core::pathutil::expand_tilde_path_with_home(value, home.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn paths_with(values: &[(EnvVar, &str)]) -> AgentIntegrationPaths {
        let values: HashMap<EnvVar, OsString> = values
            .iter()
            .map(|(variable, value)| (*variable, OsString::from(value)))
            .collect();
        AgentIntegrationPaths::resolve_with(|variable| {
            shepr_core::env::resolve_path(variable, values.get(&variable).map(OsString::as_os_str))
                .map_err(io::Error::from)
        })
    }

    fn directory(paths: &AgentIntegrationPaths, key: DirectoryKey) -> io::Result<PathBuf> {
        paths.directory(key)
    }

    #[test]
    fn config_dir_env_override_expands_tilde() {
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::CursorConfigDir, "~/cursor-home"),
        ]);
        assert_eq!(
            directory(&env, DirectoryKey::Cursor).expect("test precondition"),
            PathBuf::from("/test/home/cursor-home")
        );
        // Empty is unset; padding is refused naming the variable.
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::CursorConfigDir, "")]);
        assert_eq!(
            directory(&env, DirectoryKey::Cursor).expect("test precondition"),
            PathBuf::from("/test/home/.cursor")
        );
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::CursorConfigDir, "~/cursor-home "),
        ]);
        let error =
            directory(&env, DirectoryKey::Cursor).expect_err("a padded override is refused");
        assert!(error.to_string().contains("CURSOR_CONFIG_DIR"), "{error}");
    }

    #[test]
    fn omp_pi_config_dir_override_expands_tilde() {
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::PiConfigDir, "~/.omp2"),
        ]);
        assert_eq!(
            directory(&env, DirectoryKey::OmpExtension).expect("test precondition"),
            PathBuf::from("/test/home/.omp2/agent/extensions")
        );
    }

    #[test]
    fn opencode_state_dir_defaults_to_local_state() {
        let env = paths_with(&[(EnvVar::Home, "/test/home")]);
        assert_eq!(
            directory(&env, DirectoryKey::OpencodeState).expect("test precondition"),
            PathBuf::from("/test/home/.local/state/opencode")
        );
    }

    #[test]
    fn opencode_state_dir_honors_xdg_state_home() {
        let xdg = "/test/state";
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgStateHome, xdg)]);
        assert_eq!(
            directory(&env, DirectoryKey::OpencodeState).expect("test precondition"),
            PathBuf::from(xdg).join("opencode")
        );
    }

    #[test]
    fn devin_dir_ignores_empty_and_refuses_relative_xdg_config_home() {
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgConfigHome, "")]);
        assert_eq!(
            directory(&env, DirectoryKey::Devin).expect("home fallback"),
            PathBuf::from("/test/home/.config/devin")
        );
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::XdgConfigHome, "relative/config"),
        ]);
        let error = directory(&env, DirectoryKey::Devin)
            .expect_err("a relative XDG config home is refused");
        assert!(error.to_string().contains("XDG_CONFIG_HOME"), "{error}");

        let xdg = "/test/config";
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgConfigHome, xdg)]);
        assert_eq!(
            directory(&env, DirectoryKey::Devin).expect("absolute XDG path"),
            PathBuf::from(xdg).join("devin")
        );
    }

    #[test]
    fn opencode_and_kilo_dirs_honor_absolute_xdg_config_home() {
        let env = paths_with(&[(EnvVar::Home, "/test/home")]);
        assert_eq!(
            directory(&env, DirectoryKey::Opencode).expect("home fallback"),
            PathBuf::from("/test/home/.config/opencode")
        );
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::XdgConfigHome, "relative/config"),
        ]);
        assert!(
            directory(&env, DirectoryKey::Kilo).is_err(),
            "a relative XDG config home is refused"
        );

        let xdg = "/test/config";
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgConfigHome, xdg)]);
        assert_eq!(
            directory(&env, DirectoryKey::Opencode).expect("absolute XDG path"),
            PathBuf::from(xdg).join("opencode")
        );
        assert_eq!(
            directory(&env, DirectoryKey::Kilo).expect("absolute XDG path"),
            PathBuf::from(xdg).join("kilo")
        );
    }

    #[test]
    fn opencode_state_dir_ignores_empty_and_refuses_relative_xdg_state_home() {
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgStateHome, "")]);
        assert_eq!(
            directory(&env, DirectoryKey::OpencodeState).expect("home fallback"),
            PathBuf::from("/test/home/.local/state/opencode")
        );
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::XdgStateHome, "relative/state"),
        ]);
        assert!(
            directory(&env, DirectoryKey::OpencodeState).is_err(),
            "a relative XDG state home is refused"
        );
    }

    #[test]
    fn resolve_captures_each_integration_path_value_once() {
        let values = HashMap::from([
            (EnvVar::Home, OsString::from("/test/home")),
            (EnvVar::GrokHome, OsString::from("/test/grok")),
        ]);
        let reads = std::cell::RefCell::new(HashMap::<EnvVar, usize>::new());
        let paths = AgentIntegrationPaths::resolve_with(|variable| {
            *reads.borrow_mut().entry(variable).or_default() += 1;
            shepr_core::env::resolve_path(variable, values.get(&variable).map(OsString::as_os_str))
                .map_err(io::Error::from)
        });

        for variable in INTEGRATION_PATH_ENV_VARS {
            assert_eq!(reads.borrow().get(variable), Some(&1), "{variable}");
        }
        assert_eq!(
            directory(&paths, DirectoryKey::Grok).expect("captured Grok path"),
            PathBuf::from("/test/grok")
        );
    }
}
