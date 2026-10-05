use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::{collections::HashMap, io::ErrorKind};

use super::types::{InstallErrorKind, InstallIssue};
use shepr_agent::{Agent, IntegrationTarget};
use shepr_core::env::EnvVar;

#[derive(Clone, Debug)]
struct DirectoryError(std::sync::Arc<io::Error>);

impl std::fmt::Display for DirectoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0.as_ref(), formatter)
    }
}

impl std::error::Error for DirectoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn capture_directory(result: io::Result<PathBuf>) -> CapturedDirectory {
    result.map_err(|error| DirectoryError(std::sync::Arc::new(error)))
}

fn captured_directory(result: &CapturedDirectory) -> io::Result<PathBuf> {
    result
        .clone()
        .map_err(|error| io::Error::new(error.0.kind(), error))
}

type CapturedDirectory = Result<PathBuf, DirectoryError>;
type CapturedEnvPath = Result<Option<PathBuf>, DirectoryError>;

#[derive(Clone, Debug)]
pub(super) struct IntegrationEnvironment {
    paths: HashMap<EnvVar, CapturedEnvPath>,
}

impl IntegrationEnvironment {
    fn capture(read_path: impl Fn(EnvVar) -> io::Result<Option<PathBuf>>) -> Self {
        let paths = [EnvVar::Home, EnvVar::XdgConfigHome, EnvVar::XdgStateHome]
            .into_iter()
            .chain(
                shepr_agent::AGENTS
                    .iter()
                    // Only agents with an integration target have a directory
                    // to install into; another agent's override is never read.
                    .filter_map(|descriptor| {
                        descriptor.integration.and(descriptor.config_dir_override)
                    }),
            )
            .map(|variable| {
                let value =
                    read_path(variable).map_err(|error| DirectoryError(std::sync::Arc::new(error)));
                (variable, value)
            })
            .collect();
        Self { paths }
    }

    fn path(&self, variable: EnvVar) -> io::Result<Option<PathBuf>> {
        match self.paths.get(&variable) {
            Some(Ok(path)) => Ok(path.clone()),
            Some(Err(error)) => Err(io::Error::new(error.0.kind(), error.clone())),
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

/// Agent-owned config locations, resolved once by the caller that starts an
/// install (the server, at launch). Install and status code receives this
/// value and never consults the process environment while it is choosing
/// files to read or write. Agent-specific overrides expand `~` and must be
/// absolute: relative config paths would resolve against the server's cwd here
/// and the pane's cwd in the agent.
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
    directories: HashMap<IntegrationTarget, CapturedDirectory>,
    opencode_state: CapturedDirectory,
    config_update_lock_dir: CapturedDirectory,
}

impl AgentIntegrationPaths {
    pub fn resolve() -> Self {
        Self::resolve_with(|variable| shepr_core::env::read_path(variable).map_err(io::Error::from))
    }

    fn resolve_with(read_path: impl Fn(EnvVar) -> io::Result<Option<PathBuf>>) -> Self {
        let environment = IntegrationEnvironment::capture(read_path);
        let directories = IntegrationTarget::all()
            .map(|target| {
                (
                    target,
                    capture_directory(super::registry::resolve_target_directory(
                        &environment,
                        target,
                    )),
                )
            })
            .collect();
        Self {
            directories,
            opencode_state: capture_directory(opencode_state_dir(&environment)),
            config_update_lock_dir: capture_directory(resolve_config_update_lock_dir(&environment)),
        }
    }

    pub(crate) fn directory(&self, target: IntegrationTarget) -> io::Result<PathBuf> {
        let directory = self.directories.get(&target).ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                format!("integration directory for {target:?} was not resolved"),
            )
        })?;
        captured_directory(directory)
    }

    pub(crate) fn opencode_state_directory(&self) -> io::Result<PathBuf> {
        captured_directory(&self.opencode_state)
    }

    pub(crate) fn config_update_lock_dir(&self) -> io::Result<PathBuf> {
        captured_directory(&self.config_update_lock_dir)
    }
}

fn resolve_config_update_lock_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    // Agent configs are shared by dev and release builds. Resolve the shared
    // installer lock directory from the same environment snapshot.
    let xdg_state_home =
        shepr_core::env::xdg_state_home_with(|variable| environment.path(variable))?;
    Ok(shepr_paths::integration_lock_dir(&xdg_state_home))
}

pub(super) fn pi_extension_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(config_dir_from_env_or_home(environment, Agent::Pi, &[".pi", "agent"])?.join("extensions"))
}

pub(super) fn omp_extension_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    let config_dir =
        agent_config_override(environment, Agent::Omp)?.unwrap_or_else(|| ".omp".into());
    let config_dir = if config_dir.is_absolute() {
        config_dir
    } else {
        environment.home_dir()?.join(config_dir)
    };
    Ok(config_dir.join("agent").join("extensions"))
}

pub(super) fn claude_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, Agent::Claude, &[".claude"])
}

pub(super) fn codex_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, Agent::Codex, &[".codex"])
}

pub(super) fn kimi_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, Agent::Kimi, &[".kimi-code"])
}

pub(super) fn copilot_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, Agent::GithubCopilot, &[".copilot"])
}

pub(super) fn devin_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(shepr_core::env::xdg_config_home_with(|variable| environment.path(variable))?.join("devin"))
}

pub(super) fn droid_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(environment.home_dir()?.join(".factory"))
}

fn agent_config_override(
    environment: &IntegrationEnvironment,
    agent: Agent,
) -> io::Result<Option<PathBuf>> {
    match agent.descriptor().config_dir_override {
        Some(variable) => environment
            .path(variable)?
            .map(|value| {
                let path = expand_tilde_path_with_environment(value, environment)?;
                if !path.is_absolute() {
                    return Err(InstallIssue::io_error(
                        InstallErrorKind::ConfigShape,
                        format!(
                            "{variable} must be an absolute path; use an absolute path or ~/..."
                        ),
                    ));
                }
                Ok(path)
            })
            .transpose(),
        None => Ok(None),
    }
}

fn config_dir_from_env_or_home(
    environment: &IntegrationEnvironment,
    agent: Agent,
    home_relative_segments: &[&str],
) -> io::Result<PathBuf> {
    if let Some(value) = agent_config_override(environment, agent)? {
        return Ok(value);
    }

    let mut path = environment.home_dir()?;
    for segment in home_relative_segments {
        path.push(segment);
    }
    Ok(path)
}

pub(super) fn opencode_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(
        shepr_core::env::xdg_config_home_with(|variable| environment.path(variable))?
            .join("opencode"),
    )
}

fn opencode_state_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(
        shepr_core::env::xdg_state_home_with(|variable| environment.path(variable))?
            .join("opencode"),
    )
}

pub(super) fn kilo_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(shepr_core::env::xdg_config_home_with(|variable| environment.path(variable))?.join("kilo"))
}

pub(super) fn cursor_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    config_dir_from_env_or_home(environment, Agent::Cursor, &[".cursor"])
}

pub(super) fn mastracode_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    Ok(environment.home_dir()?.join(".mastracode"))
}

pub(super) fn antigravity_cli_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    // Antigravity CLI discovers global customizations (hooks.json included)
    // from ~/.gemini/config; ~/.gemini/antigravity-cli holds runtime data and
    // is never read for hooks.
    config_dir_from_env_or_home(environment, Agent::Antigravity, &[".gemini", "config"])
}

pub(super) fn grok_dir(environment: &IntegrationEnvironment) -> io::Result<PathBuf> {
    // The grok CLI honors GROK_HOME as its config home (config.toml,
    // auth.json, hooks/); mirror it so hook installs land where grok looks.
    config_dir_from_env_or_home(environment, Agent::Grok, &[".grok"])
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

    fn directory(paths: &AgentIntegrationPaths, target: IntegrationTarget) -> io::Result<PathBuf> {
        paths.directory(target)
    }

    #[test]
    fn config_dir_env_override_expands_tilde() {
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::CursorConfigDir, "~/cursor-home"),
        ]);
        assert_eq!(
            directory(&env, IntegrationTarget::Cursor).expect("test precondition"),
            PathBuf::from("/test/home/cursor-home")
        );
        // Empty is unset; padding is refused naming the variable.
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::CursorConfigDir, "")]);
        assert_eq!(
            directory(&env, IntegrationTarget::Cursor).expect("test precondition"),
            PathBuf::from("/test/home/.cursor")
        );
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::CursorConfigDir, "~/cursor-home "),
        ]);
        let error =
            directory(&env, IntegrationTarget::Cursor).expect_err("a padded override is refused");
        assert!(error.to_string().contains("CURSOR_CONFIG_DIR"), "{error}");
    }

    #[test]
    fn relative_agent_config_overrides_are_refused() {
        let values = HashMap::from([
            (EnvVar::Home, OsString::from("/test/home")),
            (EnvVar::ClaudeConfigDir, OsString::from("relative/config")),
            (EnvVar::CodexHome, OsString::from("relative/config")),
            (EnvVar::CopilotHome, OsString::from("relative/config")),
            (EnvVar::CursorConfigDir, OsString::from("relative/config")),
            (EnvVar::KimiCodeHome, OsString::from("relative/config")),
            (EnvVar::GrokHome, OsString::from("relative/config")),
            (EnvVar::PiCodingAgentDir, OsString::from("relative/config")),
            (
                EnvVar::AntigravityCliConfigDir,
                OsString::from("relative/config"),
            ),
            (EnvVar::PiConfigDir, OsString::from("relative/config")),
        ]);
        let environment = IntegrationEnvironment::capture(|variable| {
            shepr_core::env::resolve_path(variable, values.get(&variable).map(OsString::as_os_str))
                .map_err(io::Error::from)
        });

        for (agent, variable) in [
            (Agent::Claude, EnvVar::ClaudeConfigDir),
            (Agent::Codex, EnvVar::CodexHome),
            (Agent::GithubCopilot, EnvVar::CopilotHome),
            (Agent::Cursor, EnvVar::CursorConfigDir),
            (Agent::Kimi, EnvVar::KimiCodeHome),
            (Agent::Grok, EnvVar::GrokHome),
            (Agent::Pi, EnvVar::PiCodingAgentDir),
            (Agent::Antigravity, EnvVar::AntigravityCliConfigDir),
            (Agent::Omp, EnvVar::PiConfigDir),
        ] {
            let error = agent_config_override(&environment, agent)
                .expect_err("a relative agent config override is refused");
            assert!(error.to_string().contains(&variable.to_string()), "{error}");
        }
    }

    #[test]
    fn omp_pi_config_dir_override_expands_tilde() {
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::PiConfigDir, "~/.omp2"),
        ]);
        assert_eq!(
            directory(&env, IntegrationTarget::Omp).expect("test precondition"),
            PathBuf::from("/test/home/.omp2/agent/extensions")
        );
    }

    #[test]
    fn opencode_state_dir_defaults_to_local_state() {
        let env = paths_with(&[(EnvVar::Home, "/test/home")]);
        assert_eq!(
            env.opencode_state_directory().expect("test precondition"),
            PathBuf::from("/test/home/.local/state/opencode")
        );
    }

    #[test]
    fn opencode_state_dir_honors_xdg_state_home() {
        let xdg = "/test/state";
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgStateHome, xdg)]);
        assert_eq!(
            env.opencode_state_directory().expect("test precondition"),
            PathBuf::from(xdg).join("opencode")
        );
    }

    #[test]
    fn devin_dir_ignores_empty_and_refuses_relative_xdg_config_home() {
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgConfigHome, "")]);
        assert_eq!(
            directory(&env, IntegrationTarget::Devin).expect("home fallback"),
            PathBuf::from("/test/home/.config/devin")
        );
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::XdgConfigHome, "relative/config"),
        ]);
        let error = directory(&env, IntegrationTarget::Devin)
            .expect_err("a relative XDG config home is refused");
        assert!(error.to_string().contains("XDG_CONFIG_HOME"), "{error}");

        let xdg = "/test/config";
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgConfigHome, xdg)]);
        assert_eq!(
            directory(&env, IntegrationTarget::Devin).expect("absolute XDG path"),
            PathBuf::from(xdg).join("devin")
        );
    }

    #[test]
    fn opencode_and_kilo_dirs_honor_absolute_xdg_config_home() {
        let env = paths_with(&[(EnvVar::Home, "/test/home")]);
        assert_eq!(
            directory(&env, IntegrationTarget::Opencode).expect("home fallback"),
            PathBuf::from("/test/home/.config/opencode")
        );
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::XdgConfigHome, "relative/config"),
        ]);
        assert!(
            directory(&env, IntegrationTarget::Kilo).is_err(),
            "a relative XDG config home is refused"
        );

        let xdg = "/test/config";
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgConfigHome, xdg)]);
        assert_eq!(
            directory(&env, IntegrationTarget::Opencode).expect("absolute XDG path"),
            PathBuf::from(xdg).join("opencode")
        );
        assert_eq!(
            directory(&env, IntegrationTarget::Kilo).expect("absolute XDG path"),
            PathBuf::from(xdg).join("kilo")
        );
    }

    #[test]
    fn opencode_state_dir_ignores_empty_and_refuses_relative_xdg_state_home() {
        let env = paths_with(&[(EnvVar::Home, "/test/home"), (EnvVar::XdgStateHome, "")]);
        assert_eq!(
            env.opencode_state_directory().expect("home fallback"),
            PathBuf::from("/test/home/.local/state/opencode")
        );
        let env = paths_with(&[
            (EnvVar::Home, "/test/home"),
            (EnvVar::XdgStateHome, "relative/state"),
        ]);
        assert!(
            env.opencode_state_directory().is_err(),
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

        for variable in [EnvVar::Home, EnvVar::XdgConfigHome, EnvVar::XdgStateHome]
            .into_iter()
            .chain(
                shepr_agent::AGENTS
                    .iter()
                    // Only agents with an integration target have a directory
                    // to install into; another agent's override is never read.
                    .filter_map(|descriptor| {
                        descriptor.integration.and(descriptor.config_dir_override)
                    }),
            )
        {
            assert_eq!(reads.borrow().get(&variable), Some(&1), "{variable}");
        }
        for unused in [EnvVar::QoderConfigDir, EnvVar::QwenHome] {
            assert!(!reads.borrow().contains_key(&unused), "{unused}");
        }
        assert_eq!(
            directory(&paths, IntegrationTarget::Grok).expect("captured Grok path"),
            PathBuf::from("/test/grok")
        );
    }
}
