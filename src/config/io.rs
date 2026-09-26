use std::path::{Path, PathBuf};

use tracing::warn;

use super::{CONFIG_PATH_ENV_VAR, Config, model::LoadedConfig};

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
    if cfg!(debug_assertions) {
        "shepr-dev"
    } else {
        "shepr"
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    platform_config_dir()
}

pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    platform_state_dir()
}

fn platform_config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(format!(".config/{}", app_dir_name()))
    } else {
        std::env::temp_dir().join(app_dir_name())
    }
}

fn platform_state_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(format!(".local/state/{}", app_dir_name()))
    } else {
        std::env::temp_dir().join(format!("{}-state", app_dir_name()))
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
    pub fn load() -> LoadedConfig {
        let path = config_path();
        match read_optional_config(&path) {
            Ok(Some(content)) => Self::load_from_str(&content),
            Ok(None) => LoadedConfig {
                config: Self::default(),
                diagnostics: Vec::new(),
            },
            Err(err) => {
                warn!(err = %err, "config read error, using defaults");
                LoadedConfig {
                    config: Self::default(),
                    diagnostics: vec![format!("config read error: {err}; using defaults")],
                }
            }
        }
    }

    fn load_from_str(content: &str) -> LoadedConfig {
        match toml::Deserializer::parse(content).and_then(deserialize_with_ignored::<Config, _>) {
            Ok((config, ignored_keys)) => {
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
                // The one place config diagnostics are logged: the validators
                // behind `collect_diagnostics` are pure and are re-run by
                // accessors such as `keybinds()`.
                for diagnostic in &diagnostics {
                    warn!(message = %diagnostic, "config diagnostic");
                }
                LoadedConfig {
                    config,
                    diagnostics,
                }
            }
            Err(err) => {
                warn!(err = %err, "config parse error, using defaults");
                LoadedConfig {
                    config: Self::default(),
                    diagnostics: vec![format!("config parse error: {err}; using defaults")],
                }
            }
        }
    }
}

pub fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var(CONFIG_PATH_ENV_VAR) {
        return PathBuf::from(path);
    }
    config_dir().join("config.toml")
}

pub fn config_diagnostic_summary(diagnostics: &[String]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }

    let target = config_path()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml")
        .to_string();
    let read_error = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.starts_with("config read error:"));
    let impact = if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("using defaults"))
    {
        if read_error {
            " unreadable; using defaults"
        } else {
            " invalid; using defaults"
        }
    } else if diagnostics
        .iter()
        .all(|diagnostic| diagnostic.starts_with("unknown config key "))
    {
        " has unknown keys"
    } else {
        ""
    };

    Some(format!("{target}{impact}; shepr config check"))
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

    Some(format!("unknown config section {header}; ignoring section"))
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
        .map(|path| {
            format!(
                "unknown config key {}; ignoring key",
                format_config_key_path(&path)
            )
        })
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
    fn config_diagnostic_summary_uses_compact_actionable_banner() {
        let diagnostics = vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string(),
            "five".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml; shepr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_unknown_keys_compactly() {
        let diagnostics = vec![
            "unknown config key ui.mouse_captur; ignoring key".to_string(),
            "unknown config key keys.new_tabb; ignoring key".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml has unknown keys; shepr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_default_fallback() {
        let diagnostics = vec![
            "config parse error: TOML parse error at line 33, column 8\n   |\n33 | type = \"bogus\"\n   |        ^^^^^^^\nunknown variant `bogus`; using defaults"
                .to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml invalid; using defaults; shepr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_unreadable_config_impact() {
        let startup = vec!["config read error: permission denied; using defaults".to_string()];
        assert_eq!(
            config_diagnostic_summary(&startup).as_deref(),
            Some("config.toml unreadable; using defaults; shepr config check")
        );
    }

    #[test]
    fn config_load_reports_unreadable_path() {
        let _guard = crate::config::test_config_env_lock()
            .lock()
            .expect("test precondition");
        let path =
            std::env::temp_dir().join(format!("shepr-config-unreadable-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("test precondition");
        unsafe { std::env::set_var(CONFIG_PATH_ENV_VAR, &path) };

        let startup = Config::load();
        assert!(
            startup
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("config read error")
                    && diagnostic.contains("using defaults"))
        );

        unsafe { std::env::remove_var(CONFIG_PATH_ENV_VAR) };
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn config_load_warns_about_unknown_keys_and_applies_known_siblings() {
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
                "unknown config key keys.new_tabb; ignoring key",
                "unknown config key plugin; ignoring key",
                "unknown config key theme.custom.accentt; ignoring key",
                "unknown config key ui.\"foo.bar\"; ignoring key",
                "unknown config key ui.mouse_captur; ignoring key",
            ]
        );
        assert_eq!(loaded.config.advanced.scrollback_limit_bytes, 42);
        assert!(!loaded.config.ui.mouse_capture);
    }

    #[test]
    fn config_load_warns_about_unknown_top_level_sections() {
        let loaded = Config::load_from_str(
            r#"
[[plugin]]
id = "example"
"#,
        );

        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config section [[plugin]]; ignoring section"]
        );
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
