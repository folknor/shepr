use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shepr_core::env::EnvVar;

use super::{
    Config, ConfigDiagnostic, ConfigProvenance, ConfigSource, NewTerminalCwdConfig,
    ValidatedConfig, ValidatedTerminalConfig,
    model::{ConfigDocumentState, LoadedConfig},
    validated::CwdCheck,
};

/// The directory name shepr uses under every XDG base directory.
///
/// Every build profile uses the same name: a dev build and the installed
/// release build share config, state and runtime directories. A dev run is
/// kept apart from the installed server by running it in a named session
/// (`--session <name>`), and a server of another build is refused by the
/// build-identity checks, which cover the build profile as well as the source.
pub fn app_dir_name() -> &'static str {
    // `cfg!(test)` holds only while this crate's own unit tests are compiled;
    // a test in any other crate that reaches this compiles it as a normal
    // dependency and gets `shepr`. What keeps every test out of the real
    // directories is `shepr_test_support::IsolatedEnv`, which points `HOME`
    // and the XDG variables at scratch, not this name.
    if cfg!(test) { "shepr-test" } else { "shepr" }
}

/// Paths and the local target resolved once at the process boundary and
/// passed to consumers. Production constructors reject unresolved path inputs
/// that would put files relative to the working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppPaths {
    config_dir: PathBuf,
    state_dir: PathBuf,
    xdg_runtime_dir: PathBuf,
    runtime_dir: PathBuf,
    config_file: PathBuf,
    home_dir: Option<PathBuf>,
    current_dir: Option<PathBuf>,
    session_id: super::SessionId,
    server_address: super::ServerAddress,
    provenance: PathProvenance,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for AppPaths {
    fn default() -> Self {
        // Keep no-I/O fixtures wire-resolvable; filesystem tests use ScratchDir-backed paths.
        let root = Path::new("/nonexistent/shepr-test-config");
        Self::test_with_context(root, Some(root), None)
    }
}

impl<'de> Deserialize<'de> for AppPaths {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            config_dir: PathBuf,
            state_dir: PathBuf,
            xdg_runtime_dir: PathBuf,
            runtime_dir: PathBuf,
            config_file: PathBuf,
            home_dir: Option<PathBuf>,
            current_dir: Option<PathBuf>,
            session_id: super::SessionId,
            server_address: super::ServerAddress,
            provenance: PathProvenance,
        }

        let wire = Wire::deserialize(deserializer)?;
        let paths = Self {
            config_dir: wire.config_dir,
            state_dir: wire.state_dir,
            xdg_runtime_dir: wire.xdg_runtime_dir,
            runtime_dir: wire.runtime_dir,
            config_file: wire.config_file,
            home_dir: wire.home_dir,
            current_dir: wire.current_dir,
            session_id: wire.session_id,
            server_address: wire.server_address,
            provenance: wire.provenance,
        };
        paths
            .validate_resolved()
            .map_err(serde::de::Error::custom)?;
        Ok(paths)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathProvenance {
    pub config_dir: ConfigSource,
    pub state_dir: ConfigSource,
    pub runtime_dir: ConfigSource,
    pub config_file: ConfigSource,
    pub home_dir: ConfigSource,
    /// Captured from the process at launch; no config setting selects its source.
    pub current_dir: ConfigSource,
    pub session_id: ConfigSource,
    pub api_socket: ConfigSource,
    pub client_socket: ConfigSource,
}

impl AppPaths {
    fn validate_resolved(&self) -> Result<(), String> {
        for (name, path) in [
            ("config_dir", self.config_dir()),
            ("state_dir", self.state_dir()),
            ("XDG runtime directory", self.xdg_runtime_dir()),
            ("runtime_dir", self.runtime_dir()),
            ("config_file", self.config_file()),
            ("API socket", self.server_address.api_socket()),
            ("client socket", self.server_address.client_socket()),
        ] {
            if !path.is_absolute() {
                return Err(format!("resolved {name} must be an absolute path"));
            }
        }
        if self.home_dir().is_none_or(|path| !path.is_absolute()) {
            return Err("resolved home_dir must be an absolute path".to_owned());
        }
        if self.current_dir().is_some_and(|path| !path.is_absolute()) {
            return Err("resolved current_dir must be an absolute path".to_owned());
        }
        Ok(())
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The XDG runtime root before the application-specific directory is added.
    pub fn xdg_runtime_dir(&self) -> &Path {
        &self.xdg_runtime_dir
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn home_dir(&self) -> Option<&Path> {
        self.home_dir.as_deref()
    }

    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    pub fn session_id(&self) -> &super::SessionId {
        &self.session_id
    }

    pub fn server_address(&self) -> &super::ServerAddress {
        &self.server_address
    }

    pub fn provenance(&self) -> &PathProvenance {
        &self.provenance
    }

    /// Resolve XDG directories, session identity and socket target once from
    /// the inherited process environment.
    pub fn resolve() -> Result<Self, Vec<String>> {
        resolve_paths_from_env(None, None, false)
    }

    /// Resolve paths and the local session/socket target from one environment
    /// snapshot. `Some(Default)` represents an explicit request for the
    /// default session and therefore takes precedence over socket overrides.
    pub fn resolve_with_session(
        requested_session: Option<super::SessionId>,
    ) -> Result<Self, Vec<String>> {
        let session_source = requested_session
            .as_ref()
            .map(|_| ConfigSource::CliFlag("--session".to_owned()));
        resolve_paths_from_env(requested_session, session_source, false)
    }

    /// Resolve only the machine catalog's local paths, without allowing local
    /// session or socket environment values to affect a remote command.
    pub fn resolve_for_machine() -> Result<Self, Vec<String>> {
        resolve_paths_from_env(Some(super::SessionId::Default), None, true)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_at(root: &Path) -> Self {
        Self::test_with_context(root, None, None)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_with_context(
        root: &Path,
        home_dir: Option<&Path>,
        current_dir: Option<&Path>,
    ) -> Self {
        Self {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            xdg_runtime_dir: root.to_path_buf(),
            runtime_dir: root.join("runtime"),
            config_file: root.join("config/config.toml"),
            home_dir: home_dir.map(Path::to_path_buf),
            current_dir: current_dir.map(Path::to_path_buf),
            session_id: super::SessionId::Default,
            server_address: super::ServerAddress::resolve_paths(
                &root.join("runtime"),
                &super::SessionId::Default,
                false,
                None,
                None,
            ),
            provenance: PathProvenance {
                config_dir: ConfigSource::Default,
                state_dir: ConfigSource::Default,
                runtime_dir: ConfigSource::Default,
                config_file: ConfigSource::Default,
                home_dir: ConfigSource::Default,
                current_dir: ConfigSource::Default,
                session_id: ConfigSource::Default,
                api_socket: ConfigSource::Default,
                client_socket: ConfigSource::Default,
            },
        }
    }
}

/// An XDG base directory for shepr. Unset or empty falls back under `HOME`;
/// a relative, padded or non-UTF-8 value is refused rather than ignored, so a
/// mistyped variable fails the launch instead of silently moving shepr's
/// config or state back under `HOME`.
fn platform_xdg_dir(
    variable: EnvVar,
    home_suffix: &str,
    home_dir: Option<&Path>,
) -> io::Result<(PathBuf, ConfigSource)> {
    if let Some(directory) = shepr_core::env::read_path(variable)? {
        return Ok((
            directory.join(app_dir_name()),
            ConfigSource::EnvironmentVariable(variable.name().to_owned()),
        ));
    }

    let home_dir = home_dir.ok_or_else(shepr_core::pathutil::missing_home_error)?;
    Ok((
        home_dir.join(home_suffix).join(app_dir_name()),
        ConfigSource::Default,
    ))
}

fn socket_path_override(variable: EnvVar, diagnostics: &mut Vec<String>) -> Option<PathBuf> {
    shepr_core::env::read_path(variable).unwrap_or_else(|error| {
        diagnostics.push(error.to_string());
        None
    })
}

fn resolve_paths_from_env(
    requested_session: Option<super::SessionId>,
    requested_session_source: Option<ConfigSource>,
    ignore_local_target_env: bool,
) -> Result<AppPaths, Vec<String>> {
    let session_selection_was_forced = requested_session.is_some();
    let mut target_env_diagnostics = Vec::new();
    let (api_socket_override, client_socket_override, inherited_session) =
        if ignore_local_target_env {
            (None, None, None)
        } else {
            let api_socket_override =
                socket_path_override(EnvVar::SheprSocketPath, &mut target_env_diagnostics);
            let client_socket_override =
                socket_path_override(EnvVar::SheprClientSocketPath, &mut target_env_diagnostics);
            let inherited_session = if requested_session.is_some() {
                None
            } else {
                shepr_core::env::read_text(EnvVar::SheprSession).unwrap_or_else(|error| {
                    target_env_diagnostics.push(error.to_string());
                    None
                })
            };
            (
                api_socket_override,
                client_socket_override,
                inherited_session,
            )
        };
    if !target_env_diagnostics.is_empty() {
        return Err(target_env_diagnostics);
    }
    let inherited_session_accepted = inherited_session
        .as_deref()
        .is_some_and(|name| super::SessionId::parse(name).is_ok());
    let (session_id, session_was_requested) =
        super::SessionId::resolve(requested_session, inherited_session.as_deref())
            .map_err(|error| vec![format!("session selection error: {error}")])?;

    let home_dir = shepr_core::pathutil::home_dir().map_err(|error| vec![error.to_string()])?;
    let current_dir = std::env::current_dir().ok();
    let (config_dir, config_dir_source) =
        platform_xdg_dir(EnvVar::XdgConfigHome, ".config", Some(&home_dir))
            .map(|(path, source)| (Ok(path), source))
            .unwrap_or_else(|error| (Err(error), ConfigSource::Default));
    let (state_dir, state_dir_source) =
        platform_xdg_dir(EnvVar::XdgStateHome, ".local/state", Some(&home_dir))
            .map(|(path, source)| (Ok(path), source))
            .unwrap_or_else(|error| (Err(error), ConfigSource::Default));
    // XDG_RUNTIME_DIR has no base-directory fallback in the XDG spec. Unset
    // and empty are an error for shepr because its runtime sockets need a
    // user-private runtime directory; a relative value is refused by the
    // environment policy.
    let xdg_runtime_dir = shepr_core::env::read_path(EnvVar::XdgRuntimeDir);
    let runtime_dir = match &xdg_runtime_dir {
        Ok(Some(path)) => Ok(path.join(app_dir_name())),
        Ok(None) => Err(io::Error::other(
            "XDG_RUNTIME_DIR must be set to an absolute path",
        )),
        Err(error) => Err(io::Error::other(error.to_string())),
    };
    let xdg_runtime_dir = xdg_runtime_dir.ok().flatten();

    let config_path_override = shepr_core::env::read_path(EnvVar::SheprConfigPath);
    let config_file_source = if matches!(config_path_override, Ok(Some(_))) {
        ConfigSource::EnvironmentVariable(EnvVar::SheprConfigPath.name().to_owned())
    } else {
        config_dir_source.clone()
    };
    let config_file = match config_path_override {
        Err(error) => Err(io::Error::from(error)),
        Ok(Some(path)) => {
            if path.is_absolute() {
                Ok(path)
            } else if let Some(current_dir) = current_dir.as_ref() {
                Ok(current_dir.join(path))
            } else {
                Err(io::Error::other(format!(
                    "cannot resolve relative {} without a current directory",
                    EnvVar::SheprConfigPath
                )))
            }
        }
        Ok(None) => config_dir
            .as_ref()
            .map(|directory| directory.join("config.toml"))
            .map_err(|error| io::Error::other(error.to_string())),
    };

    let mut diagnostics = Vec::new();
    let config_file = match config_file {
        Ok(path) => Some(path),
        Err(error) => {
            diagnostics.push(format!("config path error: {error}"));
            None
        }
    };

    let config_dir = match config_dir {
        Ok(path) => Some(path),
        Err(error) if config_file.is_some() => {
            diagnostics.push(format!("config directory error: {error}"));
            None
        }
        Err(_) => None,
    };

    let state_dir = match state_dir {
        Ok(path) => Some(path),
        Err(error) => {
            diagnostics.push(format!("state directory error: {error}"));
            None
        }
    };
    let runtime_dir = match runtime_dir {
        Ok(path) => Some(path),
        Err(error) => {
            diagnostics.push(format!("runtime directory error: {error}"));
            None
        }
    };

    match (
        config_dir,
        state_dir,
        xdg_runtime_dir,
        runtime_dir,
        config_file,
    ) {
        (
            Some(config_dir),
            Some(state_dir),
            Some(xdg_runtime_dir),
            Some(runtime_dir),
            Some(config_file),
        ) if diagnostics.is_empty() => {
            let server_address = super::ServerAddress::resolve_paths(
                &runtime_dir,
                &session_id,
                session_was_requested,
                api_socket_override.as_deref(),
                client_socket_override.as_deref(),
            );
            let home_dir_source = ConfigSource::EnvironmentVariable(EnvVar::Home.name().to_owned());
            let runtime_dir_source =
                ConfigSource::EnvironmentVariable(EnvVar::XdgRuntimeDir.name().to_owned());
            let session_source = if let Some(source) = requested_session_source {
                source
            } else if inherited_session_accepted && !session_selection_was_forced {
                ConfigSource::EnvironmentVariable(EnvVar::SheprSession.name().to_owned())
            } else {
                ConfigSource::Default
            };
            let api_socket_source = if session_selection_was_forced {
                session_source.clone()
            } else if api_socket_override.is_some() {
                ConfigSource::EnvironmentVariable(EnvVar::SheprSocketPath.name().to_owned())
            } else if inherited_session_accepted {
                session_source.clone()
            } else {
                runtime_dir_source.clone()
            };
            let client_socket_source = if session_selection_was_forced {
                session_source.clone()
            } else if api_socket_override.is_some() {
                ConfigSource::EnvironmentVariable(EnvVar::SheprSocketPath.name().to_owned())
            } else if client_socket_override.is_some() {
                ConfigSource::EnvironmentVariable(EnvVar::SheprClientSocketPath.name().to_owned())
            } else if inherited_session_accepted {
                session_source.clone()
            } else {
                runtime_dir_source.clone()
            };
            Ok(AppPaths {
                config_dir,
                state_dir,
                xdg_runtime_dir,
                runtime_dir,
                config_file,
                home_dir: Some(home_dir),
                current_dir,
                session_id,
                server_address,
                provenance: PathProvenance {
                    config_dir: config_dir_source,
                    state_dir: state_dir_source,
                    runtime_dir: runtime_dir_source,
                    config_file: config_file_source,
                    home_dir: home_dir_source,
                    current_dir: ConfigSource::Default,
                    session_id: session_source,
                    api_socket: api_socket_source,
                    client_socket: client_socket_source,
                },
            })
        }
        _ if diagnostics.is_empty() => {
            Err(vec!["application paths could not be resolved".to_string()])
        }
        _ => Err(diagnostics),
    }
}

/// Normalize UTF-8 byte-order marks in config text.
///
/// TOML tolerates a single BOM at the very start of the document, but a BOM at
/// the start of a later line makes the parser reject the whole file. A
/// line-oriented edit can displace a leading BOM into the middle of the file,
/// so drop line-start BOMs that the TOML parser actually rejects. A U+FEFF that
/// is valid string data is kept, because its parse error would not point at it.
fn normalize_utf8_bom(content: &str) -> String {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if !content.contains('\u{feff}') {
        return content.to_owned();
    }

    let mut normalized = content.to_owned();
    // `toml::Table`, not `toml::Value`: since toml 0.9, `Value::from_str`
    // parses a single value expression rather than a document.
    while let Err(error) = normalized.parse::<toml::Table>() {
        let Some(span) = error.span() else {
            break;
        };
        // toml reads a line-start BOM as the start of a bare key and reports
        // the error just past it ("key with no value"), so look for a BOM at
        // the start of the error's line rather than under the span.
        let bom_len = '\u{feff}'.len_utf8();
        let Some(before) = normalized.get(..span.start) else {
            break;
        };
        let bom_start = before.rfind('\n').map_or(0, |newline| newline + 1);
        if span.start > bom_start + bom_len
            || !normalized
                .get(bom_start..)
                .is_some_and(|line| line.starts_with('\u{feff}'))
        {
            break;
        }
        normalized.replace_range(bom_start..bom_start + bom_len, "");
    }
    normalized
}

fn read_optional_config(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(normalize_utf8_bom(&content))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

impl Config {
    /// Load config data and every diagnostic without rejecting any. The
    /// `config check` command reports these; `load_validated` rejects them,
    /// so `config check` passes exactly when a launch would accept the config.
    pub fn load_for_check(paths: &AppPaths) -> LoadedConfig {
        let mut loaded = Self::load_from_path_with_paths(paths.config_file(), paths);
        if loaded.document_state == ConfigDocumentState::Unavailable
            && let Some(new_cwd) = loaded.unavailable_new_cwd.as_ref()
            && let Err(error) =
                ValidatedTerminalConfig::parse_new_cwd(new_cwd, paths, CwdCheck::AtLaunch)
        {
            loaded.diagnostics.push(ConfigDiagnostic::Path(error));
        }
        loaded
    }

    /// Load a config for an application launch. Every path or validation
    /// problem is fatal, so a default config from an unsuccessful parse is
    /// never returned to runtime callers.
    pub fn load_validated(paths: &AppPaths) -> Result<ValidatedConfig, Vec<ConfigDiagnostic>> {
        Self::load_for_check(paths).into_validated(paths.clone())
    }

    fn load_from_path_with_paths(path: &Path, paths: &AppPaths) -> LoadedConfig {
        match read_optional_config(path) {
            Ok(Some(content)) => Self::load_from_str_with_paths(&content, paths),
            Ok(None) => {
                let config = Self::default();
                let provenance = match ConfigProvenance::from_config(&config, None) {
                    Ok(provenance) => provenance,
                    Err(error) => {
                        return default_loaded_config(
                            vec![ConfigDiagnostic::Provenance(format!(
                                "config provenance error: {error}"
                            ))],
                            paths,
                        );
                    }
                };
                let resolution = super::validated::ConfigResolution::parse(
                    &config,
                    &provenance,
                    paths,
                    CwdCheck::AtLaunch,
                );
                let diagnostics = resolution
                    .diagnostics
                    .iter()
                    .cloned()
                    .map(ConfigDiagnostic::Validation)
                    .chain(
                        resolution
                            .path_diagnostics
                            .iter()
                            .cloned()
                            .map(ConfigDiagnostic::Path),
                    )
                    .collect();
                LoadedConfig {
                    provenance,
                    config,
                    resolution,
                    diagnostics,
                    document_state: ConfigDocumentState::Missing,
                    unavailable_new_cwd: None,
                }
            }
            Err(err) => default_loaded_config(
                vec![ConfigDiagnostic::Read(format!("config read error: {err}"))],
                paths,
            ),
        }
    }

    fn load_from_str_with_paths(content: &str, paths: &AppPaths) -> LoadedConfig {
        match content.parse::<toml::Table>() {
            Ok(table) => {
                let document = toml::Value::Table(table);
                match deserialize_with_ignored::<Config, _>(document.clone()) {
                    Ok((config, ignored_keys)) => {
                        let provenance =
                            match ConfigProvenance::from_config(&config, Some(&document)) {
                                Ok(provenance) => provenance,
                                Err(error) => {
                                    let mut loaded = default_loaded_config(
                                        vec![ConfigDiagnostic::Provenance(format!(
                                            "config provenance error: {error}"
                                        ))],
                                        paths,
                                    );
                                    loaded.unavailable_new_cwd =
                                        Some(config.terminal.new_cwd.clone());
                                    return loaded;
                                }
                            };
                        let resolution = super::validated::ConfigResolution::parse(
                            &config,
                            &provenance,
                            paths,
                            CwdCheck::AtLaunch,
                        );
                        let (unknown_sections, unknown_diagnostics) =
                            unknown_top_level_sections(&document, &ignored_keys);
                        let mut diagnostics = unknown_diagnostics
                            .into_iter()
                            .map(ConfigDiagnostic::Unknown)
                            .collect::<Vec<_>>();
                        diagnostics.extend(unknown_config_key_diagnostics(
                            ignored_keys
                                .into_iter()
                                .filter(|path| {
                                    !matches!(path.as_slice(), [ConfigKeyPathSegment::Key(key)] if unknown_sections.contains(key))
                                })
                                .collect(),
                        ).into_iter().map(ConfigDiagnostic::Unknown));
                        diagnostics.extend(
                            resolution
                                .diagnostics
                                .iter()
                                .cloned()
                                .map(ConfigDiagnostic::Validation),
                        );
                        diagnostics.extend(
                            resolution
                                .path_diagnostics
                                .iter()
                                .cloned()
                                .map(ConfigDiagnostic::Path),
                        );
                        LoadedConfig {
                            config,
                            provenance,
                            resolution,
                            diagnostics,
                            document_state: ConfigDocumentState::Loaded,
                            unavailable_new_cwd: None,
                        }
                    }
                    Err(err) => {
                        let mut loaded = default_loaded_config(
                            vec![ConfigDiagnostic::Parse(format!(
                                "config parse error: {err}"
                            ))],
                            paths,
                        );
                        loaded.unavailable_new_cwd = configured_new_cwd_for_check(&document);
                        loaded
                    }
                }
            }
            // Broken TOML has no typed document to project independent config
            // checks from; keep the parser diagnostic instead of interpreting
            // fragments of malformed source text.
            Err(err) => default_loaded_config(
                vec![ConfigDiagnostic::Parse(format!(
                    "config parse error: {err}"
                ))],
                paths,
            ),
        }
    }
}

#[cfg(test)]
impl Config {
    fn load_from_path(path: &Path) -> LoadedConfig {
        Self::load_from_path_with_paths(path, &AppPaths::default())
    }

    fn load_from_str(content: &str) -> LoadedConfig {
        Self::load_from_str_with_paths(content, &AppPaths::default())
    }
}

/// Parse the config for the launch-time inspection command and retain every
/// diagnostic without constructing a runtime configuration.
pub fn load_for_check(paths: &AppPaths) -> LoadedConfig {
    Config::load_for_check(paths)
}

/// Parse, resolve and validate the config for an application process.
pub fn load_validated(paths: &AppPaths) -> Result<ValidatedConfig, Vec<ConfigDiagnostic>> {
    Config::load_validated(paths)
}

fn default_loaded_config(diagnostics: Vec<ConfigDiagnostic>, paths: &AppPaths) -> LoadedConfig {
    let config = Config::default();
    let provenance = ConfigProvenance::defaults(&config);
    let resolution =
        super::validated::ConfigResolution::parse(&config, &provenance, paths, CwdCheck::AtLaunch);
    LoadedConfig {
        config,
        provenance,
        resolution,
        diagnostics,
        document_state: ConfigDocumentState::Unavailable,
        unavailable_new_cwd: None,
    }
}

#[derive(Deserialize)]
struct ConfiguredCwdPathCheck {
    terminal: Option<TerminalCwdPathCheck>,
}

#[derive(Deserialize)]
struct TerminalCwdPathCheck {
    new_cwd: Option<NewTerminalCwdConfig>,
}

fn configured_new_cwd_for_check(document: &toml::Value) -> Option<NewTerminalCwdConfig> {
    let projection: ConfiguredCwdPathCheck = document.clone().try_into().ok()?;
    projection.terminal?.new_cwd
}

fn unknown_top_level_sections(
    document: &toml::Value,
    ignored_paths: &[Vec<ConfigKeyPathSegment>],
) -> (std::collections::BTreeSet<String>, Vec<String>) {
    let Some(table) = document.as_table() else {
        return (std::collections::BTreeSet::new(), Vec::new());
    };
    let mut keys = Vec::new();
    let mut diagnostics = Vec::new();
    for path in ignored_paths {
        let [ConfigKeyPathSegment::Key(key)] = path.as_slice() else {
            continue;
        };
        let Some(value) = table.get(key) else {
            continue;
        };
        if let Some(diagnostic) = unknown_top_level_section_diagnostic(key, value) {
            keys.push(key.clone());
            diagnostics.push(diagnostic);
        }
    }
    (keys.into_iter().collect(), diagnostics)
}

fn unknown_top_level_section_diagnostic(key: &str, value: &toml::Value) -> Option<String> {
    let header = if value.is_table() {
        format!("[{key}]")
    } else if value
        .as_array()
        .is_some_and(|items| !items.is_empty() && items.iter().all(toml::Value::is_table))
    {
        format!("[[{key}]]")
    } else {
        return None;
    };

    Some(format!("unknown config section {header}"))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ConfigKeyPathSegment {
    Key(String),
    Index(usize),
}

fn config_key_path(path: &serde_ignored::Path<'_>) -> Vec<ConfigKeyPathSegment> {
    fn visit(path: &serde_ignored::Path<'_>, segments: &mut Vec<ConfigKeyPathSegment>) {
        match path {
            serde_ignored::Path::Root => {}
            serde_ignored::Path::Seq { parent, index } => {
                visit(parent, segments);
                segments.push(ConfigKeyPathSegment::Index(*index));
            }
            serde_ignored::Path::Map { parent, key } => {
                visit(parent, segments);
                segments.push(ConfigKeyPathSegment::Key(key.clone()));
            }
            serde_ignored::Path::Some { parent }
            | serde_ignored::Path::NewtypeStruct { parent }
            | serde_ignored::Path::NewtypeVariant { parent } => visit(parent, segments),
        }
    }

    let mut segments = Vec::new();
    visit(path, &mut segments);
    segments
}

fn format_config_key_path(path: &[ConfigKeyPathSegment]) -> String {
    path.iter()
        .map(|segment| match segment {
            ConfigKeyPathSegment::Key(key)
                if !key.is_empty()
                    && key.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
                    }) =>
            {
                key.clone()
            }
            ConfigKeyPathSegment::Key(key) => toml::Value::String(key.clone()).to_string(),
            ConfigKeyPathSegment::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn unknown_config_key_diagnostics(mut paths: Vec<Vec<ConfigKeyPathSegment>>) -> Vec<String> {
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| format!("unknown config key {}", format_config_key_path(&path)))
        .collect()
}

fn deserialize_with_ignored<'de, T, D>(
    deserializer: D,
) -> Result<(T, Vec<Vec<ConfigKeyPathSegment>>), D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let mut ignored = Vec::new();
    let value = serde_ignored::deserialize(deserializer, |path| {
        ignored.push(config_key_path(&path));
    })?;
    Ok((value, ignored))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_diagnostics_keep_their_kind() {
        let parse = Config::load_from_str("[keys\nprefix = 'ctrl+a'");
        assert!(matches!(
            parse.diagnostics.as_slice(),
            [ConfigDiagnostic::Parse(_)]
        ));

        let unknown = Config::load_from_str("[keys]\nunknown_binding = 'ctrl+a'");
        assert!(matches!(
            unknown.diagnostics.as_slice(),
            [ConfigDiagnostic::Unknown(_)]
        ));

        let invalid = Config::load_from_str("[keys]\nprefix = 'ctrl+'");
        assert!(
            invalid
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic, ConfigDiagnostic::Validation(_)))
        );
    }

    #[test]
    fn config_load_reports_unreadable_path() {
        // A directory where the config file should be cannot be read.
        let scratch = shepr_test_support::ScratchDir::new("config");
        let startup = Config::load_from_path(scratch.path());
        assert!(
            startup
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message().contains("config read error"))
        );
    }

    #[test]
    fn validated_config_rejects_parse_and_semantic_errors() {
        for (content, message) in [
            ("[keys]\nprefix = \"ctrl+\"\n", "keys.prefix"),
            ("[keys]\nzoom = \"prefix+nonsense\"\n", "keys.zoom"),
            ("[theme]\nname = \"not-a-theme\"\n", "theme.name"),
            (
                "[theme.custom]\nred = \"not-a-color\"\n",
                "theme.custom.red",
            ),
            ("[ui]\naccent = \"not-a-color\"\n", "ui.accent"),
            ("[ui]\nwindow_title = \"{unknown}\"\n", "ui.window_title"),
            (
                "[ui]\nsidebar_min_width = 50\nsidebar_max_width = 30\n",
                "sidebar_min_width",
            ),
            ("[server]\nheadless_cols = 0\n", "headless_cols"),
            (
                "[ui]\ntab_bar_right = [{ type = \"datetime\", format = \"%Q\" }]\n",
                "ui.tab_bar_right[0]",
            ),
            (
                "[ui]\nmouse_captur = true\n",
                "unknown config key ui.mouse_captur",
            ),
        ] {
            let loaded = Config::load_from_str(content);
            let errors = loaded
                .into_validated(AppPaths::default())
                .expect_err("invalid config must not be returned for launch");
            assert!(
                errors
                    .iter()
                    .any(|diagnostic| diagnostic.message().contains(message)),
                "expected {message:?} in {errors:?}"
            );
        }

        let parse_error = Config::load_from_str("[server]\nheadless_cols = \"wide\"\n");
        assert!(parse_error.into_validated(AppPaths::default()).is_err());
    }

    #[test]
    fn config_check_collects_all_semantic_diagnostics() {
        let scratch = shepr_test_support::ScratchDir::new("config-diagnostics");
        let paths = AppPaths::test_at(scratch.path());
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(
            paths.config_file(),
            r#"
[theme]
name = "not-a-theme"
[theme.custom]
red = "not-a-color"
[server]
headless_cols = 0
[keys]
prefix = "ctrl+"
zoom = "prefix+not-a-key"
[ui]
sidebar_width = 80
sidebar_min_width = 50
sidebar_max_width = 30
window_title = "{unknown}"
tab_bar_right = [
  { type = "datetime", format = "%Q" },
  { type = "command", command = "", interval_seconds = 0, timeout_seconds = 0 },
]
"#,
        )
        .expect("write invalid config fixture");

        let report = Config::load_for_check(&paths);
        let messages = report
            .diagnostics
            .iter()
            .map(ConfigDiagnostic::message)
            .collect::<Vec<_>>();
        for expected in [
            "theme.name",
            "theme.custom.red",
            "server.headless_cols",
            "keys.prefix",
            "keys.zoom",
            "sidebar_min_width",
            "ui.window_title",
            "ui.tab_bar_right[0]",
            "ui.tab_bar_right[1] command",
            "ui.tab_bar_right[1] interval_seconds",
            "ui.tab_bar_right[1] timeout_seconds",
        ] {
            assert!(
                messages.iter().any(|message| message.contains(expected)),
                "missing {expected:?} from {messages:?}"
            );
        }
    }

    #[test]
    fn config_check_reports_home_path_when_another_field_fails_to_parse() {
        let scratch = shepr_test_support::ScratchDir::new("config-parse-diagnostics");
        let paths = AppPaths::test_at(scratch.path());
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(
            paths.config_file(),
            "[terminal]\nnew_cwd = \"home\"\n[server]\nheadless_cols = \"wide\"\n",
        )
        .expect("write config fixture");

        let report = Config::load_for_check(&paths);
        let messages = report
            .diagnostics
            .iter()
            .map(ConfigDiagnostic::message)
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(
            messages
                .iter()
                .any(|message| message.contains("parse error")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("terminal.new_cwd")),
            "{messages:?}"
        );
    }

    #[test]
    fn load_validated_rejects_bad_config_file_and_accepts_missing_file() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("config-load");
        let paths = AppPaths::test_at(scratch.path());
        let path = paths.config_file();
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");

        std::fs::write(path, "[server]\nheadless_rows = 0\n").expect("write bad config fixture");
        assert!(Config::load_validated(&paths).is_err());

        std::fs::remove_file(path).expect("remove config fixture");
        let defaults = Config::load_validated(&paths).expect("missing config uses defaults");
        assert!(defaults.validated_live_keybinds().is_ok());
        assert_eq!(defaults.palette(), &crate::theme::Palette::catppuccin());

        std::fs::write(
            path,
            "[theme]\nname = \"nord\"\n[theme.custom]\naccent = \"#010203\"\n",
        )
        .expect("write valid themed config");
        let themed = Config::load_validated(&paths).expect("valid theme loads");
        assert_eq!(themed.palette().accent, ratatui::style::Color::Rgb(1, 2, 3));
    }

    #[test]
    fn load_validated_rejects_home_cwd_without_absolute_home() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("config-cwd");
        // Launch paths without a home directory: what resolution captures
        // when HOME is missing or relative.
        let paths = AppPaths::test_at(scratch.path());
        assert!(paths.home_dir().is_none());
        let path = paths.config_file();
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(path, "[terminal]\nnew_cwd = \"home\"\n").expect("write config fixture");

        let report = Config::load_for_check(&paths);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|error| error.message().contains("terminal.new_cwd")),
            "{:?}",
            report.diagnostics
        );
        let errors = Config::load_validated(&paths).expect_err("home cwd needs absolute HOME");
        assert!(
            errors
                .iter()
                .any(|error| error.message().contains("terminal.new_cwd")),
            "{errors:?}"
        );
    }

    #[test]
    fn config_check_reports_cwd_path_when_another_value_fails_to_parse() {
        let scratch = shepr_test_support::ScratchDir::new("config-check-cwd-path");
        let paths =
            AppPaths::test_with_context(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(
            paths.config_file(),
            "[terminal]\nnew_cwd = \"missing-directory\"\n[server]\nheadless_cols = \"wide\"\n",
        )
        .expect("write config fixture");

        let report = Config::load_for_check(&paths);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message().contains("config parse error")),
            "missing parse diagnostic: {:?}",
            report.diagnostics
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message().contains("terminal.new_cwd")),
            "missing cwd diagnostic: {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn config_path_honours_the_override_variable() {
        let env = shepr_test_support::IsolatedEnv::new();
        assert_eq!(
            AppPaths::resolve()
                .expect("default paths resolve")
                .config_file(),
            env.home()
                .join(".config")
                .join(app_dir_name())
                .join("config.toml")
                .as_path()
        );
        let custom = env.path().join("custom.toml");
        env.set(EnvVar::SheprConfigPath, &custom);
        assert_eq!(
            AppPaths::resolve()
                .expect("override resolves")
                .config_file(),
            custom
        );

        // Empty is unset: the default config file.
        env.set(EnvVar::SheprConfigPath, "");
        let paths = AppPaths::resolve().expect("an empty override reads as unset");
        assert_eq!(
            paths.config_file(),
            env.home()
                .join(".config")
                .join(app_dir_name())
                .join("config.toml")
                .as_path()
        );
        assert_eq!(paths.provenance().config_file, ConfigSource::Default);

        env.set(EnvVar::SheprConfigPath, format!("{} ", custom.display()));
        let errors = AppPaths::resolve().expect_err("a padded override is refused");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("SHEPR_CONFIG_PATH") && error.contains("whitespace")),
            "{errors:?}"
        );
    }

    #[test]
    fn socket_path_provenance_is_tracked_independently() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.remove(EnvVar::SheprSocketPath);
        env.remove(EnvVar::SheprClientSocketPath);

        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        let paths = AppPaths::resolve().expect("API socket override resolves");
        assert_eq!(
            paths.provenance().api_socket,
            ConfigSource::EnvironmentVariable(EnvVar::SheprSocketPath.name().to_owned())
        );
        assert_eq!(
            paths.provenance().client_socket,
            ConfigSource::EnvironmentVariable(EnvVar::SheprSocketPath.name().to_owned())
        );

        env.remove(EnvVar::SheprSocketPath);
        env.set(
            EnvVar::SheprClientSocketPath,
            env.path().join("client.sock"),
        );
        let paths = AppPaths::resolve().expect("client socket override resolves");
        assert_eq!(
            paths.provenance().api_socket,
            ConfigSource::EnvironmentVariable("XDG_RUNTIME_DIR".to_owned())
        );
        assert_eq!(
            paths.provenance().client_socket,
            ConfigSource::EnvironmentVariable(EnvVar::SheprClientSocketPath.name().to_owned())
        );
    }

    #[test]
    fn invalid_socket_and_session_environment_fails_resolution() {
        let env = shepr_test_support::IsolatedEnv::new();
        for variable in [EnvVar::SheprSocketPath, EnvVar::SheprClientSocketPath] {
            for (value, expected) in [
                ("", "set but empty"),
                ("rel.sock", "absolute path"),
                (" /abs.sock", "whitespace"),
            ] {
                env.set(variable, value);
                let errors = AppPaths::resolve().expect_err("invalid socket override");
                assert!(
                    errors
                        .iter()
                        .any(|error| error.contains(variable.name()) && error.contains(expected)),
                    "{variable}={value:?}: {errors:?}"
                );
                // Remote commands never consult the local target environment.
                assert!(AppPaths::resolve_for_machine().is_ok());
            }
            env.remove(variable);
        }

        // An empty inherited session is refused rather than read as default.
        env.set(EnvVar::SheprSession, "");
        let errors = AppPaths::resolve().expect_err("empty SHEPR_SESSION");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("SHEPR_SESSION") && error.contains("set but empty")),
            "{errors:?}"
        );

        // A socket override does not excuse a malformed inherited session.
        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        env.set(EnvVar::SheprSession, "bad/name");
        let errors = AppPaths::resolve().expect_err("malformed SHEPR_SESSION");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("session selection error")),
            "{errors:?}"
        );
    }

    #[test]
    fn path_provenance_distinguishes_cli_and_internal_session_selection() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "inherited");

        let cli = AppPaths::resolve_with_session(Some(crate::SessionId::Default))
            .expect("CLI session paths resolve");
        assert_eq!(
            cli.provenance().session_id,
            ConfigSource::CliFlag("--session".to_owned())
        );

        let machine = AppPaths::resolve_for_machine().expect("machine paths resolve");
        assert_eq!(machine.provenance().session_id, ConfigSource::Default);
    }

    #[test]
    fn config_load_reports_unknown_keys_and_parses_known_siblings() {
        let loaded = Config::load_from_str(
            r##"
plugin = []

[theme.custom]
accentt = "#ffffff"

[advanced]
scrollback_limit_bytes = 42

[keys]
zoom = "prefix+z"
new_tabb = "prefix+t"

[ui]
mouse_capture = false
mouse_captur = true
"foo.bar" = true
"##,
        );

        assert_eq!(
            loaded
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec![
                "unknown config key keys.new_tabb",
                "unknown config key plugin",
                "unknown config key theme.custom.accentt",
                "unknown config key ui.\"foo.bar\"",
                "unknown config key ui.mouse_captur",
            ]
        );
        assert_eq!(loaded.config.advanced.scrollback_limit_bytes, 42);
        assert!(!loaded.config.ui.mouse_capture);
    }

    #[test]
    fn config_load_records_provenance_for_ui_values() {
        let loaded = Config::load_from_str(
            r#"
[ui]
sidebar_width = 26
agent_panel_sort = "priority"
"#,
        );
        assert!(loaded.resolution.values.is_some());
        assert!(
            loaded
                .provenance
                .is_explicit(super::super::UiPreferenceKey::SidebarWidth)
        );
        assert!(
            loaded
                .provenance
                .is_explicit(super::super::UiPreferenceKey::AgentPanelSort)
        );
        assert!(
            !loaded
                .provenance
                .is_explicit(super::super::UiPreferenceKey::SidebarStartCollapsed)
        );

        let empty = Config::load_from_str("[terminal]\n");
        assert!(
            !empty
                .provenance
                .is_explicit(super::super::UiPreferenceKey::SidebarWidth)
        );
    }

    #[test]
    fn config_provenance_queries_array_fields_by_their_parent_key() {
        let configured =
            Config::load_from_str("[keys]\nfocus_agent = [\"prefix+1\", \"prefix+2\"]\n");
        assert!(configured.provenance.key_is_configured("keys.focus_agent"));

        let defaults = Config::load_from_str("[keys]\n");
        assert!(!defaults.provenance.key_is_configured("keys.focus_agent"));
    }

    #[test]
    fn config_load_reports_unknown_top_level_sections() {
        let loaded = Config::load_from_str(
            r#"
[[plugin]]
id = "example"
"#,
        );

        assert_eq!(
            loaded
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["unknown config section [[plugin]]"]
        );
    }

    #[test]
    fn xdg_paths_use_separate_roots_ignore_empty_and_refuse_relative_base_dirs() {
        let env = shepr_test_support::IsolatedEnv::new();
        let paths = AppPaths::resolve().expect("default paths resolve");
        assert_eq!(
            paths.config_dir(),
            env.home().join(".config").join(app_dir_name())
        );
        assert_eq!(
            paths.state_dir(),
            env.home().join(".local/state").join(app_dir_name())
        );
        assert_eq!(paths.xdg_runtime_dir(), env.path().join("runtime"));
        assert_eq!(
            paths.runtime_dir(),
            env.path().join("runtime").join(app_dir_name())
        );

        for (key, suffix) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            env.set(key, "");
            let paths = AppPaths::resolve().expect("an empty XDG base reads as unset");
            let expected = env.home().join(suffix).join(app_dir_name());
            let actual = if key == "XDG_CONFIG_HOME" {
                paths.config_dir()
            } else {
                paths.state_dir()
            };
            assert_eq!(actual, expected, "{key} empty");
            for refused in ["relative/path", " /padded"] {
                env.set(key, refused);
                let errors = AppPaths::resolve().expect_err("an invalid XDG base is refused");
                assert!(
                    errors.iter().any(|error| error.contains(key)),
                    "{key}={refused:?}: {errors:?}"
                );
            }
            env.set(key, env.path().join(key));
            let paths = AppPaths::resolve().expect("absolute XDG base is accepted");
            let expected = env.path().join(key).join(app_dir_name());
            let actual = if key == "XDG_CONFIG_HOME" {
                paths.config_dir()
            } else {
                paths.state_dir()
            };
            assert_eq!(actual, expected);
            env.remove(key);
        }

        for (invalid, expected) in [
            ("", "XDG_RUNTIME_DIR must be set"),
            ("relative/path", "relative path"),
        ] {
            env.set("XDG_RUNTIME_DIR", invalid);
            let errors = AppPaths::resolve().expect_err("runtime dir has no XDG default");
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("XDG_RUNTIME_DIR") && error.contains(expected)),
                "XDG_RUNTIME_DIR={invalid:?}: {errors:?}"
            );
        }
        env.remove("XDG_RUNTIME_DIR");
        assert!(AppPaths::resolve().is_err());
        for invalid in ["", "relative/home"] {
            env.set("HOME", invalid);
            assert!(AppPaths::resolve().is_err());
        }
        env.remove("HOME");
        assert!(AppPaths::resolve().is_err());
    }

    #[test]
    fn app_paths_wire_deserialization_rejects_unresolved_paths() {
        let scratch = shepr_test_support::ScratchDir::new("app-paths-wire");
        let paths = AppPaths::test_with_context(scratch.path(), Some(scratch.path()), None);
        let wire = serde_json::to_value(&paths).expect("serialize test paths");
        assert!(serde_json::from_value::<AppPaths>(wire.clone()).is_ok());

        for field in ["config_dir", "state_dir", "runtime_dir", "config_file"] {
            let mut invalid = wire.clone();
            invalid[field] = serde_json::json!("relative/path");
            assert!(
                serde_json::from_value::<AppPaths>(invalid).is_err(),
                "accepted relative {field}"
            );
        }

        let mut invalid_home = wire.clone();
        invalid_home["home_dir"] = serde_json::json!("relative/home");
        assert!(serde_json::from_value::<AppPaths>(invalid_home).is_err());

        let mut missing_home = wire.clone();
        missing_home["home_dir"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<AppPaths>(missing_home).is_err());

        let mut invalid_current_dir = wire.clone();
        invalid_current_dir["current_dir"] = serde_json::json!("relative/current");
        assert!(serde_json::from_value::<AppPaths>(invalid_current_dir).is_err());

        let mut invalid_socket = wire;
        invalid_socket["server_address"]["client_socket"] =
            serde_json::json!("relative/client.sock");
        assert!(serde_json::from_value::<AppPaths>(invalid_socket).is_err());
    }

    #[test]
    fn normalize_utf8_bom_removes_a_leading_bom() {
        let content = "\u{feff}[terminal]\n";
        assert_eq!(normalize_utf8_bom(content), "[terminal]\n");
    }

    #[test]
    fn normalize_utf8_bom_recovers_from_a_displaced_mid_file_bom() {
        let content = "[ui]\n\u{feff}[terminal]\ndefault_shell = \"zsh\"\n";
        let normalized = normalize_utf8_bom(content);
        assert_eq!(normalized, "[ui]\n[terminal]\ndefault_shell = \"zsh\"\n");
        assert!(normalized.parse::<toml::Table>().is_ok());
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_basic_strings() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\n";
        assert!(content.parse::<toml::Table>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_literal_strings() {
        let content = "[theme]\nname = '''\nfirst\n\u{feff}second\n'''\n";
        assert!(content.parse::<toml::Table>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_string_boms_despite_other_errors() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\nbroken = \n";
        assert!(content.parse::<toml::Table>().is_err());
        assert_eq!(normalize_utf8_bom(content), content);
    }
}
