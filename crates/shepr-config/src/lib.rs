mod address;
mod agent;
mod diagnostic;
mod io;
mod keybinding_table;
mod keybinds;
mod limits;
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
pub use self::limits::{
    DEFAULT_HEADLESS_COLS, DEFAULT_HEADLESS_ROWS, DEFAULT_MOUSE_SCROLL_LINES,
    DEFAULT_SCROLLBACK_LIMIT_BYTES,
};
/// The raw config values, as deserialized. Runtime code receives a
/// [`ValidatedConfig`]; raw values become one only through validation
/// ([`ValidatedConfig::from_values`] or a launch load).
pub use self::model::Config;
pub use self::session_id::{
    DEFAULT_SESSION_NAME, SessionId, SessionName, SessionNameError, validate_session_name,
};
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
pub use self::{tab_bar::ValidatedTabBarRightEntry, window_title::sanitize_window_title_text};

pub const DEFAULT_CONFIG: &str = include_str!("default.toml");

impl Config {
    pub fn resolve_palette_with_ui_accent(
        &self,
        ui_accent_is_explicit: bool,
    ) -> Result<crate::theme::Palette, Vec<String>> {
        theme_config::resolve_palette(self, ui_accent_is_explicit)
    }

    #[cfg(test)]
    pub fn collect_diagnostics(&self) -> Vec<String> {
        let provenance = ConfigProvenance::defaults(self);
        let resolution = validated::ConfigResolution::parse(
            self,
            &provenance,
            &AppPaths::default(),
            validated::CwdCheck::AtLaunch,
            // Diagnostics here cover the document alone; the launch-time
            // shell lookup reads the process environment.
            validated::ShellCheck::Received,
        );
        resolution
            .diagnostics
            .into_iter()
            .chain(resolution.path_diagnostics)
            .collect()
    }
}

#[cfg(test)]
impl Config {
    pub fn resolve_palette(&self) -> Result<crate::theme::Palette, Vec<String>> {
        self.resolve_palette_with_ui_accent(false)
    }
}

#[cfg(test)]
mod tests {
    use super::model::KeysConfig;
    use super::*;
    use std::collections::BTreeMap;

    fn collect_default_leaves(
        value: &toml::Value,
        path: &mut Vec<String>,
        leaves: &mut Vec<(Vec<String>, toml::Value)>,
    ) {
        match value {
            toml::Value::Table(table) => {
                for (key, value) in table {
                    path.push(key.clone());
                    collect_default_leaves(value, path, leaves);
                    path.pop();
                }
            }
            _ => leaves.push((path.clone(), value.clone())),
        }
    }

    fn default_template_values() -> BTreeMap<Vec<String>, Vec<String>> {
        let mut values = BTreeMap::new();
        let mut section = Vec::new();

        for line in DEFAULT_CONFIG.lines() {
            let trimmed = line.trim();
            let content = trimmed.strip_prefix("# ").unwrap_or(trimmed);
            if content.starts_with('[') && content.ends_with(']') {
                section = content[1..content.len() - 1]
                    .split('.')
                    .map(str::to_owned)
                    .collect();
                continue;
            }

            let Some(setting) = trimmed.strip_prefix("# ") else {
                continue;
            };
            let Some((key, value)) = setting.split_once(" = ") else {
                continue;
            };
            let mut path = section.clone();
            path.push(key.to_owned());
            values
                .entry(path)
                .or_insert_with(Vec::new)
                .push(value.to_owned());
        }

        values
    }

    #[test]
    fn default_template_documents_every_config_default() {
        let config = toml::Value::try_from(Config::default()).expect("default config serializes");
        let mut leaves = Vec::new();
        collect_default_leaves(&config, &mut Vec::new(), &mut leaves);
        assert!(!leaves.is_empty());

        let documented = default_template_values();
        for (path, value) in &leaves {
            let key = path.join(".");
            let expected = value.to_string();
            let matches = documented.get(path).is_some_and(|values| {
                values.iter().any(|documented| {
                    documented.strip_prefix(&expected).is_some_and(|suffix| {
                        suffix.trim().is_empty() || suffix.trim_start().starts_with('#')
                    })
                })
            });
            assert!(
                matches,
                "{key} = {expected} is missing from the commented default config"
            );
        }
    }

    #[test]
    fn config_default_template_keeps_all_settings_comment_only() {
        for (index, line) in DEFAULT_CONFIG.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
                continue;
            }

            let line_number = index + 1;
            panic!("active setting on line {line_number}: {line}");
        }
    }

    /// The template names every built-in theme, and its commented `name`
    /// setting is the real default.
    #[test]
    fn default_template_lists_every_theme_and_the_default() {
        let words: Vec<&str> = DEFAULT_CONFIG
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .collect();
        for name in theme::THEME_NAMES {
            assert!(words.contains(name), "default.toml does not list {name}");
        }
        let default_line = format!("# name = \"{}\"", theme::DEFAULT_THEME);
        assert!(
            DEFAULT_CONFIG
                .lines()
                .any(|line| line.trim() == default_line),
            "default.toml must show {default_line}"
        );
    }

    /// The commented `[keys]` settings in the template, uncommented, are
    /// exactly the built-in keymap: every field is listed and every listed
    /// value is the real default.
    #[test]
    fn default_template_lists_every_keybinding_with_its_default() {
        let mut in_keys = false;
        let mut uncommented = String::from("[keys]\n");
        let mut listed = 0;
        for line in DEFAULT_CONFIG.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_keys = trimmed == "[keys]";
                continue;
            }
            let Some(setting) = trimmed.strip_prefix("# ") else {
                continue;
            };
            let is_setting = setting
                .split_once(" = \"")
                .is_some_and(|(name, _)| name.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
            if in_keys && is_setting {
                uncommented.push_str(setting);
                uncommented.push('\n');
                listed += 1;
            }
        }
        let config: Config = toml::from_str(&uncommented).expect("template keys parse");
        assert_eq!(config.keys, KeysConfig::default());
        // The prefix plus every binding in the keybinding table.
        assert_eq!(listed, 57, "{uncommented}");
    }

    #[test]
    fn built_in_keymap_defaults_are_pinned() {
        let keys = KeysConfig::default();
        assert_eq!(keys.prefix, "ctrl+b");
        for (binding, expected) in [
            (&keys.help, "prefix+?"),
            (&keys.detach, "prefix+q"),
            (&keys.workspace_picker, "prefix+w"),
            (&keys.goto, "prefix+g"),
            (&keys.new_workspace, "prefix+shift+n"),
            (&keys.rename_workspace, "prefix+shift+w"),
            (&keys.close_workspace, "prefix+shift+d"),
            (&keys.previous_workspace, ""),
            (&keys.next_workspace, ""),
            (&keys.previous_agent, ""),
            (&keys.next_agent, ""),
            (&keys.focus_agent, ""),
            (&keys.new_tab, "prefix+c"),
            (&keys.rename_tab, "prefix+shift+t"),
            (&keys.previous_tab, "prefix+p"),
            (&keys.next_tab, "prefix+n"),
            (&keys.move_tab_previous, ""),
            (&keys.move_tab_next, ""),
            (&keys.switch_tab, "prefix+1..9"),
            (&keys.switch_workspace, ""),
            (&keys.close_tab, "prefix+shift+x"),
            (&keys.rename_pane, "prefix+shift+p"),
            (&keys.clear_pane, ""),
            (&keys.copy_mode, "prefix+["),
            (&keys.focus_pane_left, "prefix+h"),
            (&keys.focus_pane_down, "prefix+j"),
            (&keys.focus_pane_up, "prefix+k"),
            (&keys.focus_pane_right, "prefix+l"),
            (&keys.swap_pane_left, "prefix+shift+h"),
            (&keys.swap_pane_down, "prefix+shift+j"),
            (&keys.swap_pane_up, "prefix+shift+k"),
            (&keys.swap_pane_right, "prefix+shift+l"),
            (&keys.cycle_pane_next, "prefix+tab"),
            (&keys.cycle_pane_previous, "prefix+shift+tab"),
            (&keys.last_pane, ""),
            (&keys.split_vertical, "prefix+v"),
            (&keys.split_horizontal, "prefix+minus"),
            (&keys.close_pane, "prefix+x"),
            (&keys.zoom, "prefix+z"),
            (&keys.resize_mode, "prefix+r"),
            (&keys.resize_pane_left, ""),
            (&keys.resize_pane_down, ""),
            (&keys.resize_pane_up, ""),
            (&keys.resize_pane_right, ""),
            (&keys.toggle_sidebar, "prefix+b"),
            (&keys.navigate_back, "esc"),
            (&keys.navigate_workspace_up, "up"),
            (&keys.navigate_workspace_down, "down"),
            (&keys.navigate_pane_left, "h"),
            (&keys.navigate_pane_down, "j"),
            (&keys.navigate_pane_up, "k"),
            (&keys.navigate_pane_right, "l"),
            (&keys.navigate_cycle_pane_next, "tab"),
            (&keys.navigate_cycle_pane_previous, "shift+tab"),
            (&keys.navigate_open_workspace, "enter"),
            (&keys.navigate_switch_workspace, "1..9"),
        ] {
            assert_eq!(binding, &BindingConfig::one(expected));
        }
    }

    #[test]
    fn a_prefix_shared_with_a_default_navigate_key_names_the_binding_to_set() {
        let config: Config =
            toml::from_str("[keys]\nprefix = \"esc\"\n").expect("test precondition");
        let validation = config.compute_keybind_validation(|_| false);
        assert!(validation.live.is_none());
        assert!(
            validation
                .diagnostics
                .iter()
                .any(|diag| diag.contains("navigate_back") && diag.contains("keys.prefix")),
            "{:?}",
            validation.diagnostics
        );

        let config: Config = toml::from_str("[keys]\nprefix = \"esc\"\nnavigate_back = \"\"\n")
            .expect("test precondition");
        assert!(config.compute_keybind_validation(|_| false).live.is_some());
    }

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
    fn ui_host_cursor_defaults_to_native_and_parses_overrides() {
        let default_config = Config::default();
        assert_eq!(default_config.ui.host_cursor, HostCursorModeConfig::Native);

        let native: Config =
            toml::from_str("[ui]\nhost_cursor = 'native'\n").expect("test precondition");
        assert_eq!(native.ui.host_cursor, HostCursorModeConfig::Native);

        let drawn: Config =
            toml::from_str("[ui]\nhost_cursor = 'drawn'\n").expect("test precondition");
        assert_eq!(drawn.ui.host_cursor, HostCursorModeConfig::Drawn);

        assert!(toml::from_str::<Config>("[ui]\nhost_cursor = 'auto'\n").is_err());
    }
}
