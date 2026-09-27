mod address;
mod agent;
mod diagnostic;
mod io;
mod keybinds;
mod model;
mod session_id;
mod sidebar;
mod tab_bar;
pub mod theme;
mod theme_config;
mod validated;
mod window_title;
mod wire;

pub use self::address::derive_client_socket_from_api_socket;
pub use self::address::{CLIENT_SOCKET_PATH_ENV_VAR, SOCKET_PATH_ENV_VAR, ServerAddress};
pub use self::agent::ConfigAgent;
pub use self::session_id::{
    DEFAULT_SESSION_NAME, SESSION_ENV_VAR, SessionId, SessionName, SessionNameError,
    validate_session_name,
};
#[cfg(any(test, feature = "test-support"))]
pub use self::theme_config::CustomThemeColors;
pub use self::{
    diagnostic::ConfigDiagnostic,
    io::AppPaths,
    keybinds::{
        ActionKeybinds, BindingConfig, BindingKey, IndexedKeybind, Keybinds, LiveKeybindConfig,
        format_key_combo, normalize_key_combo, terminal_key_matches_combo,
    },
    model::{
        AgentPanelSortConfig, Config, HostCursorModeConfig, NewTerminalCwdConfig,
        PaneBordersConfig, RightClickPassthroughModifierConfig, SidebarCollapsedModeConfig,
        StatusIndicatorStyle, TabBarPositionConfig, validated_sidebar_bounds,
    },
    sidebar::{
        AgentSidebarToken, AgentsSidebarConfig, SidebarConfig, SidebarTokenRule, SidebarTokenStyle,
        SpaceSidebarToken, SpacesSidebarConfig,
    },
    tab_bar::TabBarRightEntryConfig,
    theme_config::ThemeConfig,
    validated::{ConfigProvenance, ConfigSource, UiPreferenceKey, ValidatedConfig},
    window_title::{WindowTitlePart, WindowTitleTemplate, WindowTitleToken},
};

pub use self::keybinds::parse_key_combo;
pub(crate) use self::{tab_bar::tab_bar_right_diagnostics, window_title::window_title_diagnostics};

pub use self::{
    tab_bar::{
        MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS, MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS,
        MAX_TAB_BAR_RIGHT_ENTRIES, parse_tab_bar_datetime_format,
    },
    window_title::sanitize_window_title_text,
};

pub const CONFIG_PATH_ENV_VAR: &str = "SHEPR_CONFIG_PATH";
pub const DEFAULT_CONFIG: &str = include_str!("default.toml");

pub const DEFAULT_SCROLLBACK_LIMIT_BYTES: usize = 10_000_000;
pub const DEFAULT_MOUSE_SCROLL_LINES: usize = 3;
pub const DEFAULT_HEADLESS_COLS: u16 = 120;
pub const DEFAULT_HEADLESS_ROWS: u16 = 40;

impl Config {
    #[cfg(any(test, feature = "test-support"))]
    pub fn resolve_palette(&self) -> Result<crate::theme::Palette, Vec<String>> {
        self.resolve_palette_with_ui_accent(false)
    }

    pub fn resolve_palette_with_ui_accent(
        &self,
        ui_accent_is_explicit: bool,
    ) -> Result<crate::theme::Palette, Vec<String>> {
        theme_config::resolve_palette(self, ui_accent_is_explicit)
    }

    /// Parsed keybinds for Shepr actions.
    pub fn keybinds(&self) -> Keybinds {
        self.compute_keybind_validation(|_| false).keybinds
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn collect_diagnostics(&self) -> Vec<String> {
        let validation = self.compute_keybind_validation(|_| false);
        self.collect_diagnostics_with_keybind_validation(&validation)
    }

    pub(crate) fn collect_diagnostics_with_keybind_validation(
        &self,
        validation: &keybinds::KeybindValidation,
    ) -> Vec<String> {
        // sidebar_section_split is persisted client chrome state, not a Config
        // field; its finite-range normalization belongs to preference loading.
        validation
            .prefix_diag
            .iter()
            .cloned()
            .chain(validation.keybind_diags.iter().cloned())
            .chain(self.theme.diagnostics())
            .chain(theme_config::color_diagnostic("ui.accent", &self.ui.accent))
            .chain(tab_bar_right_diagnostics(&self.ui.tab_bar_right))
            .chain(window_title_diagnostics(&self.ui.window_title))
            .chain(self.invalid_sidebar_bounds_diagnostic())
            .chain(self.invalid_headless_size_diagnostic())
            .collect()
    }

    pub fn headless_size(&self) -> shepr_core::geometry::GridSize {
        // `load_validated` rejects zero dimensions before any app is built.
        shepr_core::geometry::GridSize::new(self.server.headless_cols, self.server.headless_rows)
            .expect("headless size is validated before launch")
    }

    pub(crate) fn invalid_headless_size_diagnostic(&self) -> Option<String> {
        (self.server.headless_cols == 0 || self.server.headless_rows == 0).then(|| {
            format!(
                "server.headless_cols and server.headless_rows must be greater than zero (got {}x{})",
                self.server.headless_cols, self.server.headless_rows
            )
        })
    }

    pub(crate) fn invalid_sidebar_bounds_diagnostic(&self) -> Option<String> {
        validated_sidebar_bounds(self.ui.sidebar_min_width, self.ui.sidebar_max_width)
            .is_none()
            .then(|| {
                format!(
                    "ui.sidebar_min_width ({}) is greater than sidebar_max_width ({})",
                    self.ui.sidebar_min_width, self.ui.sidebar_max_width
                )
            })
    }

    /// The prefix and keybinds from one validation pass. Launch configs are
    /// validated first, so nothing reaches here with an invalid binding;
    /// `validated_live_keybinds` rejects one instead.
    #[cfg(any(test, feature = "test-support"))]
    pub fn live_keybinds(&self) -> LiveKeybindConfig {
        let validation = self.compute_keybind_validation(|_| false);
        LiveKeybindConfig {
            prefix: validation.prefix,
            keybinds: validation.keybinds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_keybinds_matches_the_separate_accessor() {
        for profile in [
            "",
            "[keys]\nprefix = \"ctrl+a\"\n",
            "[keys]\nprefix = \"ctrl+\"\n",
        ] {
            let config: Config = toml::from_str(profile).expect("test precondition");
            let live = config.live_keybinds();
            assert_eq!(live.keybinds.detach, config.keybinds().detach);
        }
    }

    #[test]
    fn ui_host_cursor_defaults_to_auto_and_parses_overrides() {
        let default_config = Config::default();
        assert_eq!(default_config.ui.host_cursor, HostCursorModeConfig::Auto);

        let native: Config =
            toml::from_str("[ui]\nhost_cursor = 'native'\n").expect("test precondition");
        assert_eq!(native.ui.host_cursor, HostCursorModeConfig::Native);

        let drawn: Config =
            toml::from_str("[ui]\nhost_cursor = 'drawn'\n").expect("test precondition");
        assert_eq!(drawn.ui.host_cursor, HostCursorModeConfig::Drawn);
    }
}
