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

pub use self::address::ServerAddress;
pub use self::address::derive_client_socket_from_api_socket;
pub use self::agent::ConfigAgent;
#[cfg(feature = "test-support")]
pub use self::model::Config;
#[cfg(not(feature = "test-support"))]
pub(crate) use self::model::Config;
pub use self::session_id::{
    DEFAULT_SESSION_NAME, SessionId, SessionName, SessionNameError, validate_session_name,
};
#[cfg(any(test, feature = "test-support"))]
pub use self::theme_config::CustomThemeColors;
pub use self::{
    diagnostic::ConfigDiagnostic,
    io::{AppPaths, load_for_check, load_validated},
    keybinds::{
        ActionKeybinds, BindingConfig, BindingKey, IndexedKeybind, Keybinds, LiveKeybindConfig,
        format_key_combo, normalize_key_combo, terminal_key_matches_combo,
    },
    model::{
        AgentPanelSortConfig, HostCursorModeConfig, NewTerminalCwdConfig, PaneBordersConfig,
        RightClickPassthroughModifierConfig, SidebarBounds, SidebarCollapsedModeConfig,
        StatusIndicatorStyle, TabBarPositionConfig, validated_sidebar_bounds,
    },
    sidebar::{
        AgentSidebarToken, AgentsSidebarConfig, SidebarConfig, SidebarTokenRule, SidebarTokenStyle,
        SpaceSidebarToken, SpacesSidebarConfig,
    },
    tab_bar::TabBarRightEntryConfig,
    theme_config::ThemeConfig,
    validated::{
        ConfigProvenance, ConfigSource, NewTerminalCwd, UiPreferenceKey, ValidatedConfig,
        ValidatedTerminalConfig, ValidatedUiConfig,
    },
    window_title::{WindowTitlePart, WindowTitleTemplate, WindowTitleToken},
};

pub use self::keybinds::parse_key_combo;
// Runtime code receives tab-bar entries already parsed on `ValidatedConfig`;
// the raw-entry parser is exported only for test fixtures.
#[cfg(any(test, feature = "test-support"))]
pub use self::tab_bar::parse_tab_bar_right_entries as parse_validated_tab_bar_entries;
pub use self::{tab_bar::ValidatedTabBarRightEntry, window_title::sanitize_window_title_text};

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

    #[cfg(any(test, feature = "test-support"))]
    pub fn collect_diagnostics(&self) -> Vec<String> {
        let provenance = ConfigProvenance::defaults(self);
        let resolution = validated::ConfigResolution::parse(
            self,
            &provenance,
            &AppPaths::default(),
            validated::CwdCheck::AtLaunch,
        );
        resolution
            .diagnostics
            .into_iter()
            .chain(resolution.path_diagnostics)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keybind_parser_returns_only_complete_values() {
        for profile in ["", "[keys]\nprefix = \"ctrl+a\"\n"] {
            let config: Config = toml::from_str(profile).expect("test precondition");
            let validation = config.compute_keybind_validation(|_| false);
            let live = validation
                .live
                .expect("valid bindings produce a complete value");
            assert_eq!(live.keybinds.detach.label(), Some("prefix+q".into()));
        }

        let config: Config =
            toml::from_str("[keys]\nprefix = \"ctrl+\"\n").expect("test precondition");
        let validation = config.compute_keybind_validation(|_| false);
        assert!(validation.live.is_none());
        assert!(
            validation
                .diagnostics
                .iter()
                .any(|diag| diag.contains("keys.prefix"))
        );
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
