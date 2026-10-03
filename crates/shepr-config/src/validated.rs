use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use shepr_core::shell::ResolvedShell;

use super::{
    AppPaths, ClientConfig, SidebarBounds,
    model::{
        AdvancedConfig, ClientUiConfig, ExperimentalConfig, NewTerminalCwdConfig, SessionConfig,
        TerminalConfig,
    },
    window_title::WindowTitleTemplate,
};
use crate::limits::{MAX_INPUT_EVENT_BATCH, MIN_MOUSE_SCROLL_LINES};

/// Client preferences that yield to explicit configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiPreferenceKey {
    SidebarWidth,
    SidebarStartCollapsed,
    AgentPanelSort,
}

/// Explicit document key paths used by keybinding validation and by client
/// chrome preferences that yield to configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProvenance {
    explicit_paths: std::collections::BTreeSet<String>,
}

impl ConfigProvenance {
    pub(crate) fn from_document(document: Option<&toml::Value>) -> Self {
        let mut paths = Vec::new();
        if let Some(document) = document {
            collect_toml_paths(document, &mut Vec::new(), &mut paths);
        }
        Self {
            explicit_paths: paths.into_iter().collect(),
        }
    }

    pub fn is_explicit(&self, key: UiPreferenceKey) -> bool {
        self.key_is_configured(match key {
            UiPreferenceKey::SidebarWidth => "ui.sidebar_width",
            UiPreferenceKey::SidebarStartCollapsed => "ui.sidebar_start_collapsed",
            UiPreferenceKey::AgentPanelSort => "ui.agent_panel_sort",
        })
    }

    pub(crate) fn key_is_configured(&self, key: &str) -> bool {
        self.explicit_paths.iter().any(|path| {
            path.strip_prefix(key).is_some_and(|suffix| {
                suffix.is_empty() || suffix.starts_with('.') || suffix.starts_with('[')
            })
        })
    }

    pub(crate) fn defaults() -> Self {
        Self::from_document(None)
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
pub struct ValidatedClientUiConfig {
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
    pub prompt_new_workspace_name: bool,
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
    pub default_shell: ResolvedShell,
    pub login_shell: bool,
    pub new_cwd: NewTerminalCwd,
}

impl ValidatedTerminalConfig {
    fn parse(config: &TerminalConfig, paths: &AppPaths) -> Result<Self, Vec<String>> {
        let default_shell = resolve_default_shell(&config.default_shell, paths);
        let new_cwd = Self::parse_new_cwd(&config.new_cwd, paths);
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
    ) -> Result<NewTerminalCwd, String> {
        match configured {
            NewTerminalCwdConfig::Follow => Ok(NewTerminalCwd::Follow),
            NewTerminalCwdConfig::Home => {
                let path = paths.home_dir().ok_or_else(|| {
                    format!(
                        "terminal.new_cwd cannot be resolved: {}",
                        shepr_core::pathutil::missing_home_error()
                    )
                })?;
                checked_new_cwd_directory(path)?;
                Ok(NewTerminalCwd::Home)
            }
            NewTerminalCwdConfig::Current => {
                let path = paths.current_dir().ok_or_else(|| {
                    "terminal.new_cwd current directory was unavailable at launch".to_owned()
                })?;
                checked_new_cwd_directory(path)?;
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
                checked_new_cwd_directory(&absolute).map(NewTerminalCwd::Path)
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
fn resolve_default_shell(configured: &str, paths: &AppPaths) -> Result<ResolvedShell, String> {
    let path = shepr_core::env::read_os(shepr_core::env::EnvVar::Path)
        .map_err(|error| error.to_string())?;
    let cwd = paths
        .current_dir()
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));

    if !configured.is_empty() {
        return resolve_recognized_shell(
            OsStr::new(configured),
            "terminal.default_shell",
            path.as_deref(),
            &cwd,
        );
    }

    let inherited = shepr_core::env::read_os(shepr_core::env::EnvVar::Shell)
        .map_err(|error| error.to_string())?
        .filter(|shell| !shell.is_empty());
    if let Some(inherited) = inherited {
        return resolve_recognized_shell(&inherited, "SHELL", path.as_deref(), &cwd).map_err(
            |error| {
                format!(
                    "{error}; terminal.default_shell is empty, so panes run SHELL={}. \
                     Set terminal.default_shell to a shell shepr recognizes, or fix SHELL",
                    inherited.to_string_lossy()
                )
            },
        );
    }

    resolve_recognized_shell(
        OsStr::new("/bin/sh"),
        "the default shell",
        path.as_deref(),
        &cwd,
    )
}

/// `source` names where `candidate` came from, so the error points at the
/// setting or variable to fix.
fn resolve_recognized_shell(
    candidate: &OsStr,
    source: &str,
    path: Option<&OsStr>,
    cwd: &Path,
) -> Result<ResolvedShell, String> {
    check_shell_whitespace(candidate, source)?;
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

    ResolvedShell::validate(resolved, |resolved| {
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
        Ok(())
    })
}

/// Shell settings follow the same interpreted-value policy for config and
/// environment input. Non-UTF-8 paths retain their bytes; ASCII edge whitespace
/// is still rejected.
fn check_shell_whitespace(value: &OsStr, source: &str) -> Result<(), String> {
    let padded = value.to_str().map_or_else(
        || {
            value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_whitespace)
                || value.as_bytes().last().is_some_and(u8::is_ascii_whitespace)
        },
        |value| value.trim() != value,
    );
    if padded {
        return Err(format!("{source} must not have surrounding whitespace"));
    }
    Ok(())
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

impl ValidatedClientUiConfig {
    /// Configured expanded sidebar width, already validated to be within
    /// `sidebar_bounds`.
    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width
    }

    pub fn sidebar_bounds(&self) -> SidebarBounds {
        self.sidebar_bounds
    }

    fn from_config(
        config: &ClientUiConfig,
        bounds: SidebarBounds,
        mouse_scroll_lines: std::num::NonZeroU16,
    ) -> Self {
        Self {
            sidebar_width: config.sidebar_width,
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
            prompt_new_workspace_name: config.prompt_new_workspace_name,
            agent_panel_sort: config.agent_panel_sort,
            status_indicators: config.status_indicators,
            sidebar: config.sidebar.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedClientValues {
    pub(crate) palette: crate::theme::Palette,
    pub(crate) live_keybinds: super::LiveKeybindConfig,
    pub(crate) ui: ValidatedClientUiConfig,
}

/// One diagnostic per machine whose label an earlier entry already uses. Labels
/// are the machines' identifiers, so they must be unique; blank labels and
/// malformed SSH targets cannot reach here, as the types refuse them.
fn machine_label_diagnostics(machines: &[super::MachineConfig]) -> Vec<String> {
    let mut seen = std::collections::HashMap::new();
    let mut diagnostics = Vec::new();
    for (index, machine) in machines.iter().enumerate() {
        let first = *seen.entry(&machine.label).or_insert(index);
        if first != index {
            diagnostics.push(format!(
                "machines[{index}] label {:?} duplicates machines[{first}]",
                machine.label.as_str()
            ));
        }
    }
    diagnostics
}

/// Results of parsing each value used at runtime, plus every diagnostic found.
/// No fallback values escape this boundary: `values` is present only when all
/// parsed fields are valid.
#[derive(Debug, Clone)]
pub(crate) struct ClientConfigResolution {
    pub(crate) diagnostics: Vec<String>,
    pub(crate) values: Option<ValidatedClientValues>,
}

impl ClientConfigResolution {
    pub(crate) fn parse(config: &ClientConfig, provenance: &ConfigProvenance) -> Self {
        let keybind_validation = config.compute_keybind_validation(|field| {
            provenance.key_is_configured(&format!("keys.{field}"))
        });
        let palette = config.resolve_palette();
        let sidebar_bounds = super::validated_sidebar_bounds(
            config.ui.sidebar_min_width,
            config.ui.sidebar_max_width,
        );
        let mouse_scroll_lines = config.ui.mouse_scroll_lines();
        let mouse_scroll_lines = u16::try_from(mouse_scroll_lines)
            .ok()
            .filter(|lines| {
                (usize::from(MIN_MOUSE_SCROLL_LINES)..=MAX_INPUT_EVENT_BATCH)
                    .contains(&usize::from(*lines))
            })
            .and_then(std::num::NonZeroU16::new);

        let mut diagnostics = keybind_validation.diagnostics.clone();
        if let Err(errors) = &palette {
            diagnostics.extend(errors.iter().cloned());
        }
        if sidebar_bounds.is_none() {
            diagnostics.push(format!(
                "ui.sidebar_min_width ({}) is greater than sidebar_max_width ({})",
                config.ui.sidebar_min_width, config.ui.sidebar_max_width
            ));
        } else if !(config.ui.sidebar_min_width..=config.ui.sidebar_max_width)
            .contains(&config.ui.sidebar_width)
        {
            diagnostics.push(format!(
                "ui.sidebar_width ({}) must be between sidebar_min_width and sidebar_max_width",
                config.ui.sidebar_width
            ));
        }
        if mouse_scroll_lines.is_none() {
            diagnostics.push(format!(
                "ui.mouse_scroll_lines must be between {MIN_MOUSE_SCROLL_LINES} and {MAX_INPUT_EVENT_BATCH} (got {})",
                config.ui.mouse_scroll_lines()
            ));
        }
        diagnostics.extend(machine_label_diagnostics(&config.machines));
        let values = if diagnostics.is_empty() {
            match (
                keybind_validation.live,
                palette,
                sidebar_bounds,
                mouse_scroll_lines,
            ) {
                (
                    Some(live_keybinds),
                    Ok(palette),
                    Some(sidebar_bounds),
                    Some(mouse_scroll_lines),
                ) => Some(ValidatedClientValues {
                    palette,
                    live_keybinds,
                    ui: ValidatedClientUiConfig::from_config(
                        &config.ui,
                        sidebar_bounds,
                        mouse_scroll_lines,
                    ),
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
pub struct ValidatedClientConfig {
    config: ClientConfig,
    provenance: ConfigProvenance,
    paths: AppPaths,
    resolved_palette: crate::theme::Palette,
    live_keybinds: super::LiveKeybindConfig,
    ui: ValidatedClientUiConfig,
}

impl ValidatedClientConfig {
    /// Validate `config` exactly as a launch does, with `source` as the config
    /// document the values were read from: it decides which values count as
    /// explicitly configured, and `None` makes every value a default. The
    /// document is not checked for unknown keys; a launch load does that
    /// before it gets here.
    pub fn from_values(
        config: ClientConfig,
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
        let provenance = ConfigProvenance::from_document(document.as_ref());
        Self::from_resolution(config, provenance, paths)
    }

    /// Build from a load whose diagnostics, including checked terminal paths,
    /// are already empty.
    pub(crate) fn from_loaded(
        config: ClientConfig,
        provenance: ConfigProvenance,
        values: ValidatedClientValues,
        paths: AppPaths,
    ) -> Self {
        Self {
            config,
            provenance,
            paths,
            resolved_palette: values.palette,
            live_keybinds: values.live_keybinds,
            ui: values.ui,
        }
    }

    fn from_resolution(
        config: ClientConfig,
        provenance: ConfigProvenance,
        paths: AppPaths,
    ) -> Result<Self, Vec<String>> {
        let resolution = ClientConfigResolution::parse(&config, &provenance);
        let diagnostics = resolution.diagnostics;
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

    pub fn ui(&self) -> &ValidatedClientUiConfig {
        &self.ui
    }

    /// The configured machines, in config order. Labels are unique.
    pub fn machines(&self) -> &[super::MachineConfig] {
        &self.config.machines
    }

    pub fn live_keybinds(&self) -> super::LiveKeybindConfig {
        self.live_keybinds.clone()
    }
}

/// Pane chrome resolved by the server at launch.
#[derive(Debug, Clone)]
pub struct ValidatedServerUiConfig {
    pub pane_borders: super::PaneBordersConfig,
    pub pane_outer_borders: bool,
    pub pane_scrollbars: bool,
    pub pane_gaps: bool,
    pub show_agent_labels_on_pane_borders: bool,
    pub window_title: Option<WindowTitleTemplate>,
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedServerValues {
    pub(crate) headless_size: shepr_core::geometry::GridSize,
    pub(crate) palette: crate::theme::Palette,
    pub(crate) ui: ValidatedServerUiConfig,
    pub(crate) terminal: ValidatedTerminalConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerConfigResolution {
    pub(crate) diagnostics: Vec<String>,
    pub(crate) path_diagnostics: Vec<String>,
    pub(crate) values: Option<ValidatedServerValues>,
}

impl ServerConfigResolution {
    pub(crate) fn parse(config: &super::ServerConfig, paths: &AppPaths) -> Self {
        Self::parse_with_terminal(
            config,
            ValidatedTerminalConfig::parse(&config.terminal, paths),
        )
    }

    fn parse_with_terminal(
        config: &super::ServerConfig,
        terminal: Result<ValidatedTerminalConfig, Vec<String>>,
    ) -> Self {
        let palette = config.resolve_palette();
        let headless_size = shepr_core::geometry::GridSize::new(
            config.server.headless_cols,
            config.server.headless_rows,
        )
        .filter(|_| {
            super::terminal_grid_cells(config.server.headless_cols, config.server.headless_rows)
                .is_some()
        });
        let window_title = WindowTitleTemplate::parse(&config.ui.window_title);
        let mut diagnostics = Vec::new();
        if let Err(errors) = &palette {
            diagnostics.extend(errors.iter().cloned());
        }
        if let Err(error) = &window_title {
            diagnostics.push(format!("ui.window_title {error}"));
        }
        if headless_size.is_none() {
            diagnostics.push(format!(
                "server.headless_cols and server.headless_rows must be greater than zero, each no larger than {}, and no larger than {} cells combined (got {}x{})",
                super::MAX_TERMINAL_GRID_DIMENSION, super::MAX_TERMINAL_GRID_CELLS,
                config.server.headless_cols, config.server.headless_rows));
        }
        let path_diagnostics = terminal.as_ref().err().cloned().unwrap_or_default();
        let values = if diagnostics.is_empty() && path_diagnostics.is_empty() {
            match (palette, headless_size, window_title, terminal) {
                (Ok(palette), Some(headless_size), Ok(window_title), Ok(terminal)) => {
                    Some(ValidatedServerValues {
                        palette,
                        headless_size,
                        terminal,
                        ui: ValidatedServerUiConfig {
                            pane_borders: config.ui.pane_borders,
                            pane_outer_borders: config.ui.pane_outer_borders,
                            pane_scrollbars: config.ui.pane_scrollbars,
                            pane_gaps: config.ui.pane_gaps,
                            show_agent_labels_on_pane_borders: config
                                .ui
                                .show_agent_labels_on_pane_borders,
                            window_title,
                        },
                    })
                }
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

/// Immutable server configuration, with no client settings.
#[derive(Debug, Clone)]
pub struct ValidatedServerConfig {
    config: super::ServerConfig,
    paths: AppPaths,
    values: ValidatedServerValues,
}

impl ValidatedServerConfig {
    /// Validate server values against the launch context. A launch loader also
    /// checks unknown document keys before constructing this value. Server
    /// validation has no fields that depend on the source document.
    pub fn from_values(config: super::ServerConfig, paths: AppPaths) -> Result<Self, Vec<String>> {
        let resolution = ServerConfigResolution::parse(&config, &paths);
        let mut errors = resolution.diagnostics;
        errors.extend(resolution.path_diagnostics);
        if !errors.is_empty() {
            return Err(errors);
        }
        match resolution.values {
            Some(values) => Ok(Self::from_loaded(config, values, paths)),
            None => Err(vec![
                "configuration resolution produced no values and no diagnostics".to_owned(),
            ]),
        }
    }
    pub(crate) fn from_loaded(
        config: super::ServerConfig,
        values: ValidatedServerValues,
        paths: AppPaths,
    ) -> Self {
        Self {
            config,
            paths,
            values,
        }
    }
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }
    pub fn palette(&self) -> &crate::theme::Palette {
        &self.values.palette
    }
    pub fn headless_size(&self) -> shepr_core::geometry::GridSize {
        self.values.headless_size
    }
    pub fn ui(&self) -> &ValidatedServerUiConfig {
        &self.values.ui
    }
    pub fn terminal(&self) -> &ValidatedTerminalConfig {
        &self.values.terminal
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
}

#[cfg(test)]
impl ValidatedClientConfig {
    pub fn validated_live_keybinds(&self) -> Result<super::LiveKeybindConfig, Vec<String>> {
        Ok(self.live_keybinds())
    }
}

#[cfg(test)]
impl ServerConfigResolution {
    pub(crate) fn parse_document(config: &super::ServerConfig, paths: &AppPaths) -> Self {
        let terminal = ValidatedTerminalConfig::parse_new_cwd(&config.terminal.new_cwd, paths)
            .map(|new_cwd| ValidatedTerminalConfig {
                default_shell: ResolvedShell::validate("/bin/sh".into(), |_| Ok(()))
                    .expect("absolute test shell"),
                login_shell: config.terminal.login_shell,
                new_cwd,
            })
            .map_err(|error| vec![error]);
        Self::parse_with_terminal(config, terminal)
    }
}

#[cfg(test)]
impl ValidatedServerConfig {
    pub fn new(config: super::ServerConfig, paths: AppPaths) -> Result<Self, Vec<String>> {
        Self::from_values(config, paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerConfig;

    #[test]
    fn accent_value_applies_without_an_explicit_document() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("accent-value");
        let mut config = ClientConfig::default();
        config.theme.accent = Some("#123456".into());
        let validated = ValidatedClientConfig::from_values(
            config,
            None,
            AppPaths::rooted_at(scratch.path(), Some(scratch.path()), None),
        )
        .expect("valid accent");
        assert_eq!(
            validated.palette().accent,
            ratatui::style::Color::Rgb(0x12, 0x34, 0x56)
        );
    }

    #[test]
    fn validated_config_resolves_runtime_values_once() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-cwd");
        let configured_cwd = scratch.join("relative/worktree");
        std::fs::create_dir_all(&configured_cwd).expect("create configured cwd");
        let mut config = ServerConfig::default();
        config.server.headless_cols = 92;
        config.server.headless_rows = 31;
        config.ui.window_title = "{hostname}: {workspace}".to_owned();
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("relative/worktree".to_owned());
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));

        let validated =
            ValidatedServerConfig::new(config, paths).expect("test configuration is valid");

        assert_eq!(
            validated.headless_size(),
            shepr_core::geometry::GridSize::new(92, 31).expect("non-zero test dimensions")
        );
        assert!(validated.ui().window_title.is_some());
        assert_eq!(
            validated.terminal().new_cwd,
            NewTerminalCwd::Path(configured_cwd)
        );
    }

    #[test]
    fn validated_config_accepts_the_maximum_shared_headless_grid() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-max-grid");
        let mut config = ServerConfig::default();
        let cols = crate::MAX_TERMINAL_GRID_DIMENSION;
        let rows = u16::try_from(crate::MAX_TERMINAL_GRID_CELLS / usize::from(cols))
            .expect("the shared grid budget fits in u16 rows");
        config.server.headless_cols = cols;
        config.server.headless_rows = rows;
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));

        let validated = ValidatedServerConfig::new(config, paths)
            .expect("the exact shared terminal cell budget is valid");

        assert_eq!(
            validated.headless_size(),
            shepr_core::geometry::GridSize::new(cols, rows).expect("non-zero grid")
        );
    }

    #[test]
    fn validated_config_expands_home_in_new_cwd_path_once() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-home-cwd");
        let home = scratch.join("home");
        let configured_cwd = home.join("work");
        std::fs::create_dir_all(&configured_cwd).expect("create home cwd");
        let mut config = ServerConfig::default();
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("~/work".to_owned());
        let paths = AppPaths::rooted_at(scratch.path(), Some(&home), Some(scratch.path()));

        let validated =
            ValidatedServerConfig::new(config, paths).expect("test configuration is valid");

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
            let mut config = ServerConfig::default();
            config.terminal.new_cwd = NewTerminalCwdConfig::Path(path.to_owned());
            let error = ValidatedServerConfig::new(config.clone(), paths.clone())
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
        let mut config = ServerConfig::default();
        config.terminal.default_shell = scratch.join("missing/zsh").to_string_lossy().into_owned();
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("missing-cwd".to_owned());

        let errors = ValidatedServerConfig::new(config.clone(), paths)
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
            let mut config = ServerConfig::default();
            config.terminal.default_shell = shell.to_string_lossy().into_owned();
            let error = ValidatedServerConfig::new(config.clone(), paths.clone())
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
        let mut config = ServerConfig::default();
        config.terminal.default_shell = configured_shell.clone();

        let validated = ValidatedServerConfig::new(config.clone(), paths)
            .expect("a configured shell takes precedence over inherited SHELL");

        assert_eq!(
            validated.terminal().default_shell.path(),
            Path::new(&configured_shell)
        );
    }

    #[test]
    fn shell_inputs_reject_surrounding_whitespace_without_trimming() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("shell-whitespace");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        for value in [" /bin/sh", "/bin/sh\t", " ", "\u{2003}/bin/sh"] {
            assert!(
                resolve_default_shell(value, &paths)
                    .expect_err("padded configured shell")
                    .contains("terminal.default_shell must not have surrounding whitespace")
            );
            env.set("SHELL", value);
            assert!(
                resolve_default_shell("", &paths)
                    .expect_err("padded inherited shell")
                    .contains("SHELL must not have surrounding whitespace")
            );
        }
    }

    #[test]
    fn a_non_utf8_shell_directory_keeps_its_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("shell-path-bytes");
        let directory = scratch.join(std::ffi::OsString::from_vec(b"shell-\xff".to_vec()));
        std::fs::create_dir(&directory).expect("create non-UTF-8 directory");
        let shell = shepr_test_support::fixture::stand_in(&directory, "zsh", &[]);
        env.set("SHELL", &shell);
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        assert_eq!(
            resolve_default_shell("", &paths)
                .expect("valid shell")
                .path(),
            shell
        );
    }

    /// With `terminal.default_shell` empty, `SHELL` is the setting: a usable
    /// one is taken, an unusable or unrecognized one fails the launch naming
    /// `SHELL` and the fix, and only an unset one means `/bin/sh`.
    #[test]
    fn an_empty_shell_setting_takes_the_inherited_shell_and_rejects_an_unusable_one() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-inherited-shell");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        let config = ServerConfig::default();
        let validate = || ValidatedServerConfig::new(config.clone(), paths.clone());

        let zsh = shepr_test_support::fixture::stand_in(scratch.path(), "zsh", &[]);
        env.set("SHELL", &zsh);
        let validated = validate().expect("a usable inherited shell is taken");
        assert_eq!(validated.terminal().default_shell.path(), zsh.as_path());

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
        assert_eq!(
            validated.terminal().default_shell.path(),
            Path::new("/bin/sh")
        );
    }
}
