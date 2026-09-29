use std::num::NonZeroUsize;

use crossterm::event::KeyModifiers;
use serde::{Deserialize, Deserializer, Serialize, de};

use super::{
    BindingConfig, DEFAULT_MOUSE_SCROLL_LINES, DEFAULT_SCROLLBACK_LIMIT_BYTES, SidebarConfig,
    TabBarRightEntryConfig, ThemeConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentPanelSortConfig {
    #[default]
    Spaces,
    Priority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StatusIndicatorStyle {
    #[default]
    Dots,
    Symbols,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostCursorModeConfig {
    #[default]
    Native,
    Drawn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarCollapsedModeConfig {
    #[default]
    Compact,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RightClickPassthroughModifierConfig(Option<KeyModifiers>);

impl RightClickPassthroughModifierConfig {
    pub fn modifiers(self) -> Option<KeyModifiers> {
        self.0
    }

    pub(crate) fn from_modifiers(modifiers: Option<KeyModifiers>) -> Self {
        Self(modifiers)
    }
}

impl Serialize for RightClickPassthroughModifierConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let value = self.0.map_or("off", canonical_right_click_modifier);
        serializer.serialize_str(value)
    }
}

impl<'de> Deserialize<'de> for RightClickPassthroughModifierConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_right_click_passthrough_modifier(&value)
            .map(Self)
            .map_err(de::Error::custom)
    }
}

const RIGHT_CLICK_MODIFIER_ALIASES: &[(&str, Option<KeyModifiers>)] = &[
    ("", None),
    ("off", None),
    ("none", None),
    ("disabled", None),
    ("ctrl", Some(KeyModifiers::CONTROL)),
    ("control", Some(KeyModifiers::CONTROL)),
    ("alt", Some(KeyModifiers::ALT)),
    ("option", Some(KeyModifiers::ALT)),
    // Terminal mouse reports encode Meta in Alt, so this alias resolves to Alt.
    ("meta", Some(KeyModifiers::ALT)),
    (
        "ctrl+alt",
        Some(KeyModifiers::CONTROL.union(KeyModifiers::ALT)),
    ),
];

fn canonical_right_click_modifier(modifiers: KeyModifiers) -> &'static str {
    RIGHT_CLICK_MODIFIER_ALIASES
        .iter()
        .find_map(|(alias, value)| (*value == Some(modifiers)).then_some(*alias))
        .unwrap_or("off")
}

fn right_click_modifier_values_error() -> String {
    let values = RIGHT_CLICK_MODIFIER_ALIASES
        .iter()
        .map(|(alias, _)| if alias.is_empty() { "empty" } else { *alias })
        .collect::<Vec<_>>()
        .join(", ");
    format!("right_click_passthrough_modifier must be one of: {values}")
}

fn parse_right_click_passthrough_modifier(value: &str) -> Result<Option<KeyModifiers>, String> {
    let trimmed = value.trim();
    if let Some((_, modifiers)) = RIGHT_CLICK_MODIFIER_ALIASES
        .iter()
        .find(|(alias, modifiers)| modifiers.is_none() && alias.eq_ignore_ascii_case(trimmed))
    {
        return Ok(*modifiers);
    }

    let mut modifiers = KeyModifiers::empty();
    for token in trimmed.split('+') {
        let token = token.trim().to_ascii_lowercase();
        let modifier = RIGHT_CLICK_MODIFIER_ALIASES
            .iter()
            .find_map(|(alias, value)| {
                (value.is_some() && !alias.contains('+') && alias.eq_ignore_ascii_case(&token))
                    .then_some(*value)
                    .flatten()
            });
        let Some(modifier) = modifier else {
            match token.as_str() {
                // A mouse report's button byte has bits for shift, alt and ctrl
                // only, so a super or hyper requirement could never be met.
                "cmd" | "command" | "super" | "hyper" => {
                    return Err(format!(
                        "right_click_passthrough_modifier cannot use {token:?}: terminal mouse reports only carry ctrl and alt"
                    ));
                }
                // Shift is left out on purpose: terminals commonly reserve
                // Shift+mouse for their own selection.
                "shift" => {
                    return Err(format!(
                        "{}; shift is unsupported",
                        right_click_modifier_values_error()
                    ));
                }
                _ => return Err(right_click_modifier_values_error()),
            }
        };
        modifiers |= modifier;
    }

    if modifiers.is_empty() {
        Err(right_click_modifier_values_error())
    } else {
        Ok(Some(modifiers))
    }
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

impl Serialize for NewTerminalCwdConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let value = match self {
            Self::Follow => "follow",
            Self::Home => "home",
            Self::Current => "current",
            Self::Path(path) => path,
        };
        serializer.serialize_str(value)
    }
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

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Executable used for new interactive panes. Empty means `$SHELL`, or
    /// /bin/sh when `SHELL` is unset; an unusable `SHELL` fails the launch.
    pub default_shell: String,
    /// Start new interactive pane shells as login shells. Default: false.
    pub login_shell: bool,
    /// CWD policy for new interactive panes, tabs, and workspaces.
    pub new_cwd: NewTerminalCwdConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Resume supported AI-agent panes into their native conversation sessions
    /// when restoring a Shepr session. Default: true.
    pub resume_agents_on_restore: bool,
    /// Milliseconds between automatic agent restores. Zero disables spacing.
    /// Default: 100.
    pub startup_per_agent_delay_ms: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            resume_agents_on_restore: true,
            startup_per_agent_delay_ms: 100,
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

impl SidebarBounds {
    pub fn min(self) -> u16 {
        self.min
    }

    pub fn max(self) -> u16 {
        self.max
    }

    pub fn clamp_width(self, width: u16) -> u16 {
        width.clamp(self.min, self.max)
    }
}

pub fn validated_sidebar_bounds(min: u16, max: u16) -> Option<SidebarBounds> {
    (min <= max).then_some(SidebarBounds { min, max })
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub theme: ThemeConfig,
    pub terminal: TerminalConfig,
    pub session: SessionConfig,
    pub server: ServerConfig,
    pub keys: KeysConfig,
    pub ui: UiConfig,
    pub advanced: AdvancedConfig,
    pub experimental: ExperimentalConfig,
    /// The `[[machines]]` entries, in file order.
    pub machines: Vec<super::MachineConfig>,
}

#[derive(Debug)]
pub(crate) struct LoadedConfig {
    pub(crate) config: Config,
    pub(crate) provenance: super::ConfigProvenance,
    pub(crate) resolution: super::validated::ConfigResolution,
    pub(crate) diagnostics: Vec<super::ConfigDiagnostic>,
}

impl LoadedConfig {
    pub(crate) fn into_validated(
        self,
        paths: super::AppPaths,
    ) -> Result<super::ValidatedConfig, Vec<super::ConfigDiagnostic>> {
        let Self {
            config,
            provenance,
            resolution,
            diagnostics,
            ..
        } = self;
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        match resolution.values {
            Some(values) => Ok(super::ValidatedConfig::from_loaded(
                config, provenance, values, paths,
            )),
            None => Err(vec![super::ConfigDiagnostic::Validation(
                "configuration resolution produced no values and no diagnostic; no invalid setting could be identified"
                    .to_owned(),
            )]),
        }
    }
}

macro_rules! define_keys_config {
    (
        actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
        indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
        navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
        navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
    ) => {
        #[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
        #[serde(default)]
        pub struct KeysConfig {
            /// Prefix key to enter prefix mode (for example, `ctrl+b` or `f12`).
            pub prefix: String,
            $(#[doc = $action_doc] pub $action_field: BindingConfig,)*
            $(#[doc = $indexed_doc] pub $indexed_field: BindingConfig,)*
            $(#[doc = $navigate_doc] pub $navigate_config_field: BindingConfig,)*
            $(#[doc = $navigate_indexed_doc] pub $navigate_indexed_config_field: BindingConfig,)*
        }

        impl Default for KeysConfig {
            fn default() -> Self {
                Self {
                    prefix: "ctrl+b".into(),
                    $($action_field: BindingConfig::one($action_default),)*
                    $($indexed_field: BindingConfig::one($indexed_default),)*
                    $($navigate_config_field: BindingConfig::one($navigate_default),)*
                    $($navigate_indexed_config_field: BindingConfig::one($navigate_indexed_default),)*
                }
            }
        }
    };
}

crate::keybinding_table!(define_keys_config);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabBarPositionConfig {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneBordersConfig {
    #[default]
    Auto,
    Always,
    Off,
}

impl PaneBordersConfig {
    pub fn draws_borders(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn shows_borders(self, multi_pane: bool) -> bool {
        self.draws_borders() && (multi_pane || matches!(self, Self::Always))
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct UiConfig {
    /// Expanded sidebar width (columns). Default: 26. While unset, the client
    /// shell remembers a width set by dragging the sidebar divider; once set,
    /// it wins at every launch.
    pub sidebar_width: u16,
    /// Minimum sidebar width (columns) when expanded. Default: 18.
    pub sidebar_min_width: u16,
    /// Maximum sidebar width (columns) when expanded. Default: 36.
    pub sidebar_max_width: u16,
    /// Start with the sidebar collapsed. Default: false. While unset, the
    /// client shell remembers the last collapse toggle; once set, it wins at
    /// every launch.
    pub sidebar_start_collapsed: bool,
    /// Collapsed sidebar presentation. Default: compact.
    pub sidebar_collapsed_mode: SidebarCollapsedModeConfig,
    /// Capture mouse input for Shepr's mouse UI. Default: true.
    pub mouse_capture: bool,
    /// Copy text selected with the mouse. Default: true.
    pub copy_on_select: bool,
    /// Host cursor policy. Default: auto.
    pub host_cursor: HostCursorModeConfig,
    /// Modifier that lets right-click gestures pass through to pane apps. Empty disables it.
    pub right_click_passthrough_modifier: RightClickPassthroughModifierConfig,
    /// Force a full host-terminal redraw when the outer terminal regains focus. Default: true.
    pub redraw_on_focus_gained: bool,
    /// Lines to scroll per mouse wheel notch. Default: 3.
    pub mouse_scroll_lines: Option<NonZeroUsize>,
    /// Ask for confirmation before closing a workspace. Default: true.
    pub confirm_close: bool,
    /// Ask for a tab name before creating a new tab. Default: true.
    pub prompt_new_tab_name: bool,
    /// Ask for a workspace name before interactive creation. Default: false.
    pub prompt_new_workspace_name: bool,
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
    /// Hide the tab row when the workspace has one tab. Default: false.
    pub hide_tab_bar_when_single_tab: bool,
    /// Desktop tab row placement. Default: top.
    pub tab_bar_position: TabBarPositionConfig,
    /// Ordered entries shown at the right edge of the desktop tab row. Empty by default.
    pub tab_bar_right: Vec<TabBarRightEntryConfig>,
    /// Text inserted between visible right-side tab bar entries. Default: one space.
    pub tab_bar_right_separator: String,
    /// Format for the outer terminal window title. Empty leaves the title alone.
    /// Default: "{hostname}: {workspace}".
    pub window_title: String,
    /// Agent sidebar ordering: "spaces" or "priority". Default: "spaces".
    /// While unset, the client shell remembers the last toggle of the agent
    /// panel's sort control; once set, it wins at every launch.
    pub agent_panel_sort: AgentPanelSortConfig,
    /// Agent status indicator style. Values are "dots" or "symbols". Default: "dots".
    pub status_indicators: StatusIndicatorStyle,
    /// Expanded sidebar row composition.
    pub sidebar: SidebarConfig,
    /// Accent color for highlights, borders, and navigation UI.
    /// Accepts hex (#89b4fa), named colors (cyan, blue), or RGB (rgb(137,180,250)).
    /// Applies when set in the config file; otherwise the theme accent applies.
    /// theme.custom.accent takes precedence.
    /// An empty string has the same meaning as unset.
    #[serde(default, deserialize_with = "deserialize_ui_accent")]
    pub accent: Option<String>,
}

fn deserialize_ui_accent<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.filter(|accent| !accent.is_empty()))
}

/// Cursor shape (DECSCUSR) used for the forced IME anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Virtual terminal width used when no client is attached. Default: 120.
    pub headless_cols: u16,
    /// Virtual terminal height used when no client is attached. Default: 40.
    pub headless_rows: u16,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct AdvancedConfig {
    /// Approximate scrollback budget in bytes per pane terminal, converted to a
    /// line count for the pane's width; 0 disables scrollback. Not a hard cap:
    /// any non-zero budget keeps at least 1000 lines, and a pane that is
    /// widened keeps the history it already holds rather than dropping it, so
    /// it can exceed the budget until it narrows again. Default: 10000000.
    pub scrollback_limit_bytes: usize,
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

fn serialize_cjk_ime_agents<S>(
    agents: &[crate::ConfigAgent],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq;

    let mut sequence = serializer.serialize_seq(Some(agents.len()))?;
    for agent in agents {
        sequence.serialize_element(agent.label())?;
    }
    sequence.end()
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ExperimentalConfig {
    /// Allow launching shepr inside an existing shepr pane. Default: false.
    pub allow_nested: bool,
    /// Persist pane screen history to session-history.json. Default: false.
    pub pane_history: bool,
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
    #[serde(
        deserialize_with = "deserialize_cjk_ime_agents",
        serialize_with = "serialize_cjk_ime_agents"
    )]
    pub cjk_ime_agents: Vec<crate::ConfigAgent>,
    /// Cursor shape rendered for the IME anchor when
    /// `reveal_hidden_cursor_for_cjk_ime` is enabled. Default: "steady_block".
    pub cjk_ime_cursor_shape: ImeCursorShape,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: 26,
            sidebar_min_width: 18,
            sidebar_max_width: 36,
            sidebar_start_collapsed: false,
            sidebar_collapsed_mode: SidebarCollapsedModeConfig::Compact,
            mouse_capture: true,
            copy_on_select: true,
            host_cursor: HostCursorModeConfig::Native,
            right_click_passthrough_modifier: RightClickPassthroughModifierConfig::default(),
            redraw_on_focus_gained: true,
            mouse_scroll_lines: None,
            confirm_close: true,
            prompt_new_tab_name: true,
            prompt_new_workspace_name: false,
            pane_borders: PaneBordersConfig::Auto,
            pane_outer_borders: true,
            pane_scrollbars: true,
            pane_gaps: true,
            show_agent_labels_on_pane_borders: false,
            hide_tab_bar_when_single_tab: false,
            tab_bar_position: TabBarPositionConfig::Top,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: " ".into(),
            window_title: super::window_title::default_window_title(),
            agent_panel_sort: AgentPanelSortConfig::Spaces,
            status_indicators: StatusIndicatorStyle::Dots,
            sidebar: SidebarConfig::default(),
            accent: None,
        }
    }
}

impl UiConfig {
    pub fn mouse_scroll_lines(&self) -> usize {
        self.mouse_scroll_lines
            .map_or(DEFAULT_MOUSE_SCROLL_LINES, NonZeroUsize::get)
    }

    pub fn right_click_passthrough_modifiers(&self) -> Option<KeyModifiers> {
        self.right_click_passthrough_modifier.modifiers()
    }
}

impl Default for ServerConfig {
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
    fn terminal_default_shell_defaults_empty_and_parses() {
        let default_config = Config::default();
        assert!(default_config.terminal.default_shell.is_empty());
        assert!(!default_config.terminal.login_shell);

        let toml = r#"
[terminal]
default_shell = "nu"
login_shell = true
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.terminal.default_shell, "nu");
        assert!(config.terminal.login_shell);
    }

    #[test]
    fn terminal_new_cwd_defaults_follow_and_parses() {
        let default_config = Config::default();
        assert_eq!(
            default_config.terminal.new_cwd,
            NewTerminalCwdConfig::Follow
        );

        let config: Config = toml::from_str(
            r#"
[terminal]
new_cwd = "home"
"#,
        )
        .expect("test precondition");
        assert_eq!(config.terminal.new_cwd, NewTerminalCwdConfig::Home);

        let config: Config = toml::from_str(
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

        let empty: Config =
            toml::from_str("[terminal]\nnew_cwd = \"\"\n").expect("empty path parses");
        assert_eq!(
            empty.terminal.new_cwd,
            NewTerminalCwdConfig::Path(String::new())
        );

        let padded: Config =
            toml::from_str("[terminal]\nnew_cwd = \" home \"\n").expect("literal path parses");
        assert_eq!(
            padded.terminal.new_cwd,
            NewTerminalCwdConfig::Path(" home ".into())
        );
    }

    #[test]
    fn resume_agents_on_restore_defaults_on_and_parses() {
        let default_config = Config::default();
        assert!(default_config.session.resume_agents_on_restore);
        assert_eq!(default_config.session.startup_per_agent_delay_ms, 100);

        let toml = r#"
[session]
resume_agents_on_restore = false
startup_per_agent_delay_ms = 0
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(!config.session.resume_agents_on_restore);
        assert_eq!(config.session.startup_per_agent_delay_ms, 0);
    }

    #[test]
    fn agent_panel_sort_config_parses_and_defaults() {
        assert_eq!(
            Config::default().ui.agent_panel_sort,
            AgentPanelSortConfig::Spaces
        );

        let toml = r#"
[ui]
agent_panel_sort = "priority"
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.ui.agent_panel_sort, AgentPanelSortConfig::Priority);
    }

    #[test]
    fn status_indicator_style_defaults_to_dots_and_parses_symbols() {
        assert_eq!(
            Config::default().ui.status_indicators,
            StatusIndicatorStyle::Dots
        );

        let config: Config = toml::from_str(
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
        let auto: Config =
            toml::from_str("[ui]\npane_borders = \"auto\"").expect("test precondition");
        assert_eq!(auto.ui.pane_borders, PaneBordersConfig::Auto);

        let off: Config =
            toml::from_str("[ui]\npane_borders = \"off\"").expect("test precondition");
        assert_eq!(off.ui.pane_borders, PaneBordersConfig::Off);

        assert!(toml::from_str::<Config>("[ui]\npane_borders = \"framed\"").is_err());
        assert!(toml::from_str::<Config>("[ui]\npane_borders = true").is_err());
    }

    #[test]
    fn pane_appearance_defaults_and_parse() {
        let default_config = Config::default();
        assert_eq!(default_config.ui.pane_borders, PaneBordersConfig::Auto);
        assert!(default_config.ui.pane_outer_borders);
        assert!(default_config.ui.pane_scrollbars);
        assert!(default_config.ui.pane_gaps);
        assert!(!default_config.ui.show_agent_labels_on_pane_borders);
        assert!(!default_config.ui.hide_tab_bar_when_single_tab);
        assert_eq!(
            default_config.ui.tab_bar_position,
            TabBarPositionConfig::Top
        );
        assert!(default_config.ui.tab_bar_right.is_empty());
        assert_eq!(default_config.ui.tab_bar_right_separator, " ");

        let toml = r#"
[ui]
pane_borders = "always"
pane_outer_borders = false
pane_scrollbars = false
pane_gaps = true
show_agent_labels_on_pane_borders = true
hide_tab_bar_when_single_tab = true
tab_bar_position = "bottom"
tab_bar_right = [
  { type = "zoom" },
  { type = "hostname" },
  { type = "datetime", format = "%H:%M" },
  { type = "text", text = "prod" },
  { type = "command", command = "status.sh", interval_seconds = 10, timeout_seconds = 3 },
]
tab_bar_right_separator = " · "
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.ui.pane_borders, PaneBordersConfig::Always);
        assert!(!config.ui.pane_outer_borders);
        assert!(!config.ui.pane_scrollbars);
        assert!(config.ui.pane_gaps);
        assert!(config.ui.show_agent_labels_on_pane_borders);
        assert!(config.ui.hide_tab_bar_when_single_tab);
        assert_eq!(config.ui.tab_bar_position, TabBarPositionConfig::Bottom);
        assert_eq!(config.ui.tab_bar_right.len(), 5);
        assert!(matches!(
            config.ui.tab_bar_right[1],
            TabBarRightEntryConfig::Hostname
        ));
        assert_eq!(config.ui.tab_bar_right_separator, " · ");
    }

    #[test]
    fn prompt_new_tab_name_defaults_on_and_parses() {
        let default_config = Config::default();
        assert!(default_config.ui.prompt_new_tab_name);

        let toml = r#"
[ui]
prompt_new_tab_name = false
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(!config.ui.prompt_new_tab_name);
    }

    #[test]
    fn prompt_new_workspace_name_defaults_off_and_parses() {
        let default_config = Config::default();
        assert!(!default_config.ui.prompt_new_workspace_name);

        let toml = r#"
[ui]
prompt_new_workspace_name = true
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(config.ui.prompt_new_workspace_name);
    }

    #[test]
    fn reveal_hidden_cursor_for_cjk_ime_default_off_and_parse() {
        let default_config = Config::default();
        assert!(!default_config.experimental.reveal_hidden_cursor_for_cjk_ime);

        let toml = r#"
[experimental]
reveal_hidden_cursor_for_cjk_ime = true
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(config.experimental.reveal_hidden_cursor_for_cjk_ime);
    }

    #[test]
    fn cjk_ime_cursor_shape_default_steady_block_and_parse() {
        let default_config = Config::default();
        assert_eq!(
            default_config.experimental.cjk_ime_cursor_shape,
            ImeCursorShape::SteadyBlock
        );

        let toml = r#"
[experimental]
cjk_ime_cursor_shape = "bar"
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(
            config.experimental.cjk_ime_cursor_shape,
            ImeCursorShape::Bar
        );
    }

    #[test]
    fn cjk_ime_agents_default_empty_and_parse() {
        let default_config = Config::default();
        assert!(default_config.experimental.cjk_ime_agents.is_empty());

        let toml = r#"
[experimental]
cjk_ime_agents = ["claude", "codex", " Claude-Code "]
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(
            config.experimental.cjk_ime_agents,
            vec![crate::ConfigAgent::Claude, crate::ConfigAgent::Codex]
        );
    }

    #[test]
    fn cjk_ime_agents_reject_unknown_names() {
        let err = toml::from_str::<Config>(
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
        let default_config = Config::default();
        assert_eq!(default_config.ui.sidebar_min_width, 18);
        assert_eq!(default_config.ui.sidebar_max_width, 36);

        let toml = r#"
[ui]
sidebar_min_width = 12
sidebar_max_width = 80
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.ui.sidebar_min_width, 12);
        assert_eq!(config.ui.sidebar_max_width, 80);
    }

    #[test]
    fn sidebar_start_collapsed_defaults_off_and_parses_on() {
        let default_config = Config::default();
        assert!(!default_config.ui.sidebar_start_collapsed);

        let toml = r#"
[ui]
sidebar_start_collapsed = true
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(config.ui.sidebar_start_collapsed);
    }

    #[test]
    fn sidebar_collapsed_mode_defaults_compact_and_parses_hidden() {
        let default_config = Config::default();
        assert_eq!(
            default_config.ui.sidebar_collapsed_mode,
            SidebarCollapsedModeConfig::Compact
        );

        let toml = r#"
[ui]
sidebar_collapsed_mode = "hidden"
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(
            config.ui.sidebar_collapsed_mode,
            SidebarCollapsedModeConfig::Hidden
        );
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
        let default_config = Config::default();
        assert!(default_config.ui.mouse_capture);

        let toml = r#"
[ui]
mouse_capture = false
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(!config.ui.mouse_capture);
    }

    #[test]
    fn copy_on_select_default_on_and_parse() {
        let default_config = Config::default();
        assert!(default_config.ui.copy_on_select);

        let toml = r#"
[ui]
copy_on_select = false
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(!config.ui.copy_on_select);
    }

    #[test]
    fn right_click_passthrough_modifier_defaults_off_and_parses() {
        let default_config = Config::default();
        assert_eq!(default_config.ui.right_click_passthrough_modifiers(), None);

        for value in ["", "off", "none", "disabled"] {
            let toml = format!(
                r#"
[ui]
right_click_passthrough_modifier = "{value}"
"#
            );
            let config: Config = toml::from_str(&toml).expect("test precondition");
            assert_eq!(
                config.ui.right_click_passthrough_modifiers(),
                None,
                "value {value:?} should disable passthrough"
            );
        }

        for (value, expected) in [
            ("ctrl", KeyModifiers::CONTROL),
            ("control", KeyModifiers::CONTROL),
            ("alt", KeyModifiers::ALT),
            ("option", KeyModifiers::ALT),
            ("meta", KeyModifiers::ALT),
            ("ctrl+alt", KeyModifiers::CONTROL | KeyModifiers::ALT),
            ("Control + Meta", KeyModifiers::CONTROL | KeyModifiers::ALT),
        ] {
            let toml = format!(
                r#"
[ui]
right_click_passthrough_modifier = "{value}"
"#
            );
            let config: Config = toml::from_str(&toml).expect("test precondition");
            assert_eq!(
                config.ui.right_click_passthrough_modifiers(),
                Some(expected),
                "value {value:?} should parse"
            );
        }
    }

    /// Every alias parses to its table value, serializes to a canonical alias
    /// with that same value, and is named in the error message.
    #[test]
    fn right_click_modifier_aliases_round_trip() {
        let error = right_click_modifier_values_error();
        for (alias, value) in RIGHT_CLICK_MODIFIER_ALIASES {
            let parsed = parse_right_click_passthrough_modifier(alias).expect("alias parses");
            assert_eq!(parsed, *value, "alias {alias:?}");
            let serialized = serde_json::to_value(RightClickPassthroughModifierConfig(parsed))
                .expect("serializes");
            let canonical = serialized.as_str().expect("a string");
            assert_eq!(
                parse_right_click_passthrough_modifier(canonical).expect("canonical parses"),
                *value,
                "alias {alias:?} serialized as {canonical:?}"
            );
            if !alias.is_empty() {
                assert!(error.contains(alias), "{error} omits {alias:?}");
            }
        }
    }

    #[test]
    fn right_click_passthrough_modifier_rejects_modifiers_mouse_reports_cannot_carry() {
        for value in ["cmd", "command", "super", "hyper", "cmd+alt", "ctrl+hyper"] {
            let toml = format!(
                r#"
[ui]
right_click_passthrough_modifier = "{value}"
"#
            );
            let error = toml::from_str::<Config>(&toml)
                .expect_err("a modifier mouse reports cannot carry must be rejected")
                .to_string();
            assert!(
                error.contains("only carry ctrl and alt"),
                "value {value:?} gave {error}"
            );
        }
    }

    #[test]
    fn right_click_passthrough_modifier_rejects_shift() {
        for value in ["shift", "shift+ctrl", "ctrl+", "ctrl++alt", "banana"] {
            let toml = format!(
                r#"
[ui]
right_click_passthrough_modifier = "{value}"
"#
            );
            assert!(
                toml::from_str::<Config>(&toml).is_err(),
                "value {value:?} should be rejected"
            );
        }
    }

    #[test]
    fn redraw_on_focus_gained_default_on_and_parse() {
        let default_config = Config::default();
        assert!(default_config.ui.redraw_on_focus_gained);

        let toml = r#"
[ui]
redraw_on_focus_gained = false
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(!config.ui.redraw_on_focus_gained);
    }

    #[test]
    fn mouse_scroll_lines_defaults_to_three_and_parses() {
        let default_config = Config::default();
        assert_eq!(
            default_config.ui.mouse_scroll_lines(),
            DEFAULT_MOUSE_SCROLL_LINES
        );

        let toml = r#"
[ui]
mouse_scroll_lines = 1
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.ui.mouse_scroll_lines(), 1);
    }

    #[test]
    fn mouse_scroll_lines_rejects_zero() {
        let toml = r#"
[ui]
mouse_scroll_lines = 0
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn server_headless_size_defaults_and_parses() {
        let default_config = Config::default();
        assert_eq!(
            default_config.server.headless_cols,
            crate::DEFAULT_HEADLESS_COLS
        );
        assert_eq!(
            default_config.server.headless_rows,
            crate::DEFAULT_HEADLESS_ROWS
        );

        let config: Config = toml::from_str(
            r#"[server]
headless_cols = 160
headless_rows = 50
"#,
        )
        .expect("test precondition");
        assert_eq!(config.server.headless_cols, 160);
        assert_eq!(config.server.headless_rows, 50);

        let invalid: Config = toml::from_str(
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
        let config = Config::default();
        assert_eq!(
            config.advanced.scrollback_limit_bytes,
            DEFAULT_SCROLLBACK_LIMIT_BYTES
        );
    }

    #[test]
    fn pane_history_persistence_is_opt_in() {
        assert!(!Config::default().experimental.pane_history);

        let toml = r#"
[experimental]
pane_history = true
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");

        assert!(config.experimental.pane_history);
    }

    #[test]
    fn experimental_config_parses() {
        let toml = r#"
[experimental]
allow_nested = true
pane_history = true
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert!(config.experimental.allow_nested);
        assert!(config.experimental.pane_history);
    }

    #[test]
    fn advanced_config_parses() {
        let toml = r#"
[advanced]
scrollback_limit_bytes = 12345
"#;
        let config: Config = toml::from_str(toml).expect("test precondition");
        assert_eq!(config.advanced.scrollback_limit_bytes, 12345);
    }
}
