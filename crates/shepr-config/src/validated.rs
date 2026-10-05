use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use shepr_core::geometry::BoundedGridSize;
use shepr_core::limits::{MAX_TERMINAL_GRID_CELLS, MAX_TERMINAL_GRID_DIMENSION};
use shepr_core::shell::ResolvedShell;
use shepr_paths::AppPaths;

use super::{
    ClientConfig, SidebarBounds,
    model::{ClientUiConfig, ImeCursorShape, NewTerminalCwdConfig, TerminalConfig},
};
use crate::limits::DEFAULT_SIDEBAR_WIDTH;

/// A value paired with whether it came from the document or the built-in
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting<T> {
    Explicit(T),
    Default(T),
}

impl<T> Setting<T> {
    pub fn value(&self) -> &T {
        match self {
            Self::Explicit(value) | Self::Default(value) => value,
        }
    }

    pub fn into_value(self) -> T {
        match self {
            Self::Explicit(value) | Self::Default(value) => value,
        }
    }

    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit(_))
    }
}

/// Explicit document key paths used to distinguish configured keybindings
/// from their built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProvenance {
    explicit_paths: std::collections::BTreeSet<super::ConfigKeyPath>,
}

impl ConfigProvenance {
    pub(crate) fn from_document(document: Option<&toml::Value>) -> Self {
        let mut paths = Vec::new();
        if let Some(document) = document {
            collect_toml_paths(document, &super::ConfigKeyPath::root(), &mut paths);
        }
        Self {
            explicit_paths: paths.into_iter().collect(),
        }
    }

    pub(crate) fn key_is_configured(&self, key: &super::ConfigKeyPath) -> bool {
        self.explicit_paths.contains(key)
    }
}

fn collect_toml_paths(
    value: &toml::Value,
    path: &super::ConfigKeyPath,
    output: &mut Vec<super::ConfigKeyPath>,
) {
    if !path.is_empty() {
        output.push(path.clone());
    }
    match value {
        toml::Value::Table(fields) => {
            for (key, value) in fields {
                collect_toml_paths(value, &path.clone().key(key.clone()), output);
            }
        }
        toml::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_toml_paths(value, &path.clone().index(index), output);
            }
        }
        _ => {}
    }
}

/// Configuration values that runtime code consumes after launch validation.
/// The width and bounds travel together and stay private, so the width is
/// always inside the bounds and no crate outside this one can build a value.
#[derive(Debug, Clone)]
pub struct ValidatedClientUiConfig {
    sidebar_width: Setting<super::SidebarWidth>,
    sidebar_bounds: SidebarBounds,
    pub sidebar_start_collapsed: Setting<bool>,
    pub mouse_capture: bool,
    pub copy_on_select: bool,
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
    /// Absolute and an existing directory when the config was validated.
    Path(shepr_core::absolute_path::AbsolutePath),
}

#[derive(Debug, Clone)]
pub struct ValidatedTerminalConfig {
    /// Absolute, recognized shell selected and resolved at process launch.
    pub default_shell: ResolvedShell,
    pub login_shell: bool,
    pub new_cwd: NewTerminalCwd,
}

impl ValidatedTerminalConfig {
    fn parse(
        config: &TerminalConfig,
        paths: &AppPaths,
    ) -> Result<Self, Vec<super::ConfigDiagnostic>> {
        let default_shell =
            resolve_default_shell(config.default_shell.as_deref(), paths).map_err(|error| {
                super::ConfigDiagnostic::path_at(
                    super::ConfigKeyPath::root()
                        .key("terminal")
                        .key("default_shell"),
                    error,
                )
            });
        let new_cwd = Self::parse_new_cwd(&config.new_cwd, paths).map_err(|error| {
            super::ConfigDiagnostic::path_at(
                super::ConfigKeyPath::root().key("terminal").key("new_cwd"),
                error,
            )
        });
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
                        "cannot be resolved: {}",
                        shepr_core::pathutil::missing_home_error()
                    )
                })?;
                require_new_cwd_directory(path)?;
                Ok(NewTerminalCwd::Home)
            }
            NewTerminalCwdConfig::Current => {
                let path = paths
                    .current_dir()
                    .ok_or_else(|| "current directory was unavailable at launch".to_owned())?;
                require_new_cwd_directory(path)?;
                Ok(NewTerminalCwd::Current)
            }
            NewTerminalCwdConfig::Path(configured_path) => {
                if configured_path.is_empty() {
                    return Err("path must not be empty; use \"follow\" explicitly".to_owned());
                }
                let path = shepr_core::pathutil::expand_tilde_path_with_home(
                    configured_path,
                    paths
                        .home_dir()
                        .map(shepr_core::absolute_path::AbsolutePath::as_path),
                )
                .map_err(|err| format!("cannot be resolved: {err}"))?;
                let absolute = match shepr_core::absolute_path::AbsolutePath::new(path) {
                    Ok(absolute) => absolute,
                    Err(relative) => paths
                        .current_dir()
                        .ok_or_else(|| {
                            "relative path requires a launch working directory".to_owned()
                        })?
                        .resolve(relative.path()),
                };
                require_new_cwd_directory(&absolute)?;
                Ok(NewTerminalCwd::Path(absolute))
            }
        }
    }
}

/// The pane shell for this launch. An unset setting means `$SHELL`, and
/// `/bin/sh` only when `SHELL` is unset or blank.
///
/// An inherited `SHELL` is held to the same standard as a configured shell:
/// one that is unusable or not a shell shepr recognises fails the launch
/// rather than falling back to `/bin/sh`. With `terminal.default_shell` unset
/// (the default, and the only state with no config file), `SHELL` is the
/// setting, so a bad value is a config problem like any other. A fallback
/// would only have surfaced as a line in the server log, which the TUI never
/// shows, while every pane silently opened a different shell than the
/// operator's own; the fix is one line in either place, so the error names
/// both.
///
/// Resolution stays in config validation rather than in server launch code so
/// the validated config holds a `ResolvedShell`, not a raw string: a shell
/// that cannot be used fails at config load, before the server starts.
fn resolve_default_shell(
    configured: Option<&str>,
    paths: &AppPaths,
) -> Result<ResolvedShell, String> {
    let path = shepr_core::env::read_os(shepr_core::env::EnvVar::Path)
        .map_err(|error| error.to_string())?;
    // A relative shell or PATH entry resolves against the launch directory
    // the paths captured, never a fresh read of this process's.
    let cwd = paths.fallback_cwd();

    if let Some(configured) = configured {
        return resolve_recognized_shell(
            OsStr::new(configured),
            "configured shell",
            path.as_deref(),
            cwd,
        );
    }

    let inherited = shepr_core::env::read_os(shepr_core::env::EnvVar::Shell)
        .map_err(|error| error.to_string())?
        .filter(|shell| !shell.is_empty());
    if let Some(inherited) = inherited {
        return resolve_recognized_shell(&inherited, "SHELL", path.as_deref(), cwd).map_err(
            |error| {
                format!(
                    "{error}; no shell was configured, so panes use SHELL={}. \
                     Set it to a shell shepr recognizes, or fix SHELL",
                    inherited.to_string_lossy()
                )
            },
        );
    }

    resolve_recognized_shell(
        OsStr::new("/bin/sh"),
        "the default shell",
        path.as_deref(),
        cwd,
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
    // The platform classifier makes the PTY's access(2) check, so noexec
    // mounts and access policy are part of validation before the server starts.
    let resolved =
        crate::shell::resolve_executable(candidate, path, cwd, shepr_platform::classify_executable)
            .map_err(|error| format!("{source} {error}"))?;

    ResolvedShell::validate(resolved, |resolved| {
        if !resolved
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(shepr_platform::is_pane_shell_process_name)
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

fn require_new_cwd_directory(path: &shepr_core::absolute_path::AbsolutePath) -> Result<(), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("directory {} is unavailable: {error}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "must name an existing directory: {}",
            path.display()
        ));
    }
    Ok(())
}

impl ValidatedClientUiConfig {
    /// Configured expanded sidebar width, already validated to be within
    /// `sidebar_bounds`.
    pub fn sidebar_width(&self) -> super::SidebarWidth {
        *self.sidebar_width.value()
    }

    pub fn sidebar_width_is_explicit(&self) -> bool {
        self.sidebar_width.is_explicit()
    }

    pub fn sidebar_start_collapsed_is_explicit(&self) -> bool {
        self.sidebar_start_collapsed.is_explicit()
    }

    pub fn sidebar_bounds(&self) -> SidebarBounds {
        self.sidebar_bounds
    }

    fn from_config(
        config: &ClientUiConfig,
        bounds: SidebarBounds,
        sidebar_width: super::SidebarWidth,
    ) -> Self {
        Self {
            sidebar_width: if config.sidebar_width.is_some() {
                Setting::Explicit(sidebar_width)
            } else {
                Setting::Default(sidebar_width)
            },
            sidebar_bounds: bounds,
            sidebar_start_collapsed: config
                .sidebar_start_collapsed
                .map_or(Setting::Default(false), Setting::Explicit),
            mouse_capture: config.mouse_capture,
            copy_on_select: config.copy_on_select,
            sidebar: config.sidebar.clone(),
        }
    }
}

/// The local server's label: `local.label`, or this host's short hostname
/// when it is unset. A hostname that cannot be read, or is not a valid label,
/// fails the launch with a pointer to `local.label`.
fn resolve_local_label(
    local: &super::LocalConfig,
) -> Result<super::MachineLabel, super::ConfigDiagnostic> {
    if let Some(label) = &local.label {
        return Ok(label.clone());
    }
    let path = super::ConfigKeyPath::root().key("local").key("label");
    let names = shepr_platform::host_names().ok_or_else(|| {
        super::ConfigDiagnostic::validation(
            path.clone(),
            "this host's name could not be read to name the local server; set a label",
        )
    })?;
    super::MachineLabel::parse(names.short()).map_err(|error| {
        super::ConfigDiagnostic::validation(
            path,
            format!(
                "this host's name {:?} cannot name the local server ({error}); set a label",
                names.short()
            ),
        )
    })
}

/// Whether `machine` is this host's own entry: its label names the local server
/// apart from ASCII case. One `client.toml` can then list every host and be
/// shared by all of them; each host skips its own entry.
fn is_local_entry(machine: &super::MachineConfig, local_label: &super::MachineLabel) -> bool {
    local_label.same_name(&machine.label)
}

/// One diagnostic per machine whose label an earlier entry already uses.
/// Labels are what the client names servers by, so they must be unique. This
/// host's own entry (`is_local_entry`) is not a duplicate: it is skipped. Blank
/// labels and malformed SSH targets cannot reach here, as the types refuse
/// them.
fn machine_label_diagnostics(
    machines: &[super::MachineConfig],
    local_label: Option<&super::MachineLabel>,
) -> Vec<super::ConfigDiagnostic> {
    let label_path = |index: usize| {
        super::ConfigKeyPath::root()
            .key("machines")
            .index(index)
            .key("label")
    };
    let mut seen = std::collections::HashMap::new();
    let mut diagnostics = Vec::new();
    for (index, machine) in machines.iter().enumerate() {
        if local_label.is_some_and(|local_label| is_local_entry(machine, local_label)) {
            continue;
        }
        let first = *seen.entry(&machine.label).or_insert(index);
        if first != index {
            diagnostics.push(super::ConfigDiagnostic::validation_related(
                label_path(index),
                vec![label_path(first)],
                format!(
                    "label {:?} duplicates an earlier machine",
                    machine.label.as_str()
                ),
            ));
        }
    }
    diagnostics
}

/// Validate the client's values against its launch paths. `provenance` says
/// which keys the document set. The document's unknown keys are the loader's
/// to report.
pub(crate) fn validate_client(
    config: &ClientConfig,
    provenance: &ConfigProvenance,
    paths: AppPaths,
) -> Result<ValidatedClientConfig, Vec<super::ConfigDiagnostic>> {
    let keybind_validation = config.compute_keybind_validation(|field| {
        provenance.key_is_configured(&super::ConfigKeyPath::root().key("keys").key(field))
    });
    let sidebar_bounds = super::model::validated_sidebar_bounds(
        config.ui.sidebar_min_width,
        config.ui.sidebar_max_width,
    );
    let sidebar_width = config.ui.sidebar_width.unwrap_or(DEFAULT_SIDEBAR_WIDTH);
    let validated_sidebar_width =
        sidebar_bounds.and_then(|bounds| bounds.checked_width(sidebar_width));

    let mut diagnostics = keybind_validation.diagnostics;
    if sidebar_bounds.is_none() {
        diagnostics.push(super::ConfigDiagnostic::validation_related(
            super::ConfigKeyPath::root()
                .key("ui")
                .key("sidebar_min_width"),
            vec![
                super::ConfigKeyPath::root()
                    .key("ui")
                    .key("sidebar_max_width"),
            ],
            format!(
                "minimum value {} is greater than maximum value {}",
                config.ui.sidebar_min_width, config.ui.sidebar_max_width
            ),
        ));
    } else if validated_sidebar_width.is_none() {
        diagnostics.push(super::ConfigDiagnostic::validation_related(
            super::ConfigKeyPath::root().key("ui").key("sidebar_width"),
            vec![
                super::ConfigKeyPath::root()
                    .key("ui")
                    .key("sidebar_min_width"),
                super::ConfigKeyPath::root()
                    .key("ui")
                    .key("sidebar_max_width"),
            ],
            format!("value {sidebar_width} must be between the configured minimum and maximum"),
        ));
    }
    let local_label = resolve_local_label(&config.local);
    if let Err(diagnostic) = &local_label {
        diagnostics.push(diagnostic.clone());
    }
    diagnostics.extend(machine_label_diagnostics(
        &config.machines,
        local_label.as_ref().ok(),
    ));
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    match (
        keybind_validation.live,
        sidebar_bounds,
        validated_sidebar_width,
        local_label,
    ) {
        (Some(live_keybinds), Some(sidebar_bounds), Some(sidebar_width), Ok(local_label)) => {
            // This host's own entry is dropped; its palette is the local
            // server's hue.
            let (own, machines): (Vec<_>, Vec<_>) = config
                .machines
                .iter()
                .cloned()
                .partition(|machine| is_local_entry(machine, &local_label));
            let local_hue = own
                .first()
                .map_or(super::DEFAULT_LOCAL_HUE, |machine| machine.palette);
            Ok(ValidatedClientConfig {
                paths,
                live_keybinds,
                ui: ValidatedClientUiConfig::from_config(&config.ui, sidebar_bounds, sidebar_width),
                local_hue,
                local_label,
                machines,
            })
        }
        _ => Err(vec![super::ConfigDiagnostic::internal(
            "configuration resolution could not produce validated client values",
        )]),
    }
}

/// Immutable, validated configuration resolved at the process boundary.
/// Runtime preferences continue to live in their own mutable state.
#[derive(Debug, Clone)]
pub struct ValidatedClientConfig {
    paths: AppPaths,
    live_keybinds: super::LiveKeybindConfig,
    ui: ValidatedClientUiConfig,
    /// The local server's hue: this host's own machine entry's palette, or
    /// the default.
    local_hue: shepr_term::host_tint::HostHue,
    /// The name the client shows for the local server, resolved at launch.
    local_label: super::MachineLabel,
    /// In config order, with unique labels, this host's own entry dropped.
    machines: Vec<super::MachineConfig>,
}

impl ValidatedClientConfig {
    /// Validate `config` exactly as a launch does. `source` identifies which
    /// keybindings are configured; the optional chrome settings carry their
    /// own unset state. `None` makes every keybinding a built-in default. The
    /// document is not checked for unknown keys; a launch load does that
    /// before it gets here.
    pub fn validate(
        config: &ClientConfig,
        source: Option<&str>,
        paths: AppPaths,
    ) -> Result<Self, Vec<super::ConfigDiagnostic>> {
        let document = source
            .map(|source| {
                source
                    .parse::<toml::Table>()
                    .map(toml::Value::Table)
                    .map_err(|error| vec![super::ConfigDiagnostic::parse(error.to_string())])
            })
            .transpose()?;
        let provenance = ConfigProvenance::from_document(document.as_ref());
        validate_client(config, &provenance, paths)
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn ui(&self) -> &ValidatedClientUiConfig {
        &self.ui
    }

    /// The local server's hue: the `palette` of this host's own
    /// `[[machines]]` entry, or [`super::DEFAULT_LOCAL_HUE`] without one.
    pub fn local_hue(&self) -> shepr_term::host_tint::HostHue {
        self.local_hue
    }

    /// The name the client shows for the local server: `local.label`, or
    /// this host's short hostname.
    pub fn local_label(&self) -> &super::MachineLabel {
        &self.local_label
    }

    /// The configured machines, in config order. Labels are unique, and none
    /// is the local server's.
    pub fn machines(&self) -> &[super::MachineConfig] {
        &self.machines
    }

    pub fn live_keybinds(&self) -> &super::LiveKeybindConfig {
        &self.live_keybinds
    }
}

/// Pane chrome resolved by the server at launch.
#[derive(Debug, Clone)]
pub struct ValidatedServerUiConfig {
    pub pane_scrollbars: bool,
    pub pane_gaps: bool,
}

/// Session restore resolved by the server at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSessionConfig {
    /// Resume supported agent panes into their own conversations on restore.
    pub resume_agents_on_restore: bool,
    /// The spacing between automatic agent resumes; zero disables spacing.
    pub startup_per_agent_delay: std::time::Duration,
}

/// The `[experimental]` settings resolved by the server at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedExperimentalConfig {
    pub reveal_hidden_cursor_for_cjk_ime: bool,
    /// Agents the cursor reveal is restricted to, without duplicates; empty
    /// means every focused pane.
    pub cjk_ime_agents: Vec<crate::ConfigAgent>,
    pub cjk_ime_cursor_shape: ImeCursorShape,
}

/// Validate the server's values against its launch paths. The server
/// document has no settings whose meaning depends on which keys it set, and
/// its unknown keys are the loader's to report.
pub(crate) fn validate_server(
    config: &super::ServerConfig,
    paths: AppPaths,
) -> Result<ValidatedServerConfig, Vec<super::ConfigDiagnostic>> {
    let headless_size =
        BoundedGridSize::new(config.server.headless_cols, config.server.headless_rows)
            .ok()
            .map(BoundedGridSize::grid);
    let terminal = ValidatedTerminalConfig::parse(&config.terminal, &paths);
    let mut diagnostics = Vec::new();
    if headless_size.is_none() {
        diagnostics.push(super::ConfigDiagnostic::validation_related(
            super::ConfigKeyPath::root().key("server").key("headless_cols"),
            vec![super::ConfigKeyPath::root().key("server").key("headless_rows")],
            format!(
                "columns and rows must be greater than zero, each no larger than {}, and no larger than {} cells combined (got {}x{})",
                MAX_TERMINAL_GRID_DIMENSION,
                MAX_TERMINAL_GRID_CELLS,
                config.server.headless_cols,
                config.server.headless_rows
            ),
        ));
    }
    if let Err(errors) = &terminal {
        diagnostics.extend(errors.iter().cloned());
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    match (headless_size, terminal) {
        (Some(headless_size), Ok(terminal)) => Ok(ValidatedServerConfig {
            paths,
            headless_size,
            terminal,
            ui: ValidatedServerUiConfig {
                pane_scrollbars: config.ui.pane_scrollbars,
                pane_gaps: config.ui.pane_gaps,
            },
            session: ValidatedSessionConfig {
                resume_agents_on_restore: config.session.resume_agents_on_restore,
                startup_per_agent_delay: config.session.startup_per_agent_delay,
            },
            scrollback: shepr_core::scrollback::ScrollbackBudget::new(
                config.advanced.scrollback_limit_bytes,
            ),
            experimental: ValidatedExperimentalConfig {
                reveal_hidden_cursor_for_cjk_ime: config
                    .experimental
                    .reveal_hidden_cursor_for_cjk_ime,
                cjk_ime_agents: config.experimental.cjk_ime_agents.clone(),
                cjk_ime_cursor_shape: config.experimental.cjk_ime_cursor_shape,
            },
        }),
        _ => Err(vec![super::ConfigDiagnostic::internal(
            "configuration resolution could not produce validated server values",
        )]),
    }
}

/// Immutable server configuration, with no client settings.
#[derive(Debug, Clone)]
pub struct ValidatedServerConfig {
    paths: AppPaths,
    headless_size: shepr_core::geometry::GridSize,
    ui: ValidatedServerUiConfig,
    terminal: ValidatedTerminalConfig,
    session: ValidatedSessionConfig,
    scrollback: shepr_core::scrollback::ScrollbackBudget,
    experimental: ValidatedExperimentalConfig,
}

impl ValidatedServerConfig {
    /// Validate server values against the launch context. A launch loader also
    /// checks unknown document keys before constructing this value. Server
    /// validation has no fields that depend on the source document.
    pub fn validate(
        config: &super::ServerConfig,
        paths: AppPaths,
    ) -> Result<Self, Vec<super::ConfigDiagnostic>> {
        validate_server(config, paths)
    }
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }
    pub fn headless_size(&self) -> shepr_core::geometry::GridSize {
        self.headless_size
    }
    pub fn ui(&self) -> &ValidatedServerUiConfig {
        &self.ui
    }
    pub fn terminal(&self) -> &ValidatedTerminalConfig {
        &self.terminal
    }
    pub fn session(&self) -> &ValidatedSessionConfig {
        &self.session
    }
    /// The per-pane scrollback budget; see `advanced.scrollback_limit_bytes`.
    pub fn scrollback(&self) -> shepr_core::scrollback::ScrollbackBudget {
        self.scrollback
    }
    pub fn experimental(&self) -> &ValidatedExperimentalConfig {
        &self.experimental
    }
}

#[cfg(test)]
impl ValidatedClientConfig {
    pub fn validated_live_keybinds(&self) -> Result<super::LiveKeybindConfig, Vec<String>> {
        Ok(self.live_keybinds().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerConfig;

    #[test]
    fn optional_chrome_settings_keep_their_default_or_explicit_origin() {
        let scratch = shepr_test_support::ScratchDir::new("validated-client-chrome-origin");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), None)
            .expect("scratch roots fit a socket");
        let defaults =
            ValidatedClientConfig::validate(&ClientConfig::default(), None, paths.clone())
                .expect("built-in chrome defaults are valid");
        assert_eq!(defaults.ui().sidebar_width().value(), 26);
        assert!(!defaults.ui().sidebar_width_is_explicit());
        assert!(!defaults.ui().sidebar_start_collapsed_is_explicit());

        let mut config = ClientConfig::default();
        config.ui.sidebar_width = Some(31);
        config.ui.sidebar_start_collapsed = Some(true);
        let configured = ValidatedClientConfig::validate(
            &config,
            Some("[ui]\nsidebar_width = 31\nsidebar_start_collapsed = true\n"),
            paths,
        )
        .expect("explicit chrome settings are valid");
        assert_eq!(configured.ui().sidebar_width().value(), 31);
        assert!(configured.ui().sidebar_width_is_explicit());
        assert!(configured.ui().sidebar_start_collapsed_is_explicit());
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
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("relative/worktree".to_owned());
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");

        let validated =
            ValidatedServerConfig::validate(&config, paths).expect("test configuration is valid");

        assert_eq!(
            validated.headless_size(),
            shepr_core::geometry::GridSize::new(92, 31).expect("non-zero test dimensions")
        );
        assert_eq!(
            validated.terminal().new_cwd,
            NewTerminalCwd::Path(
                shepr_core::absolute_path::AbsolutePath::new(configured_cwd).expect("absolute")
            )
        );
    }

    #[test]
    fn validated_config_accepts_the_maximum_shared_headless_grid() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-max-grid");
        let mut config = ServerConfig::default();
        let cols = MAX_TERMINAL_GRID_DIMENSION;
        let rows = u16::try_from(MAX_TERMINAL_GRID_CELLS / usize::from(cols))
            .expect("the shared grid budget fits in u16 rows");
        config.server.headless_cols = cols;
        config.server.headless_rows = rows;
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");

        let validated = ValidatedServerConfig::validate(&config, paths)
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
        let paths = AppPaths::rooted_at(scratch.path(), Some(&home), Some(scratch.path()))
            .expect("scratch roots fit a socket");

        let validated =
            ValidatedServerConfig::validate(&config, paths).expect("test configuration is valid");

        assert_eq!(
            validated.terminal().new_cwd,
            NewTerminalCwd::Path(
                shepr_core::absolute_path::AbsolutePath::new(configured_cwd).expect("absolute")
            )
        );
    }

    #[test]
    fn validated_config_rejects_empty_and_missing_new_cwd_paths() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-invalid-cwd");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");

        for (path, expected) in [("", "must not be empty"), ("missing", "unavailable")] {
            let mut config = ServerConfig::default();
            config.terminal.new_cwd = NewTerminalCwdConfig::Path(path.to_owned());
            let error = ValidatedServerConfig::validate(&config, paths.clone())
                .expect_err("invalid cwd must fail config parsing");
            assert!(
                error
                    .iter()
                    .any(|message| message.to_string().contains(expected)),
                "expected {expected:?} in {error:?}"
            );
        }
    }

    #[test]
    fn validation_reports_shell_and_new_cwd_errors_together() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-multiple-path-errors");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");
        let mut config = ServerConfig::default();
        config.terminal.default_shell =
            Some(scratch.join("missing/zsh").to_string_lossy().into_owned());
        config.terminal.new_cwd = NewTerminalCwdConfig::Path("missing-cwd".to_owned());

        let errors = ValidatedServerConfig::validate(&config, paths)
            .expect_err("both invalid terminal paths should be reported");

        assert!(
            errors
                .iter()
                .any(|message| message.to_string().contains("terminal.default_shell")),
            "shell error missing from {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|message| message.to_string().contains("terminal.new_cwd")),
            "new cwd error missing from {errors:?}"
        );
    }

    #[test]
    fn launch_rejects_a_configured_shell_that_is_missing_or_unrecognised() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-shell");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");
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
            config.terminal.default_shell = Some(shell.to_string_lossy().into_owned());
            let error = ValidatedServerConfig::validate(&config, paths.clone())
                .expect_err("an unusable configured shell fails the launch");
            assert!(
                error
                    .iter()
                    .any(|message| message.to_string().contains(expected)),
                "expected {expected:?} in {error:?}"
            );
        }
    }

    #[test]
    fn a_configured_shell_wins_over_an_unusable_inherited_shell() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-shell-override");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");
        let inherited = scratch.join("missing/inherited-shell");
        env.set("SHELL", &inherited);
        let configured = shepr_test_support::fixture::stand_in(scratch.path(), "zsh", &[]);
        let configured_shell = configured.to_string_lossy().into_owned();
        let mut config = ServerConfig::default();
        config.terminal.default_shell = Some(configured_shell.clone());

        let validated = ValidatedServerConfig::validate(&config, paths)
            .expect("a configured shell takes precedence over inherited SHELL");

        assert_eq!(
            validated.terminal().default_shell.path(),
            Path::new(&configured_shell)
        );
    }

    #[test]
    fn a_bare_configured_shell_name_resolves_to_its_absolute_path() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-bare-shell");
        let shell = shepr_test_support::fixture::stand_in(scratch.path(), "zsh", &[]);
        env.set("PATH", scratch.path());
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");
        let mut config = ServerConfig::default();
        config.terminal.default_shell = Some("zsh".into());

        let validated =
            ValidatedServerConfig::validate(&config, paths).expect("a shell on PATH resolves");

        assert_eq!(validated.terminal().default_shell.path(), shell.as_path());
    }

    #[test]
    fn shell_inputs_reject_surrounding_whitespace_without_trimming() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("shell-whitespace");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");
        for value in [" /bin/sh", "/bin/sh\t", " ", "\u{2003}/bin/sh"] {
            assert!(
                resolve_default_shell(Some(value), &paths)
                    .expect_err("padded configured shell")
                    .contains("configured shell must not have surrounding whitespace")
            );
            env.set("SHELL", value);
            assert!(
                resolve_default_shell(None, &paths)
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
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");
        assert_eq!(
            resolve_default_shell(None, &paths)
                .expect("valid shell")
                .path(),
            shell
        );
    }

    /// With `terminal.default_shell` unset, `SHELL` is the setting: a usable
    /// one is taken, an unusable or unrecognized one fails the launch naming
    /// `SHELL` and the fix, and only an unset one means `/bin/sh`.
    #[test]
    fn an_unset_shell_setting_takes_the_inherited_shell_and_rejects_an_unusable_one() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("validated-config-inherited-shell");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");
        let config = ServerConfig::default();
        let validate = || ValidatedServerConfig::validate(&config, paths.clone());

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
                errors.iter().any(|message| {
                    message.to_string().contains("terminal.default_shell")
                        && message.to_string().contains(expected)
                        && message.to_string().contains(&format!("SHELL={shell}"))
                        && message
                            .to_string()
                            .contains("Set it to a shell shepr recognizes")
                }),
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
