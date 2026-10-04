mod agent;
mod diagnostic;
mod io;
mod keybinding_table;
mod keybinds;
mod limits;
mod machine;
mod model;
mod shell;
mod sidebar;
pub mod theme;
mod theme_config;
mod validated;
mod window_title;

pub use self::agent::ConfigAgent;
pub use self::limits::{
    DEFAULT_HEADLESS_COLS, DEFAULT_HEADLESS_ROWS, DEFAULT_MOUSE_SCROLL_LINES,
    DEFAULT_SCROLLBACK_LIMIT_BYTES,
};
pub use self::machine::{
    LOCAL_ENDPOINT_LABEL, MachineConfig, MachineLabel, MachineLabelError, SshTarget, SshTargetError,
};
/// Role-specific raw values. Runtime code receives a [`ValidatedClientConfig`]
/// or [`ValidatedServerConfig`], constructed through validation at launch or
/// through that role's `validate`.
pub use self::model::{ClientConfig, ServerConfig};
pub use self::theme_config::CustomThemeColors;
pub use self::{
    diagnostic::{ConfigDiagnostic, ConfigDiagnosticKind, ConfigKeyPath, ConfigKeyPathSegment},
    io::{load_client_validated, load_server_validated},
    keybinding_table::HelpGroup,
    keybinds::{
        ActionKeybinds, BindingConfig, IndexedKeybind, IndexedRange, Keybinds, LiveKeybindConfig,
        format_key_chord, parse_key_chord,
    },
    model::{
        AgentPanelSortConfig, HostCursorModeConfig, NewTerminalCwdConfig, PaneBordersConfig,
        RightClickPassthroughModifierConfig, SidebarBounds, SidebarCollapsedModeConfig,
        SidebarWidth, StatusIndicatorStyle,
    },
    sidebar::{
        AgentSidebarToken, AgentSidebarTokenKind, AgentsSidebarConfig, SidebarConfig,
        SidebarTokenRendering, SidebarTokenRule, SidebarTokenSpec, SidebarTokenStyle,
        SpaceSidebarToken, SpaceSidebarTokenKind, SpacesSidebarConfig,
    },
    theme_config::ThemeConfig,
    validated::{
        ConfigProvenance, NewTerminalCwd, Setting, ValidatedClientConfig, ValidatedClientUiConfig,
        ValidatedExperimentalConfig, ValidatedServerConfig, ValidatedServerUiConfig,
        ValidatedSessionConfig, ValidatedTerminalConfig,
    },
    window_title::{WindowTitlePart, WindowTitleTemplate, WindowTitleToken},
};

pub use self::window_title::sanitize_window_title_text;

pub const DEFAULT_CLIENT_CONFIG: &str = include_str!("default-client.toml");
pub const DEFAULT_SERVER_CONFIG: &str = include_str!("default-server.toml");

impl ClientConfig {
    pub fn resolve_palette(&self) -> Result<crate::theme::Palette, Vec<ConfigDiagnostic>> {
        theme_config::resolve_palette(&self.theme)
    }
}

impl ServerConfig {
    pub fn resolve_palette(&self) -> Result<crate::theme::Palette, Vec<ConfigDiagnostic>> {
        theme_config::resolve_palette(&self.theme)
    }
}

/// Absolute like resolved launch paths, and identical across calls, so two
/// test configs compare equal.
/// The root cannot be created by an unprivileged user: a test that writes
/// through these paths fails instead of leaving files in a shared location.
#[cfg(test)]
pub(crate) fn test_paths() -> shepr_paths::AppPaths {
    let root = std::path::Path::new("/nonexistent/shepr-test-config");
    shepr_paths::AppPaths::rooted_at(root, Some(root), None).expect("short test root")
}

/// Paths under `root`, with no home or current directory.
#[cfg(test)]
pub(crate) fn test_paths_at(root: &std::path::Path) -> shepr_paths::AppPaths {
    shepr_paths::AppPaths::rooted_at(root, None, None).expect("scratch roots fit a socket")
}

#[cfg(test)]
impl ClientConfig {
    pub fn collect_diagnostics(&self) -> Vec<String> {
        // Client document validation has no shell or working-directory lookup.
        ValidatedClientConfig::validate(self, None, test_paths())
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|diagnostic| diagnostic.to_string())
            .collect()
    }
}

#[cfg(test)]
impl ServerConfig {
    pub fn collect_diagnostics(&self) -> Vec<String> {
        ValidatedServerConfig::validate(self, test_paths())
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|diagnostic| diagnostic.to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::model::{
        AdvancedConfig, ClientUiConfig, ExperimentalConfig, HeadlessConfig, KeysConfig,
        ServerConfig, ServerUiConfig, SessionConfig, TerminalConfig,
    };
    use super::*;

    #[test]
    fn default_template_documented_values_match_defaults() {
        let mut document = String::new();
        let mut section = String::new();
        for line in DEFAULT_CLIENT_CONFIG.lines() {
            let line = line.trim();
            let content = line.strip_prefix("# ").unwrap_or(line);
            if content.starts_with('[') && content.ends_with(']') {
                section = content.to_owned();
                if !matches!(
                    section.as_str(),
                    "[theme]"
                        | "[theme.custom]"
                        | "[ui.sidebar.agents.rows_by_agent]"
                        | "[[machines]]"
                ) {
                    document.push_str(content);
                    document.push('\n');
                }
                continue;
            }
            if matches!(
                section.as_str(),
                "[theme]" | "[theme.custom]" | "[ui.sidebar.agents.rows_by_agent]" | "[[machines]]"
            ) {
                continue;
            }
            let Some(setting) = line.strip_prefix("# ") else {
                continue;
            };
            let Some((key, _)) = setting.split_once(" = ") else {
                continue;
            };
            if !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                continue;
            }
            document.push_str(setting);
            document.push('\n');
        }
        let mut documented: ClientConfig =
            toml::from_str(&document).expect("documented defaults parse");
        let defaults = ClientConfig::default();
        assert_eq!(
            documented.ui.mouse_scroll_lines(),
            defaults.ui.mouse_scroll_lines()
        );
        // The optional input resolves to the same default, despite Some/None.
        documented.ui.mouse_scroll_lines = defaults.ui.mouse_scroll_lines;
        // These values carry explicitness into validation; raw defaults keep
        // them unset even though the documented template spells them out, so
        // the documented values are checked against the validated defaults.
        let validated =
            ValidatedClientConfig::validate(&ClientConfig::default(), None, test_paths())
                .expect("built-in client defaults validate");
        assert_eq!(
            documented.ui.sidebar_width,
            Some(validated.ui().sidebar_width().value())
        );
        assert_eq!(
            documented.ui.sidebar_start_collapsed,
            Some(*validated.ui().sidebar_start_collapsed.value())
        );
        assert_eq!(
            documented.ui.agent_panel_sort,
            Some(*validated.ui().agent_panel_sort.value())
        );
        documented.ui.sidebar_width = None;
        documented.ui.sidebar_start_collapsed = None;
        documented.ui.agent_panel_sort = None;
        assert_eq!(documented, defaults);
    }

    #[test]
    fn server_default_template_documented_values_match_defaults() {
        let mut document = String::new();
        let mut section = String::new();
        for line in DEFAULT_SERVER_CONFIG.lines() {
            let line = line.trim();
            let content = line.strip_prefix("# ").unwrap_or(line);
            if content.starts_with('[') && content.ends_with(']') {
                section = content.to_owned();
                // The theme sections hold examples, not the unset defaults.
                if !matches!(section.as_str(), "[theme]" | "[theme.custom]") {
                    document.push_str(content);
                    document.push('\n');
                }
                continue;
            }
            if matches!(section.as_str(), "[theme]" | "[theme.custom]") {
                continue;
            }
            let Some(setting) = line.strip_prefix("# ") else {
                continue;
            };
            let Some((key, _)) = setting.split_once(" = ") else {
                continue;
            };
            if !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                continue;
            }
            document.push_str(setting);
            document.push('\n');
        }
        let mut documented: ServerConfig =
            toml::from_str(&document).expect("documented defaults parse");
        // The shell is unset by default; the template shows an example value.
        assert!(documented.terminal.default_shell.is_some());
        documented.terminal.default_shell = None;
        let defaults = ServerConfig::default();
        assert_eq!(documented, defaults);
    }

    #[test]
    fn default_template_documents_every_config_field() {
        let mut config = ClientConfig::default();
        config.theme.custom = Some(CustomThemeColors::default());
        config.machines.push(MachineConfig {
            label: MachineLabel::parse("schema example").expect("valid test label"),
            ssh: SshTarget::parse("example.invalid").expect("valid test target"),
        });
        let fields = config_field_paths(config);

        let mut documented = BTreeSet::new();
        let mut section = String::new();
        for line in DEFAULT_CLIENT_CONFIG.lines() {
            let line = line.trim();
            let content = line.strip_prefix("# ").unwrap_or(line);
            if content.starts_with('[') && content.ends_with(']') {
                section = content.to_owned();
                let table = section.trim_start_matches('[').trim_end_matches(']');
                // A nested header documents every table above it too.
                for (index, _) in table.match_indices('.') {
                    documented.insert(table[..index].to_owned());
                }
                documented.insert(table.to_owned());
                continue;
            }
            let Some(setting) = line.strip_prefix("# ") else {
                continue;
            };
            let Some((key, _)) = setting.split_once(" = ") else {
                continue;
            };
            if !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || section == "[ui.sidebar.agents.rows_by_agent]"
            {
                continue;
            }
            let table = section.trim_start_matches('[').trim_end_matches(']');
            let path = if table.is_empty() {
                key.to_owned()
            } else {
                format!("{table}.{key}")
            };
            documented.insert(path);
        }

        let missing = fields.difference(&documented).collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "config template does not document config fields: {missing:?}"
        );
    }

    #[test]
    fn server_default_template_documents_every_config_field() {
        let mut config = ServerConfig::default();
        config.theme.custom = Some(CustomThemeColors::default());
        let fields = server_config_field_paths(config);

        let mut documented = BTreeSet::new();
        let mut section = String::new();
        for line in DEFAULT_SERVER_CONFIG.lines() {
            let line = line.trim();
            let content = line.strip_prefix("# ").unwrap_or(line);
            if content.starts_with('[') && content.ends_with(']') {
                section = content.to_owned();
                let table = section.trim_start_matches('[').trim_end_matches(']');
                // A nested header documents every table above it too.
                for (index, _) in table.match_indices('.') {
                    documented.insert(table[..index].to_owned());
                }
                documented.insert(table.to_owned());
                continue;
            }
            let Some(setting) = line.strip_prefix("# ") else {
                continue;
            };
            let Some((key, _)) = setting.split_once(" = ") else {
                continue;
            };
            if !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || section == "[ui.sidebar.agents.rows_by_agent]"
            {
                continue;
            }
            let table = section.trim_start_matches('[').trim_end_matches(']');
            let path = if table.is_empty() {
                key.to_owned()
            } else {
                format!("{table}.{key}")
            };
            documented.insert(path);
        }

        let missing = fields.difference(&documented).collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "config template does not document config fields: {missing:?}"
        );
    }

    // These exact destructures make adding a config field a compile error until
    // its documented path is included in this list and the template.
    macro_rules! record_config_fields {
        ($fields:ident, $value:expr, $prefix:expr, $type:ident {
            $($field:ident $(as $key:literal)? => $binding:pat),+ $(,)?
        }) => {
            let $type { $($field: $binding),+ } = $value;
            $(
                // A field whose TOML key differs from its name (`as`) is
                // documented under the key.
                let key = stringify!($field);
                let _ = key;
                $(let key = $key;)?
                let path = if ($prefix).is_empty() {
                    key.to_owned()
                } else {
                    format!("{}.{}", $prefix, key)
                };
                $fields.insert(path);
            )+
        };
    }

    macro_rules! record_key_config_fields {
        (
            actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:ident, $action_label:literal, $action_doc:literal),)* }
            indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:ident, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
            navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:ident, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
            navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:ident, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
        ) => {
            fn record_key_config_fields(fields: &mut BTreeSet<String>, keys: KeysConfig) {
                let KeysConfig {
                    prefix: _,
                    $($action_field: _,)*
                    $($indexed_field: _,)*
                    $($navigate_config_field: _,)*
                    $($navigate_indexed_config_field: _,)*
                } = keys;
                fields.insert("keys.prefix".to_owned());
                $(fields.insert(format!("keys.{}", stringify!($action_field)));)*
                $(fields.insert(format!("keys.{}", stringify!($indexed_field)));)*
                $(fields.insert(format!("keys.{}", stringify!($navigate_config_field)));)*
                $(fields.insert(format!("keys.{}", stringify!($navigate_indexed_config_field)));)*
            }
        };
    }
    crate::keybinding_table!(record_key_config_fields);

    fn config_field_paths(config: ClientConfig) -> BTreeSet<String> {
        let mut fields = BTreeSet::new();
        record_config_fields!(fields, config, "", ClientConfig {
            theme => theme,
            keys => keys,
            ui => ui,
            machines => machines,
        });
        record_config_fields!(fields, theme, "theme", ThemeConfig {
            name => _,
            accent => _,
            custom => custom,
        });
        if let Some(custom) = custom {
            record_config_fields!(fields, custom, "theme.custom", CustomThemeColors {
                accent => _,
                panel_bg => _,
                sidebar_bg => _,
                active_row_bg => _,
                selection_bg => _,
                surface0 => _,
                surface1 => _,
                surface_dim => _,
                overlay0 => _,
                overlay1 => _,
                text => _,
                subtext0 => _,
                mauve => _,
                green => _,
                yellow => _,
                red => _,
                blue => _,
                teal => _,
                peach => _,
            });
        }
        record_key_config_fields(&mut fields, keys);
        record_config_fields!(fields, ui, "ui", ClientUiConfig {
            sidebar_width => _,
            sidebar_min_width => _,
            sidebar_max_width => _,
            sidebar_start_collapsed => _,
            sidebar_collapsed_mode => _,
            mouse_capture => _,
            copy_on_select => _,
            host_cursor => _,
            right_click_passthrough_modifier => _,
            redraw_on_focus_gained => _,
            mouse_scroll_lines => _,
            confirm_close => _,
            prompt_new_workspace_name => _,
            agent_panel_sort => _,
            status_indicators => _,
            sidebar => sidebar,
        });
        record_config_fields!(fields, sidebar, "ui.sidebar", SidebarConfig {
            agents => agents,
            spaces => spaces,
        });
        record_config_fields!(fields, agents, "ui.sidebar.agents", AgentsSidebarConfig {
            rows => _,
            rows_by_agent => _,
            row_gap => _,
        });
        record_config_fields!(fields, spaces, "ui.sidebar.spaces", SpacesSidebarConfig {
            rows => _,
            row_gap => _,
        });
        for machine in machines {
            record_config_fields!(fields, machine, "machines", MachineConfig {
                label => _,
                ssh => _,
            });
        }
        fields
    }

    fn server_config_field_paths(config: ServerConfig) -> BTreeSet<String> {
        let mut fields = BTreeSet::new();
        record_config_fields!(fields, config, "", ServerConfig {
            theme => theme,
            terminal => terminal,
            session => session,
            server => server,
            ui => ui,
            advanced => advanced,
            experimental => experimental,
        });
        record_config_fields!(fields, theme, "theme", ThemeConfig {
            name => _,
            accent => _,
            custom => custom,
        });
        if let Some(custom) = custom {
            record_config_fields!(fields, custom, "theme.custom", CustomThemeColors {
                accent => _,
                panel_bg => _,
                sidebar_bg => _,
                active_row_bg => _,
                selection_bg => _,
                surface0 => _,
                surface1 => _,
                surface_dim => _,
                overlay0 => _,
                overlay1 => _,
                text => _,
                subtext0 => _,
                mauve => _,
                green => _,
                yellow => _,
                red => _,
                blue => _,
                teal => _,
                peach => _,
            });
        }
        record_config_fields!(fields, terminal, "terminal", TerminalConfig {
            default_shell => _,
            login_shell => _,
            new_cwd => _,
        });
        record_config_fields!(fields, session, "session", SessionConfig {
            resume_agents_on_restore => _,
            startup_per_agent_delay as "startup_per_agent_delay_ms" => _,
        });
        record_config_fields!(fields, server, "server", HeadlessConfig {
            headless_cols => _,
            headless_rows => _,
        });
        record_config_fields!(fields, ui, "ui", ServerUiConfig {
            pane_borders => _,
            pane_outer_borders => _,
            pane_scrollbars => _,
            pane_gaps => _,
            show_agent_labels_on_pane_borders => _,
            window_title => _,
        });
        record_config_fields!(fields, advanced, "advanced", AdvancedConfig {
            scrollback_limit_bytes => _,
        });
        record_config_fields!(fields, experimental, "experimental", ExperimentalConfig {
            pane_history => _,
            reveal_hidden_cursor_for_cjk_ime => _,
            cjk_ime_agents => _,
            cjk_ime_cursor_shape => _,
        });
        fields
    }

    #[test]
    fn config_default_template_keeps_all_settings_comment_only() {
        for (name, template) in [
            ("default-client.toml", DEFAULT_CLIENT_CONFIG),
            ("default-server.toml", DEFAULT_SERVER_CONFIG),
        ] {
            for (index, line) in template.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
                    continue;
                }

                let line_number = index + 1;
                panic!("active setting on {name} line {line_number}: {line}");
            }
        }
    }

    /// The template names every built-in theme, and its commented `name`
    /// setting is the real default.
    #[test]
    fn default_template_lists_every_theme_and_the_default() {
        for template in [DEFAULT_CLIENT_CONFIG, DEFAULT_SERVER_CONFIG] {
            let words: Vec<&str> = template
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .collect();
            for name in theme::THEME_NAMES {
                assert!(words.contains(name), "config template does not list {name}");
            }
            let default_line = format!("# name = \"{}\"", theme::DEFAULT_THEME);
            assert!(
                template.lines().any(|line| line.trim() == default_line),
                "config template must show {default_line}"
            );
        }
    }

    /// The commented `[keys]` settings in the template, uncommented, are
    /// exactly the built-in keymap: every field is listed and every listed
    /// value is the real default.
    #[test]
    fn default_template_lists_every_keybinding_with_its_default() {
        let mut in_keys = false;
        let mut uncommented = String::from("[keys]\n");
        let mut listed = 0;
        for line in DEFAULT_CLIENT_CONFIG.lines() {
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
        let config: ClientConfig = toml::from_str(&uncommented).expect("template keys parse");
        assert_eq!(config.keys, KeysConfig::default());
        // The prefix plus every binding in the keybinding table.
        let mut fields = BTreeSet::new();
        record_key_config_fields(&mut fields, KeysConfig::default());
        assert_eq!(listed, fields.len(), "{uncommented}");
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
            (&keys.previous_workspace, "prefix+p"),
            (&keys.next_workspace, "prefix+n"),
            (&keys.previous_agent, ""),
            (&keys.next_agent, ""),
            (&keys.focus_agent, ""),
            (&keys.switch_workspace, "prefix+1..9"),
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
        let config: ClientConfig =
            toml::from_str("[keys]\nprefix = \"esc\"\n").expect("test precondition");
        let validation = config.compute_keybind_validation(|_| false);
        assert!(validation.live.is_none());
        assert!(
            validation.diagnostics.iter().any(|diag| {
                diag.key()
                    .is_some_and(|key| key.to_string().contains("navigate_back"))
                    && diag
                        .related_keys()
                        .iter()
                        .any(|key| key.to_string() == "keys.prefix")
            }),
            "{:?}",
            validation.diagnostics
        );

        let config: ClientConfig =
            toml::from_str("[keys]\nprefix = \"esc\"\nnavigate_back = \"\"\n")
                .expect("test precondition");
        assert!(config.compute_keybind_validation(|_| false).live.is_some());
    }

    #[test]
    fn keybind_parser_returns_only_complete_values() {
        for profile in ["", "[keys]\nprefix = \"ctrl+a\"\n"] {
            let config: ClientConfig = toml::from_str(profile).expect("test precondition");
            let validation = config.compute_keybind_validation(|_| false);
            let live = validation
                .live
                .expect("valid bindings produce a complete value");
            assert_eq!(live.keybinds.detach.label(), Some("prefix+q".into()));
        }

        let config: ClientConfig =
            toml::from_str("[keys]\nprefix = \"ctrl+\"\n").expect("test precondition");
        let validation = config.compute_keybind_validation(|_| false);
        assert!(validation.live.is_none());
        assert!(validation.diagnostics.iter().any(|diag| {
            diag.key()
                .is_some_and(|key| key.to_string() == "keys.prefix")
        }));
    }

    #[test]
    fn ui_host_cursor_defaults_to_native_and_parses_overrides() {
        let default_config = ClientConfig::default();
        assert_eq!(default_config.ui.host_cursor, HostCursorModeConfig::Native);

        let native: ClientConfig =
            toml::from_str("[ui]\nhost_cursor = 'native'\n").expect("test precondition");
        assert_eq!(native.ui.host_cursor, HostCursorModeConfig::Native);

        let drawn: ClientConfig =
            toml::from_str("[ui]\nhost_cursor = 'drawn'\n").expect("test precondition");
        assert_eq!(drawn.ui.host_cursor, HostCursorModeConfig::Drawn);

        assert!(toml::from_str::<ClientConfig>("[ui]\nhost_cursor = 'auto'\n").is_err());
    }
}
