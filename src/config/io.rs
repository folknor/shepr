use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    CONFIG_PATH_ENV_VAR, Config, ConfigProvenance, ConfigSource, NewTerminalCwdConfig,
    ValidatedConfig,
    model::{ConfigDocumentState, LoadedConfig},
};

pub fn app_dir_name() -> &'static str {
    // Unit tests get a directory name of their own in every profile. `brokkr
    // test` builds release, where the name would otherwise be the installed
    // `shepr`, so a test that resolved a config or state path without
    // isolating `HOME` (`test_support::IsolatedEnv`) would land in the real
    // `~/.config/shepr`. This keeps such a slip out of both the release and
    // the dev directory.
    if cfg!(test) {
        "shepr-test"
    } else if cfg!(debug_assertions) {
        "shepr-dev"
    } else {
        "shepr"
    }
}

/// Paths and the local target resolved once at the process boundary and
/// passed to consumers. Production constructors reject unresolved path inputs
/// that would put files relative to the working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(Default))]
pub struct AppPaths {
    config_dir: PathBuf,
    state_dir: PathBuf,
    config_file: PathBuf,
    home_dir: Option<PathBuf>,
    current_dir: Option<PathBuf>,
    session_id: crate::session::SessionId,
    server_address: crate::server::socket_paths::ServerAddress,
    provenance: PathProvenance,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathProvenance {
    pub config_dir: ConfigSource,
    pub state_dir: ConfigSource,
    pub config_file: ConfigSource,
    pub home_dir: ConfigSource,
    pub current_dir: ConfigSource,
    pub session_id: ConfigSource,
    pub api_socket: ConfigSource,
    pub client_socket: ConfigSource,
}

impl AppPaths {
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
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

    pub fn session_id(&self) -> &crate::session::SessionId {
        &self.session_id
    }

    pub fn server_address(&self) -> &crate::server::socket_paths::ServerAddress {
        &self.server_address
    }

    pub fn provenance(&self) -> &PathProvenance {
        &self.provenance
    }

    /// Resolve XDG directories, session identity and socket target once from
    /// the inherited process environment.
    pub fn resolve() -> Result<Self, Vec<String>> {
        resolve_paths_from_env(None, None)
    }

    /// Resolve paths and the local session/socket target from one environment
    /// snapshot. `Some(Default)` represents an explicit request for the
    /// default session and therefore takes precedence over socket overrides.
    pub fn resolve_with_session(
        requested_session: Option<crate::session::SessionId>,
    ) -> Result<Self, Vec<String>> {
        let session_source = requested_session
            .as_ref()
            .map(|_| ConfigSource::CliFlag("--session".to_owned()));
        resolve_paths_from_env(requested_session, session_source)
    }

    /// Resolve only the machine catalog's local paths, without allowing local
    /// session or socket environment values to affect a remote command.
    pub(crate) fn resolve_for_machine() -> Result<Self, Vec<String>> {
        resolve_paths_from_env(Some(crate::session::SessionId::Default), None)
    }

    #[cfg(test)]
    pub(crate) fn test_at(root: &Path) -> Self {
        Self::test_with_context(root, None, None)
    }

    #[cfg(test)]
    pub(crate) fn test_with_context(
        root: &Path,
        home_dir: Option<&Path>,
        current_dir: Option<&Path>,
    ) -> Self {
        Self {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            config_file: root.join("config/config.toml"),
            home_dir: home_dir.map(Path::to_path_buf),
            current_dir: current_dir.map(Path::to_path_buf),
            session_id: crate::session::SessionId::Default,
            server_address: crate::server::socket_paths::ServerAddress::resolve(
                &root.join("config"),
                &crate::session::SessionId::Default,
                false,
                None,
                None,
            ),
            provenance: PathProvenance {
                config_dir: ConfigSource::Default,
                state_dir: ConfigSource::Default,
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

fn platform_xdg_dir(
    variable: &str,
    home_suffix: &str,
    home_dir: Option<&Path>,
) -> io::Result<(PathBuf, ConfigSource)> {
    if let Some(value) = std::env::var_os(variable) {
        let directory = PathBuf::from(value);
        // The XDG base directory specification says to ignore empty and
        // relative values. In that case, use the corresponding location under
        // HOME, which must itself be an absolute path.
        if directory.is_absolute() {
            return Ok((
                directory.join(app_dir_name()),
                ConfigSource::EnvironmentVariable(variable.to_owned()),
            ));
        }
    }

    let home_dir = home_dir.ok_or_else(|| {
        io::Error::other("HOME must be set to a non-empty absolute path to locate home directory")
    })?;
    Ok((
        home_dir.join(home_suffix).join(app_dir_name()),
        ConfigSource::Default,
    ))
}

fn resolve_paths_from_env(
    requested_session: Option<crate::session::SessionId>,
    requested_session_source: Option<ConfigSource>,
) -> Result<AppPaths, Vec<String>> {
    let session_selection_was_forced = requested_session.is_some();
    let api_socket_override = std::env::var(crate::api::SOCKET_PATH_ENV_VAR).ok();
    let client_socket_override =
        std::env::var(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR).ok();
    let inherited_session = std::env::var(crate::session::SESSION_ENV_VAR).ok();
    let inherited_session_accepted = inherited_session
        .as_deref()
        .is_some_and(|name| crate::session::SessionId::parse(name).is_ok());
    let (session_id, session_was_requested) = crate::session::SessionId::resolve(
        requested_session,
        inherited_session.as_deref(),
        api_socket_override.is_some(),
    )
    .map_err(|error| vec![format!("session selection error: {error}")])?;

    let home_dir = crate::pathutil::home_dir().ok();
    let current_dir = std::env::current_dir().ok();
    let (config_dir, config_dir_source) =
        platform_xdg_dir("XDG_CONFIG_HOME", ".config", home_dir.as_deref())
            .map(|(path, source)| (Ok(path), source))
            .unwrap_or_else(|error| (Err(error), ConfigSource::Default));
    let (state_dir, state_dir_source) =
        platform_xdg_dir("XDG_STATE_HOME", ".local/state", home_dir.as_deref())
            .map(|(path, source)| (Ok(path), source))
            .unwrap_or_else(|error| (Err(error), ConfigSource::Default));

    let config_path_override = std::env::var_os(CONFIG_PATH_ENV_VAR);
    let config_file_source = if config_path_override.is_some() {
        ConfigSource::EnvironmentVariable(CONFIG_PATH_ENV_VAR.to_owned())
    } else {
        config_dir_source.clone()
    };
    let config_file = match config_path_override {
        Some(path) if path.is_empty() => Err(io::Error::other(format!(
            "{CONFIG_PATH_ENV_VAR} must not be empty"
        ))),
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                Ok(path)
            } else if let Some(current_dir) = current_dir.as_ref() {
                Ok(current_dir.join(path))
            } else {
                Err(io::Error::other(format!(
                    "cannot resolve relative {CONFIG_PATH_ENV_VAR} without a current directory"
                )))
            }
        }
        None => config_dir
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

    match (config_dir, state_dir, config_file) {
        (Some(config_dir), Some(state_dir), Some(config_file)) if diagnostics.is_empty() => {
            let server_address = crate::server::socket_paths::ServerAddress::resolve(
                &config_dir,
                &session_id,
                session_was_requested,
                api_socket_override.as_deref(),
                client_socket_override.as_deref(),
            );
            let home_dir_source = if home_dir.is_some() {
                ConfigSource::EnvironmentVariable("HOME".to_owned())
            } else {
                ConfigSource::Default
            };
            let session_source = if let Some(source) = requested_session_source {
                source
            } else if inherited_session_accepted && !session_selection_was_forced {
                ConfigSource::EnvironmentVariable(crate::session::SESSION_ENV_VAR.to_owned())
            } else {
                ConfigSource::Default
            };
            let api_socket_source = if session_selection_was_forced {
                session_source.clone()
            } else if api_socket_override.is_some() {
                ConfigSource::EnvironmentVariable(crate::api::SOCKET_PATH_ENV_VAR.to_owned())
            } else if inherited_session_accepted {
                session_source.clone()
            } else {
                config_dir_source.clone()
            };
            let client_socket_source = if session_selection_was_forced {
                session_source.clone()
            } else if api_socket_override.is_some() {
                ConfigSource::EnvironmentVariable(crate::api::SOCKET_PATH_ENV_VAR.to_owned())
            } else if client_socket_override.is_some() {
                ConfigSource::EnvironmentVariable(
                    crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR.to_owned(),
                )
            } else if inherited_session_accepted {
                session_source.clone()
            } else {
                config_dir_source.clone()
            };
            Ok(AppPaths {
                config_dir,
                state_dir,
                config_file,
                home_dir,
                current_dir,
                session_id,
                server_address,
                provenance: PathProvenance {
                    config_dir: config_dir_source,
                    state_dir: state_dir_source,
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
        let mut loaded = Self::load_from_path(paths.config_file());
        let config_unavailable = loaded.document_state == ConfigDocumentState::Unavailable;
        if !config_unavailable
            && let Some(error) = configured_home_path_error(&loaded.config, paths.home_dir())
        {
            loaded.diagnostics.push(error);
        }
        loaded
    }

    /// Load a config for an application launch. Every path or validation
    /// problem is fatal, so a default config from an unsuccessful parse is
    /// never returned to runtime callers.
    pub fn load_validated(paths: &AppPaths) -> Result<ValidatedConfig, Vec<String>> {
        Self::load_for_check(paths).into_validated(paths.clone())
    }

    fn load_from_path(path: &Path) -> LoadedConfig {
        match read_optional_config(path) {
            Ok(Some(content)) => Self::load_from_str(&content),
            Ok(None) => {
                let config = Self::default();
                let provenance = ConfigProvenance::defaults(&config);
                let keybind_validation = config.compute_keybind_validation(|_| false);
                LoadedConfig {
                    provenance,
                    config,
                    keybind_validation,
                    diagnostics: Vec::new(),
                    document_state: ConfigDocumentState::Missing,
                }
            }
            Err(err) => default_loaded_config(vec![format!("config read error: {err}")]),
        }
    }

    fn load_from_str(content: &str) -> LoadedConfig {
        match content.parse::<toml::Table>() {
            Ok(table) => {
                let document = toml::Value::Table(table);
                match deserialize_with_ignored::<Config, _>(document.clone()) {
                    Ok((config, ignored_keys)) => {
                        let provenance =
                            match ConfigProvenance::from_config(&config, Some(&document)) {
                                Ok(provenance) => provenance,
                                Err(error) => {
                                    return default_loaded_config(vec![format!(
                                        "config provenance error: {error}"
                                    )]);
                                }
                            };
                        let keybind_validation = config.compute_keybind_validation(|field| {
                            provenance.key_is_configured(&format!("keys.{field}"))
                        });
                        let (unknown_sections, mut diagnostics) =
                            unknown_top_level_sections(&document, &ignored_keys);
                        diagnostics.extend(unknown_config_key_diagnostics(
                            ignored_keys
                                .into_iter()
                                .filter(|path| {
                                    !matches!(path.as_slice(), [ConfigKeyPathSegment::Key(key)] if unknown_sections.contains(key))
                                })
                                .collect(),
                        ));
                        diagnostics.extend(
                            config.collect_diagnostics_with_keybind_validation(&keybind_validation),
                        );
                        LoadedConfig {
                            config,
                            provenance,
                            keybind_validation,
                            diagnostics,
                            document_state: ConfigDocumentState::Loaded,
                        }
                    }
                    Err(err) => default_loaded_config(vec![format!("config parse error: {err}")]),
                }
            }
            Err(err) => default_loaded_config(vec![format!("config parse error: {err}")]),
        }
    }
}

fn default_loaded_config(diagnostics: Vec<String>) -> LoadedConfig {
    let config = Config::default();
    let provenance = ConfigProvenance::defaults(&config);
    let keybind_validation = config.compute_keybind_validation(|_| false);
    LoadedConfig {
        config,
        provenance,
        keybind_validation,
        diagnostics,
        document_state: ConfigDocumentState::Unavailable,
    }
}

fn configured_home_path_error(config: &Config, home_dir: Option<&Path>) -> Option<String> {
    let result = match &config.terminal.new_cwd {
        NewTerminalCwdConfig::Home => home_dir
            .map(Path::to_path_buf)
            .ok_or_else(crate::pathutil::missing_home_error),
        NewTerminalCwdConfig::Path(path) if path == "~" || path.starts_with("~/") => {
            crate::pathutil::expand_tilde_path_with_home(path, home_dir)
        }
        NewTerminalCwdConfig::Follow
        | NewTerminalCwdConfig::Current
        | NewTerminalCwdConfig::Path(_) => return None,
    };
    result
        .err()
        .map(|err| format!("terminal.new_cwd cannot be resolved: {err}"))
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
    use std::ffi::OsString;

    #[test]
    fn config_load_reports_unreadable_path() {
        // A directory where the config file should be cannot be read.
        let scratch = crate::test_support::ScratchDir::new("config");
        let startup = Config::load_from_path(scratch.path());
        assert!(
            startup
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("config read error"))
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
                errors.iter().any(|diagnostic| diagnostic.contains(message)),
                "expected {message:?} in {errors:?}"
            );
        }

        let parse_error = Config::load_from_str("[server]\nheadless_cols = \"wide\"\n");
        assert!(parse_error.into_validated(AppPaths::default()).is_err());
    }

    #[test]
    fn load_validated_rejects_bad_config_file_and_accepts_missing_file() {
        let _env = crate::test_support::IsolatedEnv::new();
        let scratch = crate::test_support::ScratchDir::new("config-load");
        let paths = AppPaths::test_at(scratch.path());
        let path = paths.config_file();
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");

        std::fs::write(path, "[server]\nheadless_rows = 0\n").expect("write bad config fixture");
        assert!(Config::load_validated(&paths).is_err());

        std::fs::remove_file(path).expect("remove config fixture");
        let defaults = Config::load_validated(&paths).expect("missing config uses defaults");
        assert!(defaults.validated_live_keybinds().is_ok());
        assert_eq!(
            defaults.palette(),
            &crate::app::state::Palette::catppuccin()
        );

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
        let _env = crate::test_support::IsolatedEnv::new();
        let scratch = crate::test_support::ScratchDir::new("config-cwd");
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
                .any(|error| error.contains("terminal.new_cwd")),
            "{:?}",
            report.diagnostics
        );
        let errors = Config::load_validated(&paths).expect_err("home cwd needs absolute HOME");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("terminal.new_cwd")),
            "{errors:?}"
        );
    }

    #[test]
    fn config_path_honours_the_override_variable() {
        let env = crate::test_support::IsolatedEnv::new();
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
        env.set(CONFIG_PATH_ENV_VAR, &custom);
        assert_eq!(
            AppPaths::resolve()
                .expect("override resolves")
                .config_file(),
            custom
        );

        env.set(CONFIG_PATH_ENV_VAR, "");
        assert!(AppPaths::resolve().is_err());
    }

    #[test]
    fn socket_path_provenance_is_tracked_independently() {
        let env = crate::test_support::IsolatedEnv::new();
        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
        env.remove(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR);

        env.set(crate::api::SOCKET_PATH_ENV_VAR, env.path().join("api.sock"));
        let paths = AppPaths::resolve().expect("API socket override resolves");
        assert_eq!(
            paths.provenance().api_socket,
            ConfigSource::EnvironmentVariable(crate::api::SOCKET_PATH_ENV_VAR.to_owned())
        );
        assert_eq!(
            paths.provenance().client_socket,
            ConfigSource::EnvironmentVariable(crate::api::SOCKET_PATH_ENV_VAR.to_owned())
        );

        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
        env.set(
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
            env.path().join("client.sock"),
        );
        let paths = AppPaths::resolve().expect("client socket override resolves");
        assert_eq!(paths.provenance().api_socket, ConfigSource::Default);
        assert_eq!(
            paths.provenance().client_socket,
            ConfigSource::EnvironmentVariable(
                crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR.to_owned()
            )
        );
    }

    #[test]
    fn path_provenance_distinguishes_cli_and_internal_session_selection() {
        let env = crate::test_support::IsolatedEnv::new();
        env.set(crate::session::SESSION_ENV_VAR, "inherited");

        let cli = AppPaths::resolve_with_session(Some(crate::session::SessionId::Default))
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
            loaded.diagnostics,
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
        assert!(loaded.keybind_validation.prefix_diag.is_none());
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
            loaded.diagnostics,
            vec!["unknown config section [[plugin]]"]
        );
    }

    #[test]
    fn xdg_paths_ignore_empty_or_relative_values_and_require_absolute_home() {
        let env = crate::test_support::IsolatedEnv::new();
        let expected_config = env.home().join(".config").join(app_dir_name());
        let expected_state = env.home().join(".local/state").join(app_dir_name());

        for invalid in [OsString::new(), OsString::from("relative/config")] {
            env.set("XDG_CONFIG_HOME", &invalid);
            env.set("XDG_STATE_HOME", &invalid);
            let paths = AppPaths::resolve().expect("home fallback");
            assert_eq!(paths.config_dir(), expected_config);
            assert_eq!(paths.state_dir(), expected_state);
        }

        let xdg_config = env.path().join("xdg-config");
        let xdg_state = env.path().join("xdg-state");
        env.set("XDG_CONFIG_HOME", &xdg_config);
        env.set("XDG_STATE_HOME", &xdg_state);
        env.set("HOME", "relative/home");
        let paths = AppPaths::resolve().expect("absolute XDG paths");
        assert_eq!(paths.config_dir(), xdg_config.join(app_dir_name()));
        assert_eq!(paths.state_dir(), xdg_state.join(app_dir_name()));

        env.remove("XDG_CONFIG_HOME");
        env.remove("XDG_STATE_HOME");
        assert!(AppPaths::resolve().is_err());
        env.set("HOME", "");
        assert!(AppPaths::resolve().is_err());
        env.remove("HOME");
        assert!(AppPaths::resolve().is_err());
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
