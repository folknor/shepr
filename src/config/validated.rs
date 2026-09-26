use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::{
    AppPaths, Config,
    wire::{WireConfig, WireKeybindCache, WirePalette},
};

/// The source that selected a resolved configuration value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigSource {
    #[default]
    Default,
    ConfigFileKey,
    EnvironmentVariable(String),
    CliFlag(String),
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("default"),
            Self::ConfigFileKey => formatter.write_str("config file key"),
            Self::EnvironmentVariable(variable) => {
                write!(formatter, "environment variable {variable}")
            }
            Self::CliFlag(flag) => write!(formatter, "CLI flag {flag}"),
        }
    }
}

/// Origin attached to one resolved leaf in the config surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigValueOrigin {
    pub key: String,
    pub value: String,
    pub source: ConfigSource,
}

/// Typed queries for the client preferences whose behavior depends on whether
/// the user supplied a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiPreferenceKey {
    SidebarWidth,
    SidebarStartCollapsed,
    AgentPanelSort,
    Accent,
}

/// Origins for every value in the resolved config, plus direct typed queries
/// for settings where persisted runtime preferences yield to config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigProvenance {
    values: Vec<ConfigValueOrigin>,
    ui_sidebar_width: ConfigSource,
    ui_sidebar_start_collapsed: ConfigSource,
    ui_agent_panel_sort: ConfigSource,
    ui_accent: ConfigSource,
}

impl ConfigProvenance {
    pub(crate) fn from_config(
        config: &Config,
        document: Option<&toml::Value>,
    ) -> Result<Self, String> {
        let encoded = serde_json::to_value(config)
            .map_err(|error| format!("cannot enumerate resolved config values: {error}"))?;
        let mut config_paths = Vec::new();
        collect_paths(&encoded, &mut Vec::new(), &mut config_paths);

        let mut explicit_paths = Vec::new();
        if let Some(document) = document {
            collect_toml_paths(document, &mut Vec::new(), &mut explicit_paths);
        }

        let values = config_paths
            .into_iter()
            .map(|(key, value)| ConfigValueOrigin {
                value,
                source: if explicit_paths.iter().any(|path| path == &key) {
                    ConfigSource::ConfigFileKey
                } else {
                    ConfigSource::Default
                },
                key,
            })
            .collect();

        let source = |key: &str| {
            if explicit_paths.iter().any(|path| path == key) {
                ConfigSource::ConfigFileKey
            } else {
                ConfigSource::Default
            }
        };
        Ok(Self {
            values,
            ui_sidebar_width: source("ui.sidebar_width"),
            ui_sidebar_start_collapsed: source("ui.sidebar_start_collapsed"),
            ui_agent_panel_sort: source("ui.agent_panel_sort"),
            ui_accent: source("ui.accent"),
        })
    }

    pub fn values(&self) -> &[ConfigValueOrigin] {
        &self.values
    }

    pub fn source(&self, key: UiPreferenceKey) -> &ConfigSource {
        match key {
            UiPreferenceKey::SidebarWidth => &self.ui_sidebar_width,
            UiPreferenceKey::SidebarStartCollapsed => &self.ui_sidebar_start_collapsed,
            UiPreferenceKey::AgentPanelSort => &self.ui_agent_panel_sort,
            UiPreferenceKey::Accent => &self.ui_accent,
        }
    }

    pub fn is_explicit(&self, key: UiPreferenceKey) -> bool {
        !matches!(self.source(key), ConfigSource::Default)
    }

    pub(crate) fn key_is_configured(&self, key: &str) -> bool {
        self.values.iter().any(|origin| {
            let value_is_under_key = origin.key.strip_prefix(key).is_some_and(|suffix| {
                suffix.is_empty() || suffix.starts_with('.') || suffix.starts_with('[')
            });
            value_is_under_key && !matches!(origin.source, ConfigSource::Default)
        })
    }

    fn keybinding_values(&self) -> impl Iterator<Item = &ConfigValueOrigin> {
        self.values
            .iter()
            .filter(|origin| origin.key.starts_with("keys."))
    }

    /// Build a default-origin record for values constructed outside process
    /// startup, such as test fixtures and snapshots built by unit tests.
    pub(crate) fn defaults(config: &Config) -> Self {
        Self::from_config(config, None).unwrap_or_else(|_| Self {
            values: Vec::new(),
            ui_sidebar_width: ConfigSource::Default,
            ui_sidebar_start_collapsed: ConfigSource::Default,
            ui_agent_panel_sort: ConfigSource::Default,
            ui_accent: ConfigSource::Default,
        })
    }
}

fn collect_paths(
    value: &serde_json::Value,
    path: &mut Vec<ConfigPathSegment>,
    output: &mut Vec<(String, String)>,
) {
    match value {
        serde_json::Value::Object(fields) if !fields.is_empty() => {
            for (key, value) in fields {
                path.push(ConfigPathSegment::Key(key.clone()));
                collect_paths(value, path, output);
                let _ = path.pop();
            }
        }
        serde_json::Value::Array(values) if !values.is_empty() => {
            for (index, value) in values.iter().enumerate() {
                path.push(ConfigPathSegment::Index(index));
                collect_paths(value, path, output);
                let _ = path.pop();
            }
        }
        _ => output.push((format_config_path(path), value.to_string())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigPathSegment {
    Key(String),
    Index(usize),
}

fn format_config_path(path: &[ConfigPathSegment]) -> String {
    let mut formatted = String::new();
    for segment in path {
        match segment {
            ConfigPathSegment::Key(key) => {
                if !formatted.is_empty() {
                    formatted.push('.');
                }
                if !key.is_empty()
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    formatted.push_str(key);
                } else {
                    formatted.push_str(&toml::Value::String(key.clone()).to_string());
                }
            }
            ConfigPathSegment::Index(index) => {
                formatted.push('[');
                formatted.push_str(&index.to_string());
                formatted.push(']');
            }
        }
    }
    formatted
}

fn collect_toml_paths(
    value: &toml::Value,
    path: &mut Vec<ConfigPathSegment>,
    output: &mut Vec<String>,
) {
    match value {
        toml::Value::Table(fields) if !fields.is_empty() => {
            for (key, value) in fields {
                path.push(ConfigPathSegment::Key(key.clone()));
                collect_toml_paths(value, path, output);
                let _ = path.pop();
            }
        }
        toml::Value::Array(values) if !values.is_empty() => {
            for (index, value) in values.iter().enumerate() {
                path.push(ConfigPathSegment::Index(index));
                collect_toml_paths(value, path, output);
                let _ = path.pop();
            }
        }
        _ => output.push(format_config_path(path)),
    }
}

/// Immutable, validated configuration resolved at the process boundary.
/// Runtime preferences continue to live in their own mutable state.
#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    config: Config,
    provenance: ConfigProvenance,
    paths: AppPaths,
    resolved_palette: crate::app::state::Palette,
    keybind_validation: super::keybinds::KeybindValidation,
}

impl ValidatedConfig {
    #[cfg(test)]
    pub(crate) fn new(
        config: Config,
        provenance: ConfigProvenance,
        paths: AppPaths,
    ) -> Result<Self, Vec<String>> {
        let keybind_validation = config.compute_keybind_validation(|field| {
            provenance.key_is_configured(&format!("keys.{field}"))
        });
        let resolved_palette = config
            .resolve_palette_with_ui_accent(provenance.is_explicit(UiPreferenceKey::Accent))?;
        let diagnostics = config.collect_diagnostics_with_keybind_validation(&keybind_validation);
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        Ok(Self {
            config,
            provenance,
            paths,
            resolved_palette,
            keybind_validation,
        })
    }

    pub(crate) fn from_loaded(
        config: Config,
        provenance: ConfigProvenance,
        keybind_validation: super::keybinds::KeybindValidation,
        paths: AppPaths,
    ) -> Result<Self, Vec<String>> {
        let resolved_palette = config
            .resolve_palette_with_ui_accent(provenance.is_explicit(UiPreferenceKey::Accent))?;
        Ok(Self {
            config,
            provenance,
            paths,
            resolved_palette,
            keybind_validation,
        })
    }

    pub fn provenance(&self) -> &ConfigProvenance {
        &self.provenance
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub(crate) fn palette(&self) -> &crate::app::state::Palette {
        &self.resolved_palette
    }

    pub(crate) fn live_keybinds(&self) -> super::LiveKeybindConfig {
        super::LiveKeybindConfig {
            prefix: self.keybind_validation.prefix,
            keybinds: self.keybind_validation.keybinds.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn validated_live_keybinds(&self) -> Result<super::LiveKeybindConfig, Vec<String>> {
        if self.keybind_validation.prefix_diag.is_some()
            || !self.keybind_validation.keybind_diags.is_empty()
        {
            Err(self
                .keybind_validation
                .prefix_diag
                .iter()
                .cloned()
                .chain(self.keybind_validation.keybind_diags.iter().cloned())
                .collect())
        } else {
            Ok(self.live_keybinds())
        }
    }

    pub(crate) fn same_keybinding_resolution(&self, other: &Self) -> bool {
        self.config.keys == other.config.keys
            && self
                .provenance
                .keybinding_values()
                .eq(other.provenance.keybinding_values())
    }

    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        let config = Config::default();
        let provenance = ConfigProvenance::defaults(&config);
        Self::new(config, provenance, AppPaths::default())
            .expect("the default test config is valid")
    }

    #[cfg(test)]
    pub(crate) fn test_from_config(config: Config, source: Option<&str>) -> Self {
        Self::test_from_config_with_paths(config, source, AppPaths::default())
    }

    #[cfg(test)]
    pub(crate) fn test_from_config_with_paths(
        config: Config,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Self {
        let document = source.map(|source| {
            source
                .parse::<toml::Table>()
                .map(toml::Value::Table)
                .expect("test config document is valid")
        });
        let provenance = ConfigProvenance::from_config(&config, document.as_ref())
            .expect("test config values are serializable");
        Self::new(config, provenance, paths).expect("test config is valid")
    }
}

impl PartialEq for ValidatedConfig {
    fn eq(&self, other: &Self) -> bool {
        let left = serde_json::to_value(WireConfig::from_config(&self.config));
        let right = serde_json::to_value(WireConfig::from_config(&other.config));
        matches!((left, right), (Ok(left), Ok(right))
            if left == right && self.provenance == other.provenance && self.paths == other.paths)
    }
}

impl Eq for ValidatedConfig {}

impl Deref for ValidatedConfig {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl Serialize for ValidatedConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let keybindings = WireKeybindCache::from_validation(&self.keybind_validation)
            .map_err(serde::ser::Error::custom)?;
        #[derive(Serialize)]
        struct Wire<'a> {
            config: WireConfig,
            provenance: &'a ConfigProvenance,
            paths: &'a AppPaths,
            palette: WirePalette,
            keybindings: WireKeybindCache,
        }

        Wire {
            config: WireConfig::from_config(&self.config),
            provenance: &self.provenance,
            paths: &self.paths,
            palette: WirePalette::from(&self.resolved_palette),
            keybindings,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ValidatedConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            config: WireConfig,
            provenance: ConfigProvenance,
            paths: AppPaths,
            palette: WirePalette,
            keybindings: WireKeybindCache,
        }

        let Wire {
            config,
            provenance,
            paths,
            palette,
            keybindings,
        } = Wire::deserialize(deserializer)?;
        let config = config.into_config().map_err(de::Error::custom)?;
        let keybind_validation = keybindings.into_validation().map_err(de::Error::custom)?;
        Ok(Self {
            config,
            provenance,
            paths,
            resolved_palette: palette.into(),
            keybind_validation,
        })
    }
}
