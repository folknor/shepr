use std::io;
use std::path::{Path, PathBuf};

use super::{CONFIG_PATH_ENV_VAR, Config, NewTerminalCwdConfig, model::LoadedConfig};

const KNOWN_TOP_LEVEL_CONFIG_KEYS: &[&str] = &[
    "advanced",
    "experimental",
    "keys",
    "remote",
    "server",
    "session",
    "terminal",
    "theme",
    "ui",
];

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

pub fn config_dir() -> PathBuf {
    resolve_or_exit("config directory", try_config_dir())
}

pub fn state_dir() -> PathBuf {
    resolve_or_exit("state directory", try_state_dir())
}

pub fn try_config_dir() -> io::Result<PathBuf> {
    platform_xdg_dir("XDG_CONFIG_HOME", ".config")
}

pub fn try_state_dir() -> io::Result<PathBuf> {
    platform_xdg_dir("XDG_STATE_HOME", ".local/state")
}

fn platform_xdg_dir(variable: &str, home_suffix: &str) -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os(variable) {
        let directory = PathBuf::from(value);
        // The XDG base directory specification says to ignore empty and
        // relative values. In that case, use the corresponding location under
        // HOME, which must itself be an absolute path.
        if directory.is_absolute() {
            return Ok(directory.join(app_dir_name()));
        }
    }

    Ok(crate::pathutil::home_dir()?
        .join(home_suffix)
        .join(app_dir_name()))
}

fn resolve_or_exit(description: &str, result: io::Result<PathBuf>) -> PathBuf {
    match result {
        Ok(path) => path,
        Err(err) => {
            eprintln!("shepr: cannot resolve {description}: {err}");
            std::process::exit(1);
        }
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
    /// Load config data for the diagnostic-only `config check` command.
    /// Application entry points must use `load_validated` and reject every issue.
    /// Both report the same problems, so `config check` passes exactly when a
    /// launch would accept the config.
    pub fn load_for_check() -> LoadedConfig {
        let mut loaded = match try_config_path() {
            Ok(path) => Self::load_from_path(&path),
            Err(err) => LoadedConfig {
                config: Self::default(),
                diagnostics: vec![format!("config path error: {err}")],
            },
        };
        let config_unavailable = loaded.diagnostics.iter().any(|diagnostic| {
            diagnostic.starts_with("config path error:")
                || diagnostic.starts_with("config read error:")
                || diagnostic.starts_with("config parse error:")
        });
        if !config_unavailable && let Some(error) = configured_home_path_error(&loaded.config) {
            loaded.diagnostics.push(error);
        }
        // SHEPR_CONFIG_PATH can name the file while the directories shepr keeps
        // sessions and state in still cannot be resolved.
        let path_error_reported = loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.starts_with("config path error:"));
        if !path_error_reported && let Err(err) = try_config_dir() {
            loaded
                .diagnostics
                .push(format!("config directory error: {err}"));
        }
        if let Err(err) = try_state_dir() {
            loaded
                .diagnostics
                .push(format!("state directory error: {err}"));
        }
        loaded
    }

    /// Load a config for an application launch. Every path or validation
    /// problem is fatal, so a default config from an unsuccessful parse is
    /// never returned to runtime callers.
    pub fn load_validated() -> Result<Self, Vec<String>> {
        Self::load_for_check().into_validated()
    }

    fn load_from_path(path: &Path) -> LoadedConfig {
        match read_optional_config(path) {
            Ok(Some(content)) => Self::load_from_str(&content),
            Ok(None) => LoadedConfig {
                config: Self::default(),
                diagnostics: Vec::new(),
            },
            Err(err) => LoadedConfig {
                config: Self::default(),
                diagnostics: vec![format!("config read error: {err}")],
            },
        }
    }

    fn load_from_str(content: &str) -> LoadedConfig {
        match toml::Deserializer::parse(content).and_then(deserialize_with_ignored::<Config, _>) {
            Ok((mut config, ignored_keys)) => {
                config.ui.user_fields = ui_user_fields(content);
                let (unknown_sections, mut diagnostics) =
                    unknown_top_level_sections_from_str(content);
                diagnostics.extend(unknown_config_key_diagnostics(
                    ignored_keys
                        .into_iter()
                        .filter(|path| {
                            !matches!(path.as_slice(), [ConfigKeyPathSegment::Key(key)] if unknown_sections.contains(key))
                        })
                        .collect(),
                ));
                diagnostics.extend(config.collect_diagnostics());
                LoadedConfig {
                    config,
                    diagnostics,
                }
            }
            Err(err) => LoadedConfig {
                config: Self::default(),
                diagnostics: vec![format!("config parse error: {err}")],
            },
        }
    }
}

fn configured_home_path_error(config: &Config) -> Option<String> {
    let result = match &config.terminal.new_cwd {
        NewTerminalCwdConfig::Home => crate::pathutil::home_dir(),
        NewTerminalCwdConfig::Path(path) if path == "~" || path.starts_with("~/") => {
            crate::pathutil::expand_tilde_path(path)
        }
        NewTerminalCwdConfig::Follow
        | NewTerminalCwdConfig::Current
        | NewTerminalCwdConfig::Path(_) => return None,
    };
    result
        .err()
        .map(|err| format!("terminal.new_cwd cannot be resolved: {err}"))
}

pub fn config_path() -> PathBuf {
    resolve_or_exit("config path", try_config_path())
}

pub fn try_config_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(CONFIG_PATH_ENV_VAR) {
        if path.is_empty() {
            return Err(io::Error::other(format!(
                "{CONFIG_PATH_ENV_VAR} must not be empty"
            )));
        }
        return Ok(PathBuf::from(path));
    }
    Ok(try_config_dir()?.join("config.toml"))
}

/// The keys written under `[ui]`, whatever their value. Serde fills unset keys
/// with defaults, so this is the only record of which ones the user chose.
fn ui_user_fields(content: &str) -> std::collections::BTreeSet<String> {
    content
        .parse::<toml::Table>()
        .ok()
        .and_then(|table| {
            table
                .get("ui")
                .and_then(toml::Value::as_table)
                .map(|ui| ui.keys().cloned().collect())
        })
        .unwrap_or_default()
}

fn unknown_top_level_sections_from_str(content: &str) -> (Vec<String>, Vec<String>) {
    let Ok(table) = content.parse::<toml::Table>() else {
        return (Vec::new(), Vec::new());
    };

    let mut keys = Vec::new();
    let mut diagnostics = Vec::new();
    for (key, value) in &table {
        if let Some(diagnostic) = unknown_top_level_section_diagnostic(key, value) {
            keys.push(key.clone());
            diagnostics.push(diagnostic);
        }
    }
    (keys, diagnostics)
}

fn unknown_top_level_section_diagnostic(key: &str, value: &toml::Value) -> Option<String> {
    if KNOWN_TOP_LEVEL_CONFIG_KEYS.contains(&key) {
        return None;
    }

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
                .into_validated()
                .expect_err("invalid config must not be returned for launch");
            assert!(
                errors.iter().any(|diagnostic| diagnostic.contains(message)),
                "expected {message:?} in {errors:?}"
            );
        }

        let parse_error = Config::load_from_str("[server]\nheadless_cols = \"wide\"\n");
        assert!(parse_error.into_validated().is_err());
    }

    #[test]
    fn load_validated_rejects_bad_config_file_and_accepts_missing_file() {
        let env = crate::test_support::IsolatedEnv::new();
        let scratch = crate::test_support::ScratchDir::new("config-load");
        let path = scratch.join("config.toml");
        env.set(CONFIG_PATH_ENV_VAR, &path);

        std::fs::write(&path, "[server]\nheadless_rows = 0\n").expect("write bad config fixture");
        assert!(Config::load_validated().is_err());

        std::fs::remove_file(&path).expect("remove config fixture");
        assert!(Config::load_validated().is_ok());
    }

    #[test]
    fn load_validated_rejects_home_cwd_without_absolute_home() {
        let env = crate::test_support::IsolatedEnv::new();
        let scratch = crate::test_support::ScratchDir::new("config-cwd");
        let path = scratch.join("config.toml");
        env.set(CONFIG_PATH_ENV_VAR, &path);
        env.set("XDG_CONFIG_HOME", scratch.path());
        env.set("XDG_STATE_HOME", scratch.path());
        env.set("HOME", "relative/home");
        std::fs::write(&path, "[terminal]\nnew_cwd = \"home\"\n").expect("write config fixture");

        let report = Config::load_for_check();
        assert!(
            report
                .diagnostics
                .iter()
                .any(|error| error.contains("terminal.new_cwd")),
            "{:?}",
            report.diagnostics
        );
        let errors = Config::load_validated().expect_err("home cwd needs absolute HOME");
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
            config_path(),
            env.home()
                .join(".config")
                .join(app_dir_name())
                .join("config.toml")
        );
        let custom = env.path().join("custom.toml");
        env.set(CONFIG_PATH_ENV_VAR, &custom);
        assert_eq!(config_path(), custom);

        env.set(CONFIG_PATH_ENV_VAR, "");
        assert!(try_config_path().is_err());
        assert!(Config::load_validated().is_err());
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
    fn config_load_records_which_ui_keys_the_user_set() {
        let loaded = Config::load_from_str(
            r#"
[ui]
sidebar_width = 26
agent_panel_sort = "priority"
"#,
        );
        assert!(loaded.config.ui.is_user_configured("sidebar_width"));
        assert!(loaded.config.ui.is_user_configured("agent_panel_sort"));
        assert!(
            !loaded
                .config
                .ui
                .is_user_configured("sidebar_start_collapsed")
        );

        let empty = Config::load_from_str("[terminal]\n");
        assert!(!empty.config.ui.is_user_configured("sidebar_width"));
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
            assert_eq!(try_config_dir().expect("home fallback"), expected_config);
            assert_eq!(try_state_dir().expect("home fallback"), expected_state);
        }

        let xdg_config = env.path().join("xdg-config");
        let xdg_state = env.path().join("xdg-state");
        env.set("XDG_CONFIG_HOME", &xdg_config);
        env.set("XDG_STATE_HOME", &xdg_state);
        env.set("HOME", "relative/home");
        assert_eq!(
            try_config_dir().expect("absolute XDG config"),
            xdg_config.join(app_dir_name())
        );
        assert_eq!(
            try_state_dir().expect("absolute XDG state"),
            xdg_state.join(app_dir_name())
        );

        env.remove("XDG_CONFIG_HOME");
        env.remove("XDG_STATE_HOME");
        assert!(try_config_dir().is_err());
        assert!(try_state_dir().is_err());
        env.set("HOME", "");
        assert!(try_config_dir().is_err());
        assert!(try_state_dir().is_err());
        env.remove("HOME");
        assert!(try_config_dir().is_err());
        assert!(try_state_dir().is_err());
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
