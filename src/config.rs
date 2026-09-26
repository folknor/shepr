mod io;
mod keybinds;
mod model;
mod sidebar;
mod tab_bar;
mod theme;
mod window_title;

#[cfg(test)]
pub use self::theme::CustomThemeColors;
#[cfg(test)]
pub(crate) use self::theme::THEME_NAMES;
pub use self::{
    io::AppPaths,
    keybinds::{
        ActionKeybinds, BindingConfig, IndexedKeybind, Keybinds, LiveKeybindConfig,
        format_key_combo, normalize_key_combo, terminal_key_matches_combo,
    },
    model::{
        AgentPanelSortConfig, Config, HostCursorModeConfig, NewTerminalCwdConfig,
        PaneBordersConfig, ShellModeConfig, SidebarCollapsedModeConfig, StatusIndicatorStyle,
        TabBarPositionConfig, validated_sidebar_bounds,
    },
    sidebar::{
        AgentSidebarToken, AgentsSidebarConfig, SidebarConfig, SidebarTokenStyle,
        SpaceSidebarToken, SpacesSidebarConfig,
    },
    tab_bar::TabBarRightEntryConfig,
    theme::ThemeConfig,
    window_title::{WindowTitlePart, WindowTitleTemplate, WindowTitleToken},
};

pub(crate) use self::keybinds::parse_key_combo;
pub(crate) use self::theme::ParsedThemeColors;
pub(crate) use self::{
    tab_bar::{
        MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS, MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS,
        MAX_TAB_BAR_RIGHT_ENTRIES, parse_tab_bar_datetime_format, tab_bar_right_diagnostics,
    },
    theme::canonical_theme_name,
    window_title::{sanitize_window_title_text, window_title_diagnostics},
};

pub const CONFIG_PATH_ENV_VAR: &str = "SHEPR_CONFIG_PATH";

pub const DEFAULT_SCROLLBACK_LIMIT_BYTES: usize = 10_000_000;
pub const DEFAULT_MOUSE_SCROLL_LINES: usize = 3;
pub const DEFAULT_HEADLESS_COLS: u16 = 120;
pub const DEFAULT_HEADLESS_ROWS: u16 = 40;

impl Config {
    pub(crate) fn resolve_palette(&mut self) -> Result<(), Vec<String>> {
        self.resolved_palette = theme::resolve_palette(self)?;
        Ok(())
    }

    /// Parsed keybinds for Shepr actions.
    pub fn keybinds(&self) -> Keybinds {
        self.validated_keybinds().3
    }

    pub fn collect_diagnostics(&self) -> Vec<String> {
        // sidebar_section_split is persisted client chrome state, not a Config
        // field; its finite-range normalization belongs to preference loading.
        let (prefix_diag, _, keybind_diags, _) = self.validated_keybinds();
        prefix_diag
            .into_iter()
            .chain(keybind_diags)
            .chain(self.theme.diagnostics())
            .chain(theme::color_diagnostic("ui.accent", &self.ui.accent))
            .chain(tab_bar_right_diagnostics(&self.ui.tab_bar_right))
            .chain(window_title_diagnostics(&self.ui.window_title))
            .chain(self.invalid_sidebar_bounds_diagnostic())
            .chain(self.invalid_headless_size_diagnostic())
            .collect()
    }

    pub(crate) fn headless_size(&self) -> (u16, u16) {
        // `load_validated` rejects zero dimensions before any app is built.
        (self.server.headless_cols, self.server.headless_rows)
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
    pub(crate) fn live_keybinds(&self) -> LiveKeybindConfig {
        let (_, prefix, _, keybinds) = self.validated_keybinds();
        LiveKeybindConfig { prefix, keybinds }
    }

    pub(crate) fn validated_live_keybinds(&self) -> Result<LiveKeybindConfig, Vec<String>> {
        let (prefix_diag, prefix, keybind_diags, keybinds) = self.validated_keybinds();
        if prefix_diag.is_some() || !keybind_diags.is_empty() {
            Err(prefix_diag.into_iter().chain(keybind_diags).collect())
        } else {
            Ok(LiveKeybindConfig { prefix, keybinds })
        }
    }

    pub(crate) fn local_keybindings_profile_toml(&self) -> Result<String, toml::ser::Error> {
        #[derive(serde::Serialize)]
        struct KeysProfile {
            keys: model::KeysConfigOverlay,
        }

        let live = self.live_keybinds();
        let mut keys = self.keys.local_profile(&live.keybinds);
        keys.set_prefix(format_key_combo(live.prefix));
        toml::to_string_pretty(&KeysProfile { keys })
    }
}

/// Keybinds from an endpoint's published keybinding profile. Invalid
/// bindings reject the profile, just as they reject a local config at launch.
pub(crate) fn keybindings_from_profile_toml(profile: &str) -> Result<LiveKeybindConfig, String> {
    let config = toml::from_str::<Config>(profile)
        .map_err(|err| format!("invalid keybinding profile: {err}"))?;
    config
        .validated_live_keybinds()
        .map_err(|diagnostics| diagnostics.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_keybindings_profile_includes_defaults() {
        let config: Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_tab = "prefix+t"
"#,
        )
        .expect("test precondition");

        let profile = config
            .local_keybindings_profile_toml()
            .expect("test precondition");
        assert!(profile.contains("[keys]"));
        assert!(profile.contains("prefix = \"ctrl+a\""));
        assert!(profile.contains("new_tab = \"prefix+t\""));
        assert!(profile.contains("next_tab = \"prefix+n\""));
    }

    #[test]
    fn keybinding_profile_rejects_invalid_bindings() {
        let error = keybindings_from_profile_toml(
            r#"
[keys]
zoom = "prefix+nonsense-key"
"#,
        )
        .expect_err("a bad binding rejects the profile");

        assert!(error.contains("keys.zoom"), "{error}");
    }

    #[test]
    fn keybinding_profile_with_invalid_prefix_is_an_error() {
        let error = keybindings_from_profile_toml("[keys]\nprefix = \"ctrl+\"\n")
            .expect_err("an invalid prefix rejects the profile");
        assert!(error.contains("keys.prefix"), "{error}");
    }

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
    fn local_keybindings_profile_preserves_user_default_provenance() {
        let config: Config = toml::from_str(
            r#"
[keys]
zoom = "prefix+?"
"#,
        )
        .expect("test precondition");

        let profile = config
            .local_keybindings_profile_toml()
            .expect("test precondition");
        let round_tripped: Config = toml::from_str(&profile).expect("test precondition");

        assert!(profile.contains("zoom = \"prefix+?\""));
        assert!(!profile.contains("help = \"prefix+?\""));
        assert!(
            round_tripped
                .keybinds()
                .zoom
                .bindings
                .iter()
                .any(|binding| binding.label == "prefix+?")
        );
        assert!(round_tripped.keybinds().help.bindings.is_empty());
    }

    #[test]
    fn local_keybindings_profile_omits_default_displaced_by_user_prefix() {
        let config: Config = toml::from_str(
            r#"
[keys]
prefix = "n"
"#,
        )
        .expect("test precondition");

        let profile = config
            .local_keybindings_profile_toml()
            .expect("test precondition");
        let round_tripped: Config = toml::from_str(&profile).expect("test precondition");

        assert!(profile.contains("prefix = \"n\""));
        assert!(!profile.contains("next_tab = \"prefix+n\""));
        assert!(round_tripped.keybinds().next_tab.bindings.is_empty());
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
