use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

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
use crate::limits::{MAX_MOUSE_SCROLL_LINES, MIN_MOUSE_SCROLL_LINES};

/// The source that selected a resolved configuration value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigSource {
    #[default]
    Default,
    ConfigFileKey,
    EnvironmentVariable(String),
    /// The session was selected with the `--session` flag.
    CliFlag,
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("default"),
            Self::ConfigFileKey => formatter.write_str("config file key"),
            Self::EnvironmentVariable(variable) => {
                write!(formatter, "environment variable {variable}")
            }
            Self::CliFlag => formatter.write_str("CLI flag --session"),
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
///
/// Every value is stringified and shipped to each attached client for
/// display, paths and `tab_bar_right` command lines included. That is fine
/// while no config key holds a credential; a key that does must be left out
/// of `values`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigProvenance {
    /// One entry per resolved config leaf, so the local config file bounds
    /// its length. It needs no field cap of its own on the wire: it travels only
    /// inside the client snapshot's resolved config blob, which is capped at
    /// the frame size and decoded under the codec's collection limit.
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
        // SidebarTokenRule serializes through RawRule, whose optional fields remain
        // present as null so provenance can enumerate absent rule settings.
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
        let ui_accent = if config.ui.accent.is_some() {
            source("ui.accent")
        } else {
            ConfigSource::Default
        };
        Ok(Self {
            values,
            ui_sidebar_width: source("ui.sidebar_width"),
            ui_sidebar_start_collapsed: source("ui.sidebar_start_collapsed"),
            ui_agent_panel_sort: source("ui.agent_panel_sort"),
            ui_accent,
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
/// `Path` is an existing absolute directory. Configured relative paths are
/// resolved against the current directory captured at launch
/// ([`AppPaths::current_dir`]; for the server, the directory `shepr` was
/// launched from), so creating a pane later does not depend on the caller's
/// working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewTerminalCwd {
    Follow,
    Home,
    Current,
    Path(std::path::PathBuf),
}

#[derive(Debug, Clone)]
pub struct ValidatedTerminalConfig {
    /// Absolute, recognized shell selected and resolved at process launch.
    pub default_shell: String,
    pub login_shell: bool,
    pub new_cwd: NewTerminalCwd,
}

/// Whether resolving `terminal.new_cwd` checks the directory on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CwdCheck {
    /// The process loading the config file: the directory must exist here.
    AtLaunch,
    /// A config decoded from a server, possibly on another host. The sender
    /// already checked the directory against its own filesystem; only the
    /// pure resolution (tilde, relative join) is repeated.
    Received,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellCheck {
    /// Resolve the process's configured and inherited shell inputs.
    AtLaunch,
    /// Preserve the resolved path received from another process or host.
    Received,
}

impl ValidatedTerminalConfig {
    fn parse(
        config: &TerminalConfig,
        paths: &AppPaths,
        cwd_check: CwdCheck,
        shell_check: ShellCheck,
    ) -> Result<Self, Vec<String>> {
        let default_shell = match shell_check {
            ShellCheck::AtLaunch => resolve_default_shell(&config.default_shell, paths),
            ShellCheck::Received => Ok(config.default_shell.clone()),
        };
        let new_cwd = Self::parse_new_cwd(&config.new_cwd, paths, cwd_check);
        match (default_shell, new_cwd) {
            (Ok(default_shell), Ok(new_cwd)) => Ok(Self {
                default_shell,
                login_shell: config.login_shell,
                new_cwd,
            }),
            (default_shell, new_cwd) => {
                let mut errors = Vec::new();
                if let Err(error) = default_shell {
                    errors.push(error);
                }
                if let Err(error) = new_cwd {
                    errors.push(error);
                }
                Err(errors)
            }
        }
    }

    pub(crate) fn parse_new_cwd(
        configured: &NewTerminalCwdConfig,
        paths: &AppPaths,
        check: CwdCheck,
    ) -> Result<NewTerminalCwd, String> {
        let check_dir = |path: &Path| match check {
            CwdCheck::AtLaunch => checked_new_cwd_directory(path),
            CwdCheck::Received => Ok(path.to_path_buf()),
        };
        match configured {
            NewTerminalCwdConfig::Follow => Ok(NewTerminalCwd::Follow),
            NewTerminalCwdConfig::Home => {
                let path = paths.home_dir().ok_or_else(|| {
                    format!(
                        "terminal.new_cwd cannot be resolved: {}",
                        shepr_core::pathutil::missing_home_error()
                    )
                })?;
                check_dir(path)?;
                Ok(NewTerminalCwd::Home)
            }
            NewTerminalCwdConfig::Current => {
                let path = paths.current_dir().ok_or_else(|| {
                    "terminal.new_cwd current directory was unavailable at launch".to_owned()
                })?;
                check_dir(path)?;
                Ok(NewTerminalCwd::Current)
            }
            NewTerminalCwdConfig::Path(configured_path) => {
                if configured_path.is_empty() {
                    return Err(
                        "terminal.new_cwd path must not be empty; use \"follow\" explicitly"
                            .to_owned(),
                    );
                }
                let path = shepr_core::pathutil::expand_tilde_path_with_home(
                    configured_path,
                    paths.home_dir(),
                )
                .map_err(|err| format!("terminal.new_cwd cannot be resolved: {err}"))?;
                let absolute = if path.is_absolute() {
                    path
                } else {
                    let current_dir = paths.current_dir().ok_or_else(|| {
                        "terminal.new_cwd relative path requires a launch working directory"
                            .to_owned()
                    })?;
                    current_dir.join(path)
                };
                check_dir(&absolute).map(NewTerminalCwd::Path)
            }
        }
    }
}

/// The pane shell for this launch. An empty setting means `$SHELL`, and
/// `/bin/sh` only when `SHELL` is unset or blank.
///
/// An inherited `SHELL` is held to the same standard as a configured shell:
/// one that is unusable or not a shell shepr recognises fails the launch
/// rather than falling back to `/bin/sh`. With `terminal.default_shell` empty
/// (the default, and the only state with no config file), `SHELL` is the
/// setting, so a bad value is a config problem like any other. A fallback
/// would only have surfaced as a line in the server log, which the TUI never
/// shows, while every pane silently opened a different shell than the
/// operator's own; the fix is one line in either place, so the error names
/// both.
fn resolve_default_shell(configured: &str, paths: &AppPaths) -> Result<String, String> {
    let path = shepr_core::env::read_os(shepr_core::env::EnvVar::Path)
        .map_err(|error| error.to_string())?;
    let cwd = paths
        .current_dir()
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));

    let configured = configured.trim();
    if !configured.is_empty() {
        return resolve_recognized_shell(
            OsStr::new(configured),
            "terminal.default_shell",
            path.as_deref(),
            &cwd,
        )
        .and_then(|shell| shell_path_string(shell, "terminal.default_shell"));
    }

    let inherited = shepr_core::env::read_os(shepr_core::env::EnvVar::Shell)
        .map_err(|error| error.to_string())?
        .and_then(|shell| shepr_core::shell::trim_shell_value(&shell));
    if let Some(inherited) = inherited {
        return resolve_recognized_shell(&inherited, "SHELL", path.as_deref(), &cwd)
            .and_then(|shell| shell_path_string(shell, "SHELL"))
            .map_err(|error| {
                format!(
                    "{error}; terminal.default_shell is empty, so panes run SHELL={}. \
                     Set terminal.default_shell to a shell shepr recognizes, or fix SHELL",
                    inherited.to_string_lossy()
                )
            });
    }

    resolve_recognized_shell(
        OsStr::new("/bin/sh"),
        "the default shell",
        path.as_deref(),
        &cwd,
    )
    .and_then(|shell| shell_path_string(shell, "the default shell"))
}

/// `source` names where `candidate` came from, so the error points at the
/// setting or variable to fix.
fn resolve_recognized_shell(
    candidate: &OsStr,
    source: &str,
    path: Option<&OsStr>,
    cwd: &Path,
) -> Result<PathBuf, String> {
    // Match the PTY's access(2) check so noexec mounts and access policy are
    // part of validation before the server starts.
    let resolved =
        shepr_core::shell::resolve_executable(
            candidate,
            path,
            cwd,
            |path| match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => shepr_core::shell::ExecutableStatus::Directory,
                Ok(_) if shepr_platform::has_execute_access(path) => {
                    shepr_core::shell::ExecutableStatus::Executable
                }
                Ok(_) => shepr_core::shell::ExecutableStatus::NotExecutable,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    shepr_core::shell::ExecutableStatus::Missing
                }
                Err(error) => shepr_core::shell::ExecutableStatus::Uninspectable(error.kind()),
            },
        )
        .map_err(|error| format!("{source} {error}"))?;

    if !resolved
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(shepr_agent::detect::is_pane_shell_process_name)
    {
        return Err(format!(
            "{source} resolves to a shell name shepr does not recognize: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn shell_path_string(path: PathBuf, source: &str) -> Result<String, String> {
    path.into_os_string().into_string().map_err(|path| {
        format!(
            "{source} resolves to a non-UTF-8 path: {}",
            path.to_string_lossy()
        )
    })
}

fn checked_new_cwd_directory(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("terminal.new_cwd must resolve to an absolute path".to_owned());
    }
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "terminal.new_cwd directory {} is unavailable: {error}",
            path.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "terminal.new_cwd must name an existing directory: {}",
            path.display()
        ));
    }
    Ok(path.to_path_buf())
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
    pub(crate) path_diagnostics: Vec<String>,
    pub(crate) values: Option<ValidatedValues>,
}

impl ConfigResolution {
    pub(crate) fn parse(
        config: &Config,
        provenance: &ConfigProvenance,
        paths: &AppPaths,
        cwd_check: CwdCheck,
        shell_check: ShellCheck,
    ) -> Self {
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
        let terminal =
            ValidatedTerminalConfig::parse(&config.terminal, paths, cwd_check, shell_check);
        let mouse_scroll_lines = u16::try_from(config.ui.mouse_scroll_lines())
            .ok()
            .filter(|lines| *lines >= MIN_MOUSE_SCROLL_LINES)
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
                "ui.mouse_scroll_lines must be between {MIN_MOUSE_SCROLL_LINES} and {MAX_MOUSE_SCROLL_LINES} (got {})",
                config.ui.mouse_scroll_lines()
            ));
        }
        let mut path_diagnostics = Vec::new();
        if let Err(errors) = &terminal {
            path_diagnostics.extend(errors.iter().cloned());
        }

        let values = if diagnostics.is_empty() && path_diagnostics.is_empty() {
            match (
                keybind_validation.live,
                palette,
                headless_size,
                sidebar_bounds,
                mouse_scroll_lines,
                tab_bar_right,
                window_title,
                terminal,
            ) {
                (
                    Some(live_keybinds),
                    Ok(palette),
                    Some(headless_size),
                    Some(sidebar_bounds),
                    Some(mouse_scroll_lines),
                    Ok(tab_bar_right),
                    Ok(window_title),
                    Ok(terminal),
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
                    terminal,
                }),
                _ => None,
            }
        } else {
            None
        };

        Self {
            diagnostics,
            path_diagnostics,
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
    /// Validate `config` exactly as a launch does, with `source` as the config
    /// document the values were read from: it decides which values count as
    /// explicitly configured, and `None` makes every value a default. The
    /// document is not checked for unknown keys; a launch load does that
    /// before it gets here.
    pub fn from_values(
        config: Config,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Result<Self, Vec<String>> {
        let document = source
            .map(|source| {
                source
                    .parse::<toml::Table>()
                    .map(toml::Value::Table)
                    .map_err(|error| vec![format!("config parse error: {error}")])
            })
            .transpose()?;
        let provenance = ConfigProvenance::from_config(&config, document.as_ref())
            .map_err(|error| vec![format!("config provenance error: {error}")])?;
        Self::from_resolution(
            config,
            provenance,
            paths,
            CwdCheck::AtLaunch,
            ShellCheck::AtLaunch,
        )
    }

    /// Build from a load whose diagnostics, including checked terminal paths,
    /// are already empty.
    pub(crate) fn from_loaded(
        config: Config,
        provenance: ConfigProvenance,
        values: ValidatedValues,
        paths: AppPaths,
    ) -> Self {
        let mut config = config;
        // The wire config carries the selected path so a receiver does not
        // resolve the sender's shell against its own search path or inherited
        // shell variable.
        config.terminal.default_shell = values.terminal.default_shell.clone();
        Self {
            config,
            provenance,
            paths,
            resolved_palette: values.palette,
            headless_size: values.headless_size,
            live_keybinds: values.live_keybinds,
            ui: values.ui,
            terminal: values.terminal,
        }
    }

    fn from_resolution(
        config: Config,
        provenance: ConfigProvenance,
        paths: AppPaths,
        cwd_check: CwdCheck,
        shell_check: ShellCheck,
    ) -> Result<Self, Vec<String>> {
        let resolution =
            ConfigResolution::parse(&config, &provenance, &paths, cwd_check, shell_check);
        let mut diagnostics = resolution.diagnostics;
        diagnostics.extend(resolution.path_diagnostics);
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        match resolution.values {
            Some(values) => Ok(Self::from_loaded(config, provenance, values, paths)),
            None => Err(vec![
                "configuration resolution produced no values and no diagnostics".to_owned(),
            ]),
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

    pub fn same_keybinding_resolution(&self, other: &Self) -> bool {
        self.config.keys == other.config.keys
            && self
                .provenance
                .keybinding_values()
                .eq(other.provenance.keybinding_values())
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

// Keep the received-value adapters explicit across protocol and config:
// geometry, addresses, and paths are field mirrors with distinct checks,
// while this wire shape omits runtime caches and rebuilds them with
// received-value rules. A local macro per crate would duplicate its generator;
// a shared derive needs a new proc-macro crate and per-type hooks. The explicit
// Wire literals already make field drift a compile-time error, so that
// machinery would cost more than it removes.
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
        // Rebuild runtime values from the serialized config. The shell path was
        // resolved by the sender and is preserved; paths are also the sender's,
        // possibly on another host, so new_cwd is not looked up here.
        Self::from_resolution(
            config,
            provenance,
            paths,
            CwdCheck::Received,
            ShellCheck::Received,
        )
        .map_err(|diagnostics| de::Error::custom(diagnostics.join("\n")))
    }
}

#[cfg(test)]
impl ValidatedConfig {
    pub fn new(
        config: Config,
        provenance: ConfigProvenance,
        paths: AppPaths,
    ) -> Result<Self, Vec<String>> {
        Self::from_resolution(
            config,
            provenance,
            paths,
            CwdCheck::AtLaunch,
            ShellCheck::AtLaunch,
        )
    }

    pub fn validated_live_keybinds(&self) -> Result<super::LiveKeybindConfig, Vec<String>> {
        Ok(self.live_keybinds())
    }

    pub fn test_from_config_with_paths(
        config: Config,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Self {
        Self::from_values(config, source, paths).expect("test config is valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_provenance_includes_absent_sidebar_rule_fields() {
        let config: Config = toml::from_str(
            r#"
[ui.sidebar.agents]
rows = [[{ token = "workspace", rules = [{ equals = "local" }] }, { token = "agent" }]]
"#,
        )
        .expect("test config");
        let encoded = toml::to_string(&config).expect("config serializes");
        let decoded = toml::from_str::<Config>(&encoded).expect("serialized config parses");
        assert_eq!(decoded, config);
        let provenance = ConfigProvenance::from_config(&config, None).expect("provenance");
        let keys = provenance
            .values()
            .iter()
            .map(|origin| origin.key.as_str())
            .collect::<Vec<_>>();

        for field in [
            "equals",
            "contains",
            "starts_with",
            "gt",
            "lt",
            "ignore_case",
            "fg",
            "bold",
            "dim",
            "hide",
        ] {
            let key = format!("ui.sidebar.agents.rows[0][0].rules[0].{field}");
            assert!(keys.contains(&key.as_str()), "missing {key}");
        }
        for field in ["fg", "bold", "dim"] {
            let key = format!("ui.sidebar.agents.rows[0][0].{field}");
            assert!(keys.contains(&key.as_str()), "missing {key}");
        }
        assert!(keys.contains(&"ui.sidebar.agents.rows[0][1].rules"));
    }

    #[test]
    fn validated_config_resolves_runtime_values_once() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-cwd");
        let configured_cwd = scratch.join("relative/worktree");
        std::fs::create_dir_all(&configured_cwd).expect("create configured cwd");
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
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));

        let validated =
            ValidatedConfig::new(config, provenance, paths).expect("test configuration is valid");

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
            NewTerminalCwd::Path(configured_cwd)
        );
    }

    #[test]
    fn validated_config_expands_home_in_new_cwd_path_once() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-home-cwd");
        let home = scratch.join("home");
        let configured_cwd = home.join("work");
        std::fs::create_dir_all(&configured_cwd).expect("create home cwd");
        let mut config = Config::default();
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("~/work".to_owned());
        let provenance = ConfigProvenance::defaults(&config);
        let paths = AppPaths::rooted_at(scratch.path(), Some(&home), Some(scratch.path()));

        let validated =
            ValidatedConfig::new(config, provenance, paths).expect("test configuration is valid");

        assert_eq!(
            validated.terminal().new_cwd,
            NewTerminalCwd::Path(configured_cwd)
        );
    }

    #[test]
    fn validated_config_rejects_empty_and_missing_new_cwd_paths() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-invalid-cwd");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));

        for (path, expected) in [("", "must not be empty"), ("missing", "unavailable")] {
            let mut config = Config::default();
            config.terminal.new_cwd = NewTerminalCwdConfig::Path(path.to_owned());
            let error = ValidatedConfig::new(
                config.clone(),
                ConfigProvenance::defaults(&config),
                paths.clone(),
            )
            .expect_err("invalid cwd must fail config parsing");
            assert!(
                error.iter().any(|message| message.contains(expected)),
                "expected {expected:?} in {error:?}"
            );
        }
    }

    #[test]
    fn validation_reports_shell_and_new_cwd_errors_together() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-multiple-path-errors");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        let mut config = Config::default();
        config.terminal.default_shell = scratch.join("missing/zsh").to_string_lossy().into_owned();
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("missing-cwd".to_owned());

        let errors =
            ValidatedConfig::new(config.clone(), ConfigProvenance::defaults(&config), paths)
                .expect_err("both invalid terminal paths should be reported");

        assert!(
            errors
                .iter()
                .any(|message| message.contains("terminal.default_shell")),
            "shell error missing from {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|message| message.contains("terminal.new_cwd")),
            "new cwd error missing from {errors:?}"
        );
    }

    #[test]
    fn launch_rejects_a_configured_shell_that_is_missing_or_unrecognised() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-shell");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        let not_a_shell = shepr_test_support::fixture::stand_in(scratch.path(), "not-a-shell", &[]);
        let non_executable_shell = scratch.join("zsh");
        std::fs::write(&non_executable_shell, "not launched").expect("test precondition");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &non_executable_shell,
                std::fs::Permissions::from_mode(0o644),
            )
            .expect("test precondition");
        }

        for (shell, expected) in [
            (scratch.join("missing/zsh"), "terminal.default_shell"),
            (non_executable_shell, "is not executable"),
            (not_a_shell, "does not recognize"),
        ] {
            let mut config = Config::default();
            config.terminal.default_shell = shell.to_string_lossy().into_owned();
            let error = ValidatedConfig::new(
                config.clone(),
                ConfigProvenance::defaults(&config),
                paths.clone(),
            )
            .expect_err("an unusable configured shell fails the launch");
            assert!(
                error.iter().any(|message| message.contains(expected)),
                "expected {expected:?} in {error:?}"
            );
        }
    }

    #[test]
    fn a_configured_shell_wins_over_an_unusable_inherited_shell() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-shell-override");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        let inherited = scratch.join("missing/inherited-shell");
        env.set("SHELL", &inherited);
        let configured = shepr_test_support::fixture::stand_in(scratch.path(), "zsh", &[]);
        let configured_shell = configured.to_string_lossy().into_owned();
        let mut config = Config::default();
        config.terminal.default_shell = configured_shell.clone();

        let validated =
            ValidatedConfig::new(config.clone(), ConfigProvenance::defaults(&config), paths)
                .expect("a configured shell takes precedence over inherited SHELL");

        assert_eq!(validated.terminal().default_shell, configured_shell);
    }

    /// With `terminal.default_shell` empty, `SHELL` is the setting: a usable
    /// one is taken, an unusable or unrecognized one fails the launch naming
    /// `SHELL` and the fix, and only an unset one means `/bin/sh`.
    #[test]
    fn an_empty_shell_setting_takes_the_inherited_shell_and_rejects_an_unusable_one() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-inherited-shell");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        let config = Config::default();
        let validate = || {
            ValidatedConfig::new(
                config.clone(),
                ConfigProvenance::defaults(&config),
                paths.clone(),
            )
        };

        let zsh = shepr_test_support::fixture::stand_in(scratch.path(), "zsh", &[]);
        env.set("SHELL", &zsh);
        let validated = validate().expect("a usable inherited shell is taken");
        assert_eq!(
            Some(validated.terminal().default_shell.as_str()),
            zsh.to_str()
        );

        let not_a_shell = shepr_test_support::fixture::stand_in(scratch.path(), "not-a-shell", &[]);
        for (shell, expected) in [
            (scratch.join("missing/zsh"), "does not exist"),
            (not_a_shell, "does not recognize"),
        ] {
            env.set("SHELL", &shell);
            let errors = validate().expect_err("an unusable SHELL fails the launch");
            let shell = shell.to_string_lossy();
            assert!(
                errors.iter().any(|message| message.starts_with("SHELL ")
                    && message.contains(expected)
                    && message.contains(&format!("SHELL={shell}"))
                    && message.contains("Set terminal.default_shell")),
                "expected a SHELL diagnostic with {expected:?} in {errors:?}"
            );
        }

        env.remove("SHELL");
        let validated = validate().expect("an unset SHELL means /bin/sh");
        assert_eq!(validated.terminal().default_shell, "/bin/sh");
    }

    #[test]
    fn wire_deserialization_revalidates_raw_config() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-wire");
        let validated = ValidatedConfig::test_from_config_with_paths(
            Config::default(),
            None,
            AppPaths::rooted_at(scratch.path(), Some(scratch.path()), None),
        );
        let mut wire = serde_json::to_value(validated).expect("serialize test config");
        wire["config"]["server"]["headless_cols"] = serde_json::json!(0);

        assert!(serde_json::from_value::<ValidatedConfig>(wire).is_err());
    }

    #[test]
    fn wire_deserialization_rejects_missing_captured_home_directory() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-home-wire");
        let validated = ValidatedConfig::test_from_config_with_paths(
            Config::default(),
            None,
            AppPaths::rooted_at(scratch.path(), Some(scratch.path()), None),
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

    #[test]
    fn wire_deserialization_does_not_look_up_the_senders_cwd_directory() {
        let _env = shepr_test_support::IsolatedEnv::new();
        // A remote server's new_cwd names a directory on the remote host; the
        // receiving client must not require it on its own filesystem.
        let scratch = shepr_test_support::ScratchDir::new("validated-config-cwd-wire");
        let sender_dir = scratch.join("sender/project");
        std::fs::create_dir_all(&sender_dir).expect("create sender cwd");
        let mut config = Config::default();
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("project".to_owned());
        let paths = AppPaths::rooted_at(
            scratch.path(),
            Some(scratch.path()),
            Some(&scratch.join("sender")),
        );
        let validated =
            ValidatedConfig::new(config.clone(), ConfigProvenance::defaults(&config), paths)
                .expect("the sender's directory exists");
        let wire = serde_json::to_value(validated).expect("serialize test config");
        std::fs::remove_dir_all(scratch.join("sender")).expect("remove sender cwd");

        let received =
            serde_json::from_value::<ValidatedConfig>(wire).expect("receiver skips the lookup");
        assert_eq!(
            received.terminal().new_cwd,
            NewTerminalCwd::Path(sender_dir)
        );
    }
}
