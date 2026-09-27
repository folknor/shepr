use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::fmt;

use super::{
    AppPaths, Config, SidebarBounds,
    model::{
        AdvancedConfig, ExperimentalConfig, NewTerminalCwdConfig, RemoteConfig, SessionConfig,
        TerminalConfig, UiConfig,
    },
    tab_bar::ValidatedTabBarRightEntry,
    window_title::WindowTitleTemplate,
    wire::WireConfig,
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

    /// Build a default-origin record for test fixtures and for the placeholder
    /// config a failed load carries alongside its (non-empty) diagnostics; a
    /// load with diagnostics never becomes a `ValidatedConfig`.
    pub(crate) fn defaults(config: &Config) -> Self {
        Self::from_config(config, None).unwrap_or_else(|_| Self {
            // Production uses this only for a failed-load placeholder, whose diagnostics prevent it
            // from becoming a ValidatedConfig.
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

/// Configuration values that runtime code consumes after launch validation.
/// The width and bounds travel together and stay private, so the width is
/// always inside the bounds and no crate outside this one can build a value.
#[derive(Debug, Clone)]
pub struct ValidatedUiConfig {
    sidebar_width: u16,
    sidebar_bounds: SidebarBounds,
    pub sidebar_start_collapsed: bool,
    pub sidebar_collapsed_mode: super::SidebarCollapsedModeConfig,
    pub mouse_capture: bool,
    pub copy_on_select: bool,
    pub host_cursor: super::HostCursorModeConfig,
    pub right_click_passthrough_modifiers: Option<crossterm::event::KeyModifiers>,
    pub redraw_on_focus_gained: bool,
    pub mouse_scroll_lines: std::num::NonZeroU16,
    pub confirm_close: bool,
    pub prompt_new_tab_name: bool,
    pub prompt_new_workspace_name: bool,
    pub pane_borders: super::PaneBordersConfig,
    pub pane_outer_borders: bool,
    pub pane_scrollbars: bool,
    pub pane_gaps: bool,
    pub show_agent_labels_on_pane_borders: bool,
    pub hide_tab_bar_when_single_tab: bool,
    pub tab_bar_position: super::TabBarPositionConfig,
    pub tab_bar_right: Vec<ValidatedTabBarRightEntry>,
    pub tab_bar_right_separator: String,
    pub window_title: Option<WindowTitleTemplate>,
    pub agent_panel_sort: super::AgentPanelSortConfig,
    pub status_indicators: super::StatusIndicatorStyle,
    pub sidebar: super::SidebarConfig,
}

/// Working directory policy resolved from launch config.
/// `Path` has a leading `~` already expanded against the captured home
/// directory. It is not promised to be absolute or to exist: whether a
/// directory is usable is a runtime fact the pane launch checks when it spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewTerminalCwd {
    Follow,
    Home,
    Current,
    Path(std::path::PathBuf),
}

#[derive(Debug, Clone)]
pub struct ValidatedTerminalConfig {
    pub default_shell: String,
    pub login_shell: bool,
    pub new_cwd: NewTerminalCwd,
}

impl ValidatedTerminalConfig {
    fn from_config(config: &TerminalConfig) -> Self {
        let new_cwd = match &config.new_cwd {
            NewTerminalCwdConfig::Follow => NewTerminalCwd::Follow,
            NewTerminalCwdConfig::Home => NewTerminalCwd::Home,
            NewTerminalCwdConfig::Current => NewTerminalCwd::Current,
            NewTerminalCwdConfig::Path(path) => {
                NewTerminalCwd::Path(std::path::PathBuf::from(path))
            }
        };
        Self {
            default_shell: config.default_shell.clone(),
            login_shell: config.login_shell,
            new_cwd,
        }
    }

    /// Expand a `~` in `new_cwd` once, against the paths captured with the
    /// config. `configured_home_path_error` reports the same failure as a
    /// diagnostic before any caller reaches this.
    fn expand_home(mut self, home_dir: Option<&std::path::Path>) -> Result<Self, String> {
        if let NewTerminalCwd::Path(path) = &self.new_cwd {
            let expanded = shepr_core::pathutil::expand_tilde_path_with_home(path, home_dir)
                .map_err(|err| format!("terminal.new_cwd cannot be resolved: {err}"))?;
            self.new_cwd = NewTerminalCwd::Path(expanded);
        }
        Ok(self)
    }
}

impl ValidatedUiConfig {
    /// Configured expanded sidebar width, already clamped to `sidebar_bounds`.
    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width
    }

    pub fn sidebar_bounds(&self) -> SidebarBounds {
        self.sidebar_bounds
    }

    fn from_config(
        config: &UiConfig,
        bounds: SidebarBounds,
        mouse_scroll_lines: std::num::NonZeroU16,
        tab_bar_right: Vec<ValidatedTabBarRightEntry>,
        window_title: Option<WindowTitleTemplate>,
    ) -> Self {
        Self {
            sidebar_width: bounds.clamp_width(config.sidebar_width),
            sidebar_bounds: bounds,
            sidebar_start_collapsed: config.sidebar_start_collapsed,
            sidebar_collapsed_mode: config.sidebar_collapsed_mode,
            mouse_capture: config.mouse_capture,
            copy_on_select: config.copy_on_select,
            host_cursor: config.host_cursor,
            right_click_passthrough_modifiers: config.right_click_passthrough_modifiers(),
            redraw_on_focus_gained: config.redraw_on_focus_gained,
            mouse_scroll_lines,
            confirm_close: config.confirm_close,
            prompt_new_tab_name: config.prompt_new_tab_name,
            prompt_new_workspace_name: config.prompt_new_workspace_name,
            pane_borders: config.pane_borders,
            pane_outer_borders: config.pane_outer_borders,
            pane_scrollbars: config.pane_scrollbars,
            pane_gaps: config.pane_gaps,
            show_agent_labels_on_pane_borders: config.show_agent_labels_on_pane_borders,
            hide_tab_bar_when_single_tab: config.hide_tab_bar_when_single_tab,
            tab_bar_position: config.tab_bar_position,
            tab_bar_right,
            tab_bar_right_separator: config.tab_bar_right_separator.clone(),
            window_title,
            agent_panel_sort: config.agent_panel_sort,
            status_indicators: config.status_indicators,
            sidebar: config.sidebar.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedValues {
    pub(crate) headless_size: shepr_core::geometry::GridSize,
    pub(crate) palette: crate::theme::Palette,
    pub(crate) live_keybinds: super::LiveKeybindConfig,
    pub(crate) ui: ValidatedUiConfig,
    pub(crate) terminal: ValidatedTerminalConfig,
}

/// Results of parsing each value used at runtime, plus every diagnostic found.
/// No fallback values escape this boundary: `values` is present only when all
/// parsed fields are valid.
#[derive(Debug, Clone)]
pub(crate) struct ConfigResolution {
    pub(crate) diagnostics: Vec<String>,
    pub(crate) values: Option<ValidatedValues>,
}

impl ConfigResolution {
    pub(crate) fn parse(config: &Config, provenance: &ConfigProvenance) -> Self {
        let keybind_validation = config.compute_keybind_validation(|field| {
            provenance.key_is_configured(&format!("keys.{field}"))
        });
        let palette =
            config.resolve_palette_with_ui_accent(provenance.is_explicit(UiPreferenceKey::Accent));
        let headless_size = shepr_core::geometry::GridSize::new(
            config.server.headless_cols,
            config.server.headless_rows,
        );
        let sidebar_bounds = super::validated_sidebar_bounds(
            config.ui.sidebar_min_width,
            config.ui.sidebar_max_width,
        );
        let tab_bar_right = super::tab_bar::parse_tab_bar_right_entries(&config.ui.tab_bar_right);
        let window_title = WindowTitleTemplate::parse(&config.ui.window_title);
        let mouse_scroll_lines = u16::try_from(config.ui.mouse_scroll_lines())
            .ok()
            .and_then(std::num::NonZeroU16::new);

        let mut diagnostics = keybind_validation.diagnostics.clone();
        if let Err(errors) = &palette {
            diagnostics.extend(errors.iter().cloned());
        }
        if let Err(errors) = &tab_bar_right {
            diagnostics.extend(errors.iter().cloned());
        }
        if let Err(error) = &window_title {
            diagnostics.push(format!("ui.window_title {error}"));
        }
        if sidebar_bounds.is_none() {
            diagnostics.push(format!(
                "ui.sidebar_min_width ({}) is greater than sidebar_max_width ({})",
                config.ui.sidebar_min_width, config.ui.sidebar_max_width
            ));
        }
        if headless_size.is_none() {
            diagnostics.push(format!(
                "server.headless_cols and server.headless_rows must be greater than zero (got {}x{})",
                config.server.headless_cols, config.server.headless_rows
            ));
        }
        if mouse_scroll_lines.is_none() {
            diagnostics.push(format!(
                "ui.mouse_scroll_lines must be between 1 and {} (got {})",
                u16::MAX,
                config.ui.mouse_scroll_lines()
            ));
        }

        let values = if diagnostics.is_empty() {
            match (
                keybind_validation.live,
                palette,
                headless_size,
                sidebar_bounds,
                mouse_scroll_lines,
                tab_bar_right,
                window_title,
            ) {
                (
                    Some(live_keybinds),
                    Ok(palette),
                    Some(headless_size),
                    Some(sidebar_bounds),
                    Some(mouse_scroll_lines),
                    Ok(tab_bar_right),
                    Ok(window_title),
                ) => Some(ValidatedValues {
                    headless_size,
                    palette,
                    live_keybinds,
                    ui: ValidatedUiConfig::from_config(
                        &config.ui,
                        sidebar_bounds,
                        mouse_scroll_lines,
                        tab_bar_right,
                        window_title,
                    ),
                    terminal: ValidatedTerminalConfig::from_config(&config.terminal),
                }),
                _ => None,
            }
        } else {
            None
        };

        Self {
            diagnostics,
            values,
        }
    }
}

/// Immutable, validated configuration resolved at the process boundary.
/// Runtime preferences continue to live in their own mutable state.
#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    config: Config,
    provenance: ConfigProvenance,
    paths: AppPaths,
    resolved_palette: crate::theme::Palette,
    headless_size: shepr_core::geometry::GridSize,
    live_keybinds: super::LiveKeybindConfig,
    ui: ValidatedUiConfig,
    terminal: ValidatedTerminalConfig,
}

impl ValidatedConfig {
    #[cfg(any(test, feature = "test-support"))]
    pub fn new(
        config: Config,
        provenance: ConfigProvenance,
        paths: AppPaths,
    ) -> Result<Self, Vec<String>> {
        Self::from_resolution(config, provenance, paths)
    }

    /// Build from a load whose diagnostics, including the home-path check, are
    /// already empty.
    pub(crate) fn from_loaded(
        config: Config,
        provenance: ConfigProvenance,
        values: ValidatedValues,
        paths: AppPaths,
    ) -> Result<Self, String> {
        let terminal = values.terminal.expand_home(paths.home_dir())?;
        Ok(Self {
            config,
            provenance,
            paths,
            resolved_palette: values.palette,
            headless_size: values.headless_size,
            live_keybinds: values.live_keybinds,
            ui: values.ui,
            terminal,
        })
    }

    fn from_resolution(
        config: Config,
        provenance: ConfigProvenance,
        paths: AppPaths,
    ) -> Result<Self, Vec<String>> {
        let resolution = ConfigResolution::parse(&config, &provenance);
        let mut diagnostics = resolution.diagnostics;
        if let Some(error) = super::io::configured_home_path_error(&config, paths.home_dir()) {
            diagnostics.push(error);
        }
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        match resolution.values {
            Some(values) => {
                Self::from_loaded(config, provenance, values, paths).map_err(|error| vec![error])
            }
            None => Err(vec!["configuration could not be resolved".to_owned()]),
        }
    }

    pub fn provenance(&self) -> &ConfigProvenance {
        &self.provenance
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn palette(&self) -> &crate::theme::Palette {
        &self.resolved_palette
    }

    pub fn headless_size(&self) -> shepr_core::geometry::GridSize {
        self.headless_size
    }

    pub fn ui(&self) -> &ValidatedUiConfig {
        &self.ui
    }

    pub fn terminal(&self) -> &ValidatedTerminalConfig {
        &self.terminal
    }

    pub fn session(&self) -> &SessionConfig {
        &self.config.session
    }

    pub fn advanced(&self) -> &AdvancedConfig {
        &self.config.advanced
    }

    pub fn experimental(&self) -> &ExperimentalConfig {
        &self.config.experimental
    }

    pub fn remote(&self) -> &RemoteConfig {
        &self.config.remote
    }

    pub fn live_keybinds(&self) -> super::LiveKeybindConfig {
        self.live_keybinds.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn validated_live_keybinds(&self) -> Result<super::LiveKeybindConfig, Vec<String>> {
        Ok(self.live_keybinds())
    }

    pub fn same_keybinding_resolution(&self, other: &Self) -> bool {
        self.config.keys == other.config.keys
            && self
                .provenance
                .keybinding_values()
                .eq(other.provenance.keybinding_values())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_default() -> Self {
        let config = Config::default();
        let provenance = ConfigProvenance::defaults(&config);
        Self::new(config, provenance, test_app_paths()).expect("the default test config is valid")
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_from_config(config: Config, source: Option<&str>) -> Self {
        Self::test_from_config_with_paths(config, source, test_app_paths())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_from_config_with_paths(
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
        // These inputs determine every resolved field cached on ValidatedConfig.
        self.config == other.config
            && self.provenance == other.provenance
            && self.paths == other.paths
    }
}

impl Eq for ValidatedConfig {}

/// Absolute paths, so the config survives the resolved-path check on the
/// wire, that are identical across calls, so two test configs compare equal.
/// The root cannot be created by an unprivileged user: a test that writes
/// through these paths fails instead of leaving files in a shared location.
/// Tests that need real directories pass a `ScratchDir` to
/// `test_from_config_with_paths`.
#[cfg(any(test, feature = "test-support"))]
fn test_app_paths() -> AppPaths {
    AppPaths::default()
}

impl Serialize for ValidatedConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            config: WireConfig,
            provenance: &'a ConfigProvenance,
            paths: &'a AppPaths,
        }

        Wire {
            config: WireConfig::from_config(&self.config),
            provenance: &self.provenance,
            paths: &self.paths,
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
        }

        let Wire {
            config,
            provenance,
            paths,
        } = Wire::deserialize(deserializer)?;
        let config = config.into_config().map_err(de::Error::custom)?;
        // Rebuild runtime values from the raw config so the receiver applies
        // the same validation boundary as the server.
        Self::from_resolution(config, provenance, paths)
            .map_err(|diagnostics| de::Error::custom(diagnostics.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_config_resolves_runtime_values_once() {
        let mut config = Config::default();
        config.server.headless_cols = 92;
        config.server.headless_rows = 31;
        config.ui.sidebar_min_width = 12;
        config.ui.sidebar_max_width = 30;
        config.ui.sidebar_width = 80;
        config.ui.window_title = "{hostname}: {workspace}".to_owned();
        config.ui.tab_bar_right = vec![super::super::TabBarRightEntryConfig::Datetime {
            format: "%H:%M".to_owned(),
        }];
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("relative/worktree".to_owned());
        let provenance = ConfigProvenance::defaults(&config);

        let validated = ValidatedConfig::new(config, provenance, AppPaths::default())
            .expect("test configuration is valid");

        assert_eq!(
            validated.headless_size(),
            shepr_core::geometry::GridSize::new(92, 31).expect("non-zero test dimensions")
        );
        assert_eq!(validated.ui().sidebar_width(), 30);
        assert_eq!(validated.ui().sidebar_bounds().min(), 12);
        assert_eq!(validated.ui().sidebar_bounds().max(), 30);
        assert!(validated.ui().window_title.is_some());
        assert!(matches!(
            validated.ui().tab_bar_right.as_slice(),
            [ValidatedTabBarRightEntry::Datetime { .. }]
        ));
        assert_eq!(
            validated.terminal().new_cwd,
            NewTerminalCwd::Path(std::path::PathBuf::from("relative/worktree"))
        );
    }

    #[test]
    fn validated_config_expands_home_in_new_cwd_path_once() {
        let mut config = Config::default();
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("~/work".to_owned());
        let provenance = ConfigProvenance::defaults(&config);
        let paths = AppPaths::test_with_context(
            std::path::Path::new("/shepr-test-root"),
            Some(std::path::Path::new("/home/shepr-test")),
            None,
        );

        let validated =
            ValidatedConfig::new(config, provenance, paths).expect("test configuration is valid");

        assert_eq!(
            validated.terminal().new_cwd,
            NewTerminalCwd::Path(std::path::PathBuf::from("/home/shepr-test/work"))
        );
    }

    #[test]
    fn wire_deserialization_revalidates_raw_config() {
        let scratch = shepr_test_support::ScratchDir::new("validated-config-wire");
        let validated = ValidatedConfig::test_from_config_with_paths(
            Config::default(),
            None,
            AppPaths::test_with_context(scratch.path(), Some(scratch.path()), None),
        );
        let mut wire = serde_json::to_value(validated).expect("serialize test config");
        wire["config"]["server"]["headless_cols"] = serde_json::json!(0);

        assert!(serde_json::from_value::<ValidatedConfig>(wire).is_err());
    }

    #[test]
    fn wire_deserialization_rejects_missing_captured_home_directory() {
        let scratch = shepr_test_support::ScratchDir::new("validated-config-home-wire");
        let validated = ValidatedConfig::test_from_config_with_paths(
            Config::default(),
            None,
            AppPaths::test_with_context(scratch.path(), Some(scratch.path()), None),
        );
        let mut wire = serde_json::to_value(validated).expect("serialize test config");
        wire["config"]["terminal"]["new_cwd"] = serde_json::json!("Home");
        wire["paths"]["home_dir"] = serde_json::Value::Null;

        let error = serde_json::from_value::<ValidatedConfig>(wire)
            .expect_err("resolved paths require the captured home directory");
        assert!(
            error.to_string().contains("resolved home_dir"),
            "unexpected error: {error}"
        );
    }
}
