use serde::{Deserialize, Deserializer, de};

use super::{BindingConfig, DEFAULT_SCROLLBACK_LIMIT_BYTES, SidebarConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StatusIndicatorStyle {
    #[default]
    Dots,
    Symbols,
}

/// The exact strings `follow`, `home`, and `current` are policy keywords;
/// every other string is preserved as a literal path for launch-time parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NewTerminalCwdConfig {
    #[default]
    Follow,
    Home,
    Current,
    Path(String),
}

impl<'de> Deserialize<'de> for NewTerminalCwdConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "follow" => Ok(Self::Follow),
            "home" => Ok(Self::Home),
            "current" => Ok(Self::Current),
            _ => Ok(Self::Path(value)),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Executable used for new interactive panes. Unset means `$SHELL`, or
    /// /bin/sh when `SHELL` is unset; an unusable `SHELL` fails the launch.
    /// A set value and `SHELL` must have no surrounding whitespace.
    pub default_shell: Option<String>,
    /// Start new interactive pane shells as login shells. Default: false.
    pub login_shell: bool,
    /// CWD policy for new interactive panes and workspaces.
    pub new_cwd: NewTerminalCwdConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Resume supported AI-agent panes into their native conversation sessions
    /// when restoring a Shepr session. Default: true.
    pub resume_agents_on_restore: bool,
    /// Time between automatic agent restores. Zero disables spacing. The TOML
    /// key is `startup_per_agent_delay_ms`, in milliseconds. Default: 100 ms.
    #[serde(
        rename = "startup_per_agent_delay_ms",
        deserialize_with = "deserialize_millis"
    )]
    pub startup_per_agent_delay: std::time::Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            resume_agents_on_restore: true,
            startup_per_agent_delay: crate::limits::DEFAULT_STARTUP_PER_AGENT_DELAY,
        }
    }
}

/// Validate `[ui]` sidebar bound configuration.
///
/// Returns bounds when `min <= max`, `None` otherwise. The two
/// values are funneled through this helper before they reach any
/// `u16::clamp(min, max)` call site (`u16::clamp` panics when `min > max`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarBounds {
    min: u16,
    max: u16,
}

/// An expanded sidebar width that has passed through its configured bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarWidth(u16);

impl SidebarWidth {
    pub fn value(self) -> u16 {
        self.0
    }
}

impl SidebarBounds {
    pub fn min(self) -> u16 {
        self.min
    }

    pub fn max(self) -> u16 {
        self.max
    }

    pub fn clamp_width(self, width: u16) -> SidebarWidth {
        SidebarWidth(width.clamp(self.min, self.max))
    }

    pub(crate) fn checked_width(self, width: u16) -> Option<SidebarWidth> {
        (self.min..=self.max)
            .contains(&width)
            .then_some(SidebarWidth(width))
    }
}

pub(crate) fn validated_sidebar_bounds(min: u16, max: u16) -> Option<SidebarBounds> {
    (min <= max).then_some(SidebarBounds { min, max })
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub keys: KeysConfig,
    pub ui: ClientUiConfig,
    pub local: super::LocalConfig,
    pub machines: Vec<super::MachineConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub terminal: TerminalConfig,
    pub session: SessionConfig,
    pub server: HeadlessConfig,
    pub ui: ServerUiConfig,
    pub advanced: AdvancedConfig,
    pub experimental: ExperimentalConfig,
}

crate::keybinding_rows! {
    $ define_keys_config;
    actions(field = $action_field, default = $action_default, doc = $action_doc)
    indexed(field = $indexed_field, default = $indexed_default, doc = $indexed_doc)
    navigate(config_field = $navigate_config_field, default = $navigate_default, doc = $navigate_doc)
    => {
        #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
        #[serde(default)]
        pub struct KeysConfig {
            /// Prefix key to enter prefix mode (for example, `ctrl+b` or `f12`).
            pub prefix: String,
            $(#[doc = $action_doc] pub $action_field: BindingConfig,)*
            $(#[doc = $indexed_doc] pub $indexed_field: BindingConfig,)*
            $(#[doc = $navigate_doc] pub $navigate_config_field: BindingConfig,)*
        }

        impl Default for KeysConfig {
            fn default() -> Self {
                Self {
                    prefix: "ctrl+b".into(),
                    $($action_field: BindingConfig::one($action_default),)*
                    $($indexed_field: BindingConfig::one($indexed_default),)*
                    $($navigate_config_field: BindingConfig::one($navigate_default),)*
                }
            }
        }
    }
}

/// The setting is the core chrome math's own mode, so the config value is what
/// the pane chrome computation takes.
pub use shepr_core::chrome::PaneBorders as PaneBordersConfig;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ClientUiConfig {
    /// Expanded sidebar width (columns). Default: 26. While unset, the client
    /// shell remembers a width set by dragging the sidebar divider; once set,
    /// it wins at every launch.
    pub sidebar_width: Option<u16>,
    /// Minimum sidebar width (columns) when expanded. Default: 18.
    pub sidebar_min_width: u16,
    /// Maximum sidebar width (columns) when expanded. Default: 36.
    pub sidebar_max_width: u16,
    /// Start with the sidebar collapsed. Default: false. While unset, the
    /// client shell remembers the last collapse toggle; once set, it wins at
    /// every launch.
    pub sidebar_start_collapsed: Option<bool>,
    /// Capture mouse input for Shepr's mouse UI. Default: true.
    pub mouse_capture: bool,
    /// Copy text selected with the mouse. Default: true.
    pub copy_on_select: bool,
    /// Ask for confirmation before closing a workspace. Default: true.
    pub confirm_close: bool,
    /// Agent status indicator style. Values are "dots" or "symbols". Default: "dots".
    pub status_indicators: StatusIndicatorStyle,
    /// Expanded sidebar row composition.
    pub sidebar: SidebarConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServerUiConfig {
    /// Draw borders around split panes. auto draws them only for split panes,
    /// always also frames a lone pane (only while pane_outer_borders is
    /// enabled, since every edge of a lone pane is an outer edge), off
    /// disables them. Default: auto.
    pub pane_borders: PaneBordersConfig,
    /// Draw borders along the outside edge of the pane area. Default: true.
    pub pane_outer_borders: bool,
    /// Draw interactive scrollbars beside terminal panes. Default: true.
    pub pane_scrollbars: bool,
    /// Keep split panes visually separated instead of sharing divider borders. Default: true.
    pub pane_gaps: bool,
    /// Show agent labels in split pane borders when no manual pane label is set. Default: false.
    pub show_agent_labels_on_pane_borders: bool,
}

/// Cursor shape (DECSCUSR) used for the forced IME anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImeCursorShape {
    Block,
    #[default]
    SteadyBlock,
    Underline,
    SteadyUnderline,
    Bar,
    SteadyBar,
}

impl ImeCursorShape {
    /// Convert to DECSCUSR parameter (1-6).
    pub fn to_decscusr(self) -> u8 {
        match self {
            Self::Block => 1,
            Self::SteadyBlock => 2,
            Self::Underline => 3,
            Self::SteadyUnderline => 4,
            Self::Bar => 5,
            Self::SteadyBar => 6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct HeadlessConfig {
    /// Virtual terminal width used when no client is attached. Default: 120.
    pub headless_cols: u16,
    /// Virtual terminal height used when no client is attached. Default: 40.
    pub headless_rows: u16,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AdvancedConfig {
    /// Approximate scrollback budget in bytes per pane terminal, converted to a
    /// line count for the pane's width (`shepr_core::scrollback::ScrollbackBudget`
    /// owns the policy); 0 disables scrollback. Not a hard cap:
    /// any non-zero budget keeps at least 1000 lines, and a pane that is
    /// widened keeps the history it already holds rather than dropping it, so
    /// it can exceed the budget until it narrows again. Default: 10000000.
    pub scrollback_limit_bytes: usize,
}

/// A duration written in the TOML as whole milliseconds.
fn deserialize_millis<'de, D>(deserializer: D) -> Result<std::time::Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(std::time::Duration::from_millis(
        u32::deserialize(deserializer)?.into(),
    ))
}

fn deserialize_cjk_ime_agents<'de, D>(deserializer: D) -> Result<Vec<crate::ConfigAgent>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let names = Vec::<String>::deserialize(deserializer)?;
    let mut agents = Vec::with_capacity(names.len());
    for name in names {
        let Some(agent) = crate::agent::parse_config_agent(&name) else {
            return Err(de::Error::custom(format!(
                "unknown agent name {name:?} in experimental.cjk_ime_agents"
            )));
        };
        if !agents.contains(&agent) {
            agents.push(agent);
        }
    }
    Ok(agents)
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ExperimentalConfig {
    /// Expose the focused pane's cursor anchor to the outer terminal even when
    /// the pane requested `?25l`, so an input method (fcitx5, ibus) that
    /// places its candidate window at the terminal cursor keeps tracking the
    /// input position when TUIs paint their own cursor (Claude Code, pi,
    /// codex, etc.). Default: false.
    ///
    /// When the pane reports no cursor position, falls back to the pane's
    /// top-left so a stable IME anchor is always available.
    ///
    /// Trade-off when enabled: an extra hardware cursor will be visible in the
    /// outer terminal for apps that hide the cursor without painting a
    /// replacement (vim normal mode, etc.).
    pub reveal_hidden_cursor_for_cjk_ime: bool,
    /// Restrict `reveal_hidden_cursor_for_cjk_ime` to focused panes whose
    /// detected agent matches one of these names (case-insensitive). Empty
    /// list means apply to any focused pane. Unknown names are a config error.
    /// Agent labels and aliases are accepted; executable paths and suffixes are
    /// not config names.
    /// Default: empty.
    #[serde(deserialize_with = "deserialize_cjk_ime_agents")]
    pub cjk_ime_agents: Vec<crate::ConfigAgent>,
    /// Cursor shape rendered for the IME anchor when
    /// `reveal_hidden_cursor_for_cjk_ime` is enabled. Default: "steady_block".
    pub cjk_ime_cursor_shape: ImeCursorShape,
}

impl Default for ClientUiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: None,
            sidebar_min_width: 18,
            sidebar_max_width: 36,
            sidebar_start_collapsed: None,
            mouse_capture: true,
            copy_on_select: true,
            confirm_close: true,
            status_indicators: StatusIndicatorStyle::Dots,
            sidebar: SidebarConfig::default(),
        }
    }
}

impl Default for ServerUiConfig {
    fn default() -> Self {
        Self {
            pane_borders: PaneBordersConfig::Auto,
            pane_outer_borders: true,
            pane_scrollbars: true,
            pane_gaps: true,
            show_agent_labels_on_pane_borders: false,
        }
    }
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            headless_cols: crate::DEFAULT_HEADLESS_COLS,
            headless_rows: crate::DEFAULT_HEADLESS_ROWS,
        }
    }
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            scrollback_limit_bytes: DEFAULT_SCROLLBACK_LIMIT_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_default_shell_defaults_unset_and_parses() {
        let default_config = ServerConfig::default();
        assert_eq!(default_config.terminal.default_shell, None);
        assert!(!default_config.terminal.login_shell);

        let toml = r#"
[terminal]
default_shell = "nu"
login_shell = true
"#;
        let config: ServerConfig = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.terminal.default_shell.as_deref(), Some("nu"));
        assert!(config.terminal.login_shell);
    }

    #[test]
    fn terminal_new_cwd_defaults_follow_and_parses() {
        let default_config = ServerConfig::default();
        assert_eq!(
            default_config.terminal.new_cwd,
            NewTerminalCwdConfig::Follow
        );

        let config: ServerConfig = toml::from_str(
            r#"
[terminal]
new_cwd = "home"
"#,
        )
        .expect("test precondition");
        assert_eq!(config.terminal.new_cwd, NewTerminalCwdConfig::Home);

        let config: ServerConfig = toml::from_str(
            r#"
[terminal]
new_cwd = "~/Projects"
"#,
        )
        .expect("test precondition");
        assert_eq!(
            config.terminal.new_cwd,
            NewTerminalCwdConfig::Path("~/Projects".into())
        );

        let empty: ServerConfig =
            toml::from_str("[terminal]\nnew_cwd = \"\"\n").expect("empty path parses");
        assert_eq!(
            empty.terminal.new_cwd,
            NewTerminalCwdConfig::Path(String::new())
        );

        let padded: ServerConfig =
            toml::from_str("[terminal]\nnew_cwd = \" home \"\n").expect("literal path parses");
        assert_eq!(
            padded.terminal.new_cwd,
            NewTerminalCwdConfig::Path(" home ".into())
        );
    }

    #[test]
    fn resume_agents_on_restore_defaults_on_and_parses() {
        let default_config = ServerConfig::default();
        assert!(default_config.session.resume_agents_on_restore);
        assert_eq!(
            default_config.session.startup_per_agent_delay,
            std::time::Duration::from_millis(100)
        );

        let toml = r#"
[session]
resume_agents_on_restore = false
startup_per_agent_delay_ms = 0
"#;
        let config: ServerConfig = toml::from_str(toml).expect("test precondition");
        assert!(!config.session.resume_agents_on_restore);
        assert_eq!(
            config.session.startup_per_agent_delay,
            std::time::Duration::ZERO
        );
    }

    #[test]
    fn status_indicator_style_defaults_to_dots_and_parses_symbols() {
        assert_eq!(
            ClientConfig::default().ui.status_indicators,
            StatusIndicatorStyle::Dots
        );

        let config: ClientConfig = toml::from_str(
            r#"
[ui]
status_indicators = "symbols"
"#,
        )
        .expect("test precondition");
        assert_eq!(config.ui.status_indicators, StatusIndicatorStyle::Symbols);
    }

    #[test]
    fn pane_borders_parse_modes() {
        let auto: ServerConfig =
            toml::from_str("[ui]\npane_borders = \"auto\"").expect("test precondition");
        assert_eq!(auto.ui.pane_borders, PaneBordersConfig::Auto);

        let off: ServerConfig =
            toml::from_str("[ui]\npane_borders = \"off\"").expect("test precondition");
        assert_eq!(off.ui.pane_borders, PaneBordersConfig::Off);

        assert!(toml::from_str::<ServerConfig>("[ui]\npane_borders = \"framed\"").is_err());
        assert!(toml::from_str::<ServerConfig>("[ui]\npane_borders = true").is_err());
    }

    #[test]
    fn pane_appearance_defaults_and_parse() {
        let default_config = ServerConfig::default();
        assert_eq!(default_config.ui.pane_borders, PaneBordersConfig::Auto);
        assert!(default_config.ui.pane_outer_borders);
        assert!(default_config.ui.pane_scrollbars);
        assert!(default_config.ui.pane_gaps);
        assert!(!default_config.ui.show_agent_labels_on_pane_borders);

        let toml = r#"
[ui]
pane_borders = "always"
pane_outer_borders = false
pane_scrollbars = false
pane_gaps = true
show_agent_labels_on_pane_borders = true
"#;
        let config: ServerConfig = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.ui.pane_borders, PaneBordersConfig::Always);
        assert!(!config.ui.pane_outer_borders);
        assert!(!config.ui.pane_scrollbars);
        assert!(config.ui.pane_gaps);
        assert!(config.ui.show_agent_labels_on_pane_borders);
    }

    #[test]
    fn reveal_hidden_cursor_for_cjk_ime_default_off_and_parse() {
        let default_config = ServerConfig::default();
        assert!(!default_config.experimental.reveal_hidden_cursor_for_cjk_ime);

        let toml = r#"
[experimental]
reveal_hidden_cursor_for_cjk_ime = true
"#;
        let config: ServerConfig = toml::from_str(toml).expect("test precondition");
        assert!(config.experimental.reveal_hidden_cursor_for_cjk_ime);
    }

    #[test]
    fn cjk_ime_cursor_shape_default_steady_block_and_parse() {
        let default_config = ServerConfig::default();
        assert_eq!(
            default_config.experimental.cjk_ime_cursor_shape,
            ImeCursorShape::SteadyBlock
        );

        let toml = r#"
[experimental]
cjk_ime_cursor_shape = "bar"
"#;
        let config: ServerConfig = toml::from_str(toml).expect("test precondition");
        assert_eq!(
            config.experimental.cjk_ime_cursor_shape,
            ImeCursorShape::Bar
        );
    }

    #[test]
    fn cjk_ime_agents_default_empty_and_parse() {
        let default_config = ServerConfig::default();
        assert!(default_config.experimental.cjk_ime_agents.is_empty());

        let toml = r#"
[experimental]
cjk_ime_agents = ["claude", "codex", " Claude-Code "]
"#;
        let config: ServerConfig = toml::from_str(toml).expect("test precondition");
        assert_eq!(
            config.experimental.cjk_ime_agents,
            vec![crate::ConfigAgent::Claude, crate::ConfigAgent::Codex]
        );
    }

    #[test]
    fn cjk_ime_agents_reject_unknown_names() {
        let err = toml::from_str::<ServerConfig>(
            r#"
[experimental]
cjk_ime_agents = ["claude", "typo"]
"#,
        )
        .expect_err("unknown cjk_ime_agents names are config errors");

        assert!(err.to_string().contains("unknown agent name \"typo\""));
    }

    #[test]
    fn sidebar_bounds_default_and_parse() {
        let default_config = ClientConfig::default();
        assert_eq!(default_config.ui.sidebar_min_width, 18);
        assert_eq!(default_config.ui.sidebar_max_width, 36);

        let toml = r#"
[ui]
sidebar_min_width = 12
sidebar_max_width = 80
"#;
        let config: ClientConfig = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.ui.sidebar_min_width, 12);
        assert_eq!(config.ui.sidebar_max_width, 80);
    }

    #[test]
    fn sidebar_start_collapsed_defaults_off_and_parses_on() {
        let default_config = ClientConfig::default();
        assert_eq!(default_config.ui.sidebar_start_collapsed, None);

        let toml = r#"
[ui]
sidebar_start_collapsed = true
"#;
        let config: ClientConfig = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.ui.sidebar_start_collapsed, Some(true));
    }

    #[test]
    fn validated_sidebar_bounds_rejects_inverted() {
        assert_eq!(
            validated_sidebar_bounds(18, 36),
            Some(SidebarBounds { min: 18, max: 36 })
        );
        assert_eq!(
            validated_sidebar_bounds(20, 20),
            Some(SidebarBounds { min: 20, max: 20 })
        );
        assert_eq!(
            validated_sidebar_bounds(0, u16::MAX),
            Some(SidebarBounds {
                min: 0,
                max: u16::MAX
            })
        );
        assert_eq!(validated_sidebar_bounds(50, 30), None);
        assert_eq!(validated_sidebar_bounds(u16::MAX, 0), None);
    }

    #[test]
    fn mouse_capture_default_on_and_parse() {
        let default_config = ClientConfig::default();
        assert!(default_config.ui.mouse_capture);

        let toml = r#"
[ui]
mouse_capture = false
"#;
        let config: ClientConfig = toml::from_str(toml).expect("test precondition");
        assert!(!config.ui.mouse_capture);
    }

    #[test]
    fn copy_on_select_default_on_and_parse() {
        let default_config = ClientConfig::default();
        assert!(default_config.ui.copy_on_select);

        let toml = r#"
[ui]
copy_on_select = false
"#;
        let config: ClientConfig = toml::from_str(toml).expect("test precondition");
        assert!(!config.ui.copy_on_select);
    }

    #[test]
    fn server_headless_size_defaults_and_parses() {
        let default_config = ServerConfig::default();
        assert_eq!(
            default_config.server.headless_cols,
            crate::DEFAULT_HEADLESS_COLS
        );
        assert_eq!(
            default_config.server.headless_rows,
            crate::DEFAULT_HEADLESS_ROWS
        );

        let config: ServerConfig = toml::from_str(
            r#"[server]
headless_cols = 160
headless_rows = 50
"#,
        )
        .expect("test precondition");
        assert_eq!(config.server.headless_cols, 160);
        assert_eq!(config.server.headless_rows, 50);

        let invalid: ServerConfig = toml::from_str(
            r#"[server]
headless_cols = 0
headless_rows = 50
"#,
        )
        .expect("test precondition");
        assert!(
            invalid
                .collect_diagnostics()
                .iter()
                .any(|diag| diag.contains("server.headless_cols"))
        );
        assert_eq!(invalid.server.headless_cols, 0);
    }

    #[test]
    fn advanced_defaults_include_scrollback_limit_bytes() {
        let config = ServerConfig::default();
        assert_eq!(
            config.advanced.scrollback_limit_bytes,
            DEFAULT_SCROLLBACK_LIMIT_BYTES
        );
    }

    #[test]
    fn advanced_config_parses() {
        let toml = r#"
[advanced]
scrollback_limit_bytes = 12345
"#;
        let config: ServerConfig = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.advanced.scrollback_limit_bytes, 12345);
    }
}
