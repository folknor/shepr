use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    AgentPanelSortConfig, BindingConfig, HostCursorModeConfig, NewTerminalCwdConfig,
    PaneBordersConfig, RightClickPassthroughModifierConfig, SidebarCollapsedModeConfig,
    SidebarTokenRule, StatusIndicatorStyle, TabBarPositionConfig, TabBarRightEntryConfig,
    ThemeConfig,
    model::{
        AdvancedConfig, Config, ExperimentalConfig, KeysConfig, RemoteConfig, ServerConfig,
        SessionConfig, TerminalConfig, UiConfig,
    },
    sidebar::{
        AgentSidebarToken, AgentsSidebarConfig, SidebarConfig, SpaceSidebarToken,
        SpacesSidebarConfig, WireSidebarTokenRule, WireSidebarTokenStyle,
    },
    tab_bar::TabBarRightEntryConfig as ConfigTabBarEntry,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WireConfig {
    theme: ThemeConfig,
    terminal: WireTerminalConfig,
    session: SessionConfig,
    server: ServerConfig,
    keys: WireKeysConfig,
    ui: WireUiConfig,
    advanced: AdvancedConfig,
    experimental: WireExperimentalConfig,
    remote: RemoteConfig,
}

impl WireConfig {
    pub(super) fn from_config(config: &Config) -> Self {
        Self {
            theme: config.theme.clone(),
            terminal: WireTerminalConfig::from_config(&config.terminal),
            session: config.session.clone(),
            server: config.server.clone(),
            keys: WireKeysConfig::from_config(&config.keys),
            ui: WireUiConfig::from_config(&config.ui),
            advanced: config.advanced.clone(),
            experimental: WireExperimentalConfig::from_config(&config.experimental),
            remote: config.remote.clone(),
        }
    }

    pub(super) fn into_config(self) -> Result<Config, String> {
        Ok(Config {
            theme: self.theme,
            terminal: self.terminal.into_config(),
            session: self.session,
            server: self.server,
            keys: self.keys.into_config()?,
            ui: self.ui.into_config(),
            advanced: self.advanced,
            experimental: self.experimental.into_config(),
            remote: self.remote,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireTerminalConfig {
    default_shell: String,
    login_shell: bool,
    new_cwd: WireNewTerminalCwd,
}

impl WireTerminalConfig {
    fn from_config(config: &TerminalConfig) -> Self {
        Self {
            default_shell: config.default_shell.clone(),
            login_shell: config.login_shell,
            new_cwd: (&config.new_cwd).into(),
        }
    }

    fn into_config(self) -> TerminalConfig {
        TerminalConfig {
            default_shell: self.default_shell,
            login_shell: self.login_shell,
            new_cwd: self.new_cwd.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireNewTerminalCwd {
    Follow,
    Home,
    Current,
    Path(String),
}

impl From<&NewTerminalCwdConfig> for WireNewTerminalCwd {
    fn from(config: &NewTerminalCwdConfig) -> Self {
        match config {
            NewTerminalCwdConfig::Follow => Self::Follow,
            NewTerminalCwdConfig::Home => Self::Home,
            NewTerminalCwdConfig::Current => Self::Current,
            NewTerminalCwdConfig::Path(path) => Self::Path(path.clone()),
        }
    }
}

impl From<WireNewTerminalCwd> for NewTerminalCwdConfig {
    fn from(config: WireNewTerminalCwd) -> Self {
        match config {
            WireNewTerminalCwd::Follow => Self::Follow,
            WireNewTerminalCwd::Home => Self::Home,
            WireNewTerminalCwd::Current => Self::Current,
            WireNewTerminalCwd::Path(path) => Self::Path(path),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireExperimentalConfig {
    allow_nested: bool,
    pane_history: bool,
    reveal_hidden_cursor_for_cjk_ime: bool,
    cjk_ime_agents: Vec<super::ConfigAgent>,
    cjk_ime_cursor_shape: super::model::ImeCursorShape,
}

impl WireExperimentalConfig {
    fn from_config(config: &ExperimentalConfig) -> Self {
        Self {
            allow_nested: config.allow_nested,
            pane_history: config.pane_history,
            reveal_hidden_cursor_for_cjk_ime: config.reveal_hidden_cursor_for_cjk_ime,
            cjk_ime_agents: config.cjk_ime_agents.clone(),
            cjk_ime_cursor_shape: config.cjk_ime_cursor_shape,
        }
    }

    fn into_config(self) -> ExperimentalConfig {
        ExperimentalConfig {
            allow_nested: self.allow_nested,
            pane_history: self.pane_history,
            reveal_hidden_cursor_for_cjk_ime: self.reveal_hidden_cursor_for_cjk_ime,
            cjk_ime_agents: self.cjk_ime_agents,
            cjk_ime_cursor_shape: self.cjk_ime_cursor_shape,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireRightClickModifier {
    Off,
    Control,
    Alt,
    ControlAlt,
}

impl From<RightClickPassthroughModifierConfig> for WireRightClickModifier {
    fn from(config: RightClickPassthroughModifierConfig) -> Self {
        use crossterm::event::KeyModifiers;
        match config.modifiers() {
            None => Self::Off,
            Some(modifiers) if modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                Self::ControlAlt
            }
            Some(modifiers) if modifiers.contains(KeyModifiers::CONTROL) => Self::Control,
            Some(_) => Self::Alt,
        }
    }
}

impl From<WireRightClickModifier> for RightClickPassthroughModifierConfig {
    fn from(modifier: WireRightClickModifier) -> Self {
        use crossterm::event::KeyModifiers;
        let modifiers = match modifier {
            WireRightClickModifier::Off => None,
            WireRightClickModifier::Control => Some(KeyModifiers::CONTROL),
            WireRightClickModifier::Alt => Some(KeyModifiers::ALT),
            WireRightClickModifier::ControlAlt => Some(KeyModifiers::CONTROL | KeyModifiers::ALT),
        };
        Self::from_modifiers(modifiers)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireKeysConfig {
    prefix: String,
    bindings: Vec<WireBindingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireBindingConfig {
    One(String),
    Many(Vec<String>),
}

impl From<&BindingConfig> for WireBindingConfig {
    fn from(config: &BindingConfig) -> Self {
        match config {
            BindingConfig::One(value) => Self::One(value.clone()),
            BindingConfig::Many(values) => Self::Many(values.clone()),
        }
    }
}

impl From<WireBindingConfig> for BindingConfig {
    fn from(config: WireBindingConfig) -> Self {
        match config {
            WireBindingConfig::One(value) => Self::One(value),
            WireBindingConfig::Many(values) => Self::Many(values),
        }
    }
}

macro_rules! key_binding_fields {
    ($apply:ident) => {
        $apply! {
            help new_workspace rename_workspace close_workspace workspace_picker goto
            navigate_workspace_up navigate_workspace_down navigate_pane_left navigate_pane_down
            navigate_pane_up navigate_pane_right detach previous_workspace next_workspace
            previous_agent next_agent focus_agent new_tab rename_tab previous_tab next_tab
            move_tab_previous move_tab_next switch_tab switch_workspace close_tab rename_pane
            clear_pane copy_mode focus_pane_left focus_pane_down focus_pane_up focus_pane_right
            swap_pane_left swap_pane_down swap_pane_up swap_pane_right cycle_pane_next
            cycle_pane_previous last_pane split_vertical split_horizontal close_pane zoom
            resize_mode resize_pane_left resize_pane_down resize_pane_up resize_pane_right
            toggle_sidebar
        }
    };
}

macro_rules! count_key_binding_fields {
    ($($field:ident)*) => {
        [$(stringify!($field)),*].len()
    };
}

const KEY_BINDING_COUNT: usize = key_binding_fields!(count_key_binding_fields);

impl WireKeysConfig {
    fn from_config(keys: &KeysConfig) -> Self {
        macro_rules! add_bindings {
            ($($field:ident)*) => {
                vec![$(WireBindingConfig::from(&keys.$field)),*]
            };
        }
        let bindings = key_binding_fields!(add_bindings);
        Self {
            prefix: keys.prefix.clone(),
            bindings,
        }
    }

    fn into_config(self) -> Result<KeysConfig, String> {
        if self.bindings.len() != KEY_BINDING_COUNT {
            return Err(format!(
                "resolved config has {} keybindings; expected {KEY_BINDING_COUNT}",
                self.bindings.len()
            ));
        }
        let mut bindings = self.bindings.into_iter();
        macro_rules! take_bindings {
            ($($field:ident)*) => {
                KeysConfig {
                    prefix: self.prefix,
                    $(
                        $field: bindings
                            .next()
                            .ok_or("resolved config is missing a keybinding")?
                            .into(),
                    )*
                }
            };
        }
        Ok(key_binding_fields!(take_bindings))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireUiConfig {
    sidebar_width: u16,
    sidebar_min_width: u16,
    sidebar_max_width: u16,
    sidebar_start_collapsed: bool,
    sidebar_collapsed_mode: SidebarCollapsedModeConfig,
    mouse_capture: bool,
    copy_on_select: bool,
    host_cursor: HostCursorModeConfig,
    right_click_passthrough_modifier: WireRightClickModifier,
    redraw_on_focus_gained: bool,
    mouse_scroll_lines: Option<std::num::NonZeroUsize>,
    confirm_close: bool,
    prompt_new_tab_name: bool,
    prompt_new_workspace_name: bool,
    pane_borders: PaneBordersConfig,
    pane_outer_borders: bool,
    pane_scrollbars: bool,
    pane_gaps: bool,
    show_agent_labels_on_pane_borders: bool,
    hide_tab_bar_when_single_tab: bool,
    tab_bar_position: TabBarPositionConfig,
    tab_bar_right: Vec<WireTabBarRightEntry>,
    tab_bar_right_separator: String,
    window_title: String,
    agent_panel_sort: AgentPanelSortConfig,
    status_indicators: StatusIndicatorStyle,
    sidebar: WireSidebarConfig,
    accent: String,
}

impl WireUiConfig {
    fn from_config(ui: &UiConfig) -> Self {
        Self {
            sidebar_width: ui.sidebar_width,
            sidebar_min_width: ui.sidebar_min_width,
            sidebar_max_width: ui.sidebar_max_width,
            sidebar_start_collapsed: ui.sidebar_start_collapsed,
            sidebar_collapsed_mode: ui.sidebar_collapsed_mode,
            mouse_capture: ui.mouse_capture,
            copy_on_select: ui.copy_on_select,
            host_cursor: ui.host_cursor,
            right_click_passthrough_modifier: ui.right_click_passthrough_modifier.into(),
            redraw_on_focus_gained: ui.redraw_on_focus_gained,
            mouse_scroll_lines: ui.mouse_scroll_lines,
            confirm_close: ui.confirm_close,
            prompt_new_tab_name: ui.prompt_new_tab_name,
            prompt_new_workspace_name: ui.prompt_new_workspace_name,
            pane_borders: ui.pane_borders,
            pane_outer_borders: ui.pane_outer_borders,
            pane_scrollbars: ui.pane_scrollbars,
            pane_gaps: ui.pane_gaps,
            show_agent_labels_on_pane_borders: ui.show_agent_labels_on_pane_borders,
            hide_tab_bar_when_single_tab: ui.hide_tab_bar_when_single_tab,
            tab_bar_position: ui.tab_bar_position,
            tab_bar_right: ui.tab_bar_right.iter().map(Into::into).collect(),
            tab_bar_right_separator: ui.tab_bar_right_separator.clone(),
            window_title: ui.window_title.clone(),
            agent_panel_sort: ui.agent_panel_sort,
            status_indicators: ui.status_indicators,
            sidebar: WireSidebarConfig::from_config(&ui.sidebar),
            accent: ui.accent.clone(),
        }
    }

    fn into_config(self) -> UiConfig {
        UiConfig {
            sidebar_width: self.sidebar_width,
            sidebar_min_width: self.sidebar_min_width,
            sidebar_max_width: self.sidebar_max_width,
            sidebar_start_collapsed: self.sidebar_start_collapsed,
            sidebar_collapsed_mode: self.sidebar_collapsed_mode,
            mouse_capture: self.mouse_capture,
            copy_on_select: self.copy_on_select,
            host_cursor: self.host_cursor,
            right_click_passthrough_modifier: self.right_click_passthrough_modifier.into(),
            redraw_on_focus_gained: self.redraw_on_focus_gained,
            mouse_scroll_lines: self.mouse_scroll_lines,
            confirm_close: self.confirm_close,
            prompt_new_tab_name: self.prompt_new_tab_name,
            prompt_new_workspace_name: self.prompt_new_workspace_name,
            pane_borders: self.pane_borders,
            pane_outer_borders: self.pane_outer_borders,
            pane_scrollbars: self.pane_scrollbars,
            pane_gaps: self.pane_gaps,
            show_agent_labels_on_pane_borders: self.show_agent_labels_on_pane_borders,
            hide_tab_bar_when_single_tab: self.hide_tab_bar_when_single_tab,
            tab_bar_position: self.tab_bar_position,
            tab_bar_right: self.tab_bar_right.into_iter().map(Into::into).collect(),
            tab_bar_right_separator: self.tab_bar_right_separator,
            window_title: self.window_title,
            agent_panel_sort: self.agent_panel_sort,
            status_indicators: self.status_indicators,
            sidebar: self.sidebar.into_config(),
            accent: self.accent,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireTabBarRightEntry {
    Zoom,
    Hostname,
    Datetime {
        format: String,
    },
    Text {
        text: String,
    },
    Command {
        command: String,
        interval_seconds: u64,
        timeout_seconds: u64,
    },
}

impl From<&TabBarRightEntryConfig> for WireTabBarRightEntry {
    fn from(entry: &TabBarRightEntryConfig) -> Self {
        match entry {
            ConfigTabBarEntry::Zoom => Self::Zoom,
            ConfigTabBarEntry::Hostname => Self::Hostname,
            ConfigTabBarEntry::Datetime { format } => Self::Datetime {
                format: format.clone(),
            },
            ConfigTabBarEntry::Text { text } => Self::Text { text: text.clone() },
            ConfigTabBarEntry::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => Self::Command {
                command: command.clone(),
                interval_seconds: *interval_seconds,
                timeout_seconds: *timeout_seconds,
            },
        }
    }
}

impl From<WireTabBarRightEntry> for TabBarRightEntryConfig {
    fn from(entry: WireTabBarRightEntry) -> Self {
        match entry {
            WireTabBarRightEntry::Zoom => Self::Zoom,
            WireTabBarRightEntry::Hostname => Self::Hostname,
            WireTabBarRightEntry::Datetime { format } => Self::Datetime { format },
            WireTabBarRightEntry::Text { text } => Self::Text { text },
            WireTabBarRightEntry::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => Self::Command {
                command,
                interval_seconds,
                timeout_seconds,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireSidebarConfig {
    agents: WireAgentsSidebarConfig,
    spaces: WireSpacesSidebarConfig,
}

impl WireSidebarConfig {
    fn from_config(sidebar: &SidebarConfig) -> Self {
        Self {
            agents: WireAgentsSidebarConfig::from_config(&sidebar.agents),
            spaces: WireSpacesSidebarConfig::from_config(&sidebar.spaces),
        }
    }

    fn into_config(self) -> SidebarConfig {
        SidebarConfig {
            agents: self.agents.into_config(),
            spaces: self.spaces.into_config(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireAgentsSidebarConfig {
    rows: Vec<Vec<WireAgentSidebarToken>>,
    rows_by_agent: BTreeMap<String, Vec<Vec<WireAgentSidebarToken>>>,
    row_gap: u16,
}

impl WireAgentsSidebarConfig {
    fn from_config(config: &AgentsSidebarConfig) -> Self {
        Self {
            rows: agent_rows_to_wire(&config.rows),
            rows_by_agent: config
                .rows_by_agent
                .iter()
                .map(|(agent, rows)| (agent.clone(), agent_rows_to_wire(rows)))
                .collect(),
            row_gap: config.row_gap,
        }
    }

    fn into_config(self) -> AgentsSidebarConfig {
        AgentsSidebarConfig {
            rows: agent_rows_from_wire(self.rows),
            rows_by_agent: self
                .rows_by_agent
                .into_iter()
                .map(|(agent, rows)| (agent, agent_rows_from_wire(rows)))
                .collect(),
            row_gap: self.row_gap,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireSpacesSidebarConfig {
    rows: Vec<Vec<WireSpaceSidebarToken>>,
    row_gap: u16,
}

impl WireSpacesSidebarConfig {
    fn from_config(config: &SpacesSidebarConfig) -> Self {
        Self {
            rows: space_rows_to_wire(&config.rows),
            row_gap: config.row_gap,
        }
    }

    fn into_config(self) -> SpacesSidebarConfig {
        SpacesSidebarConfig {
            rows: space_rows_from_wire(self.rows),
            row_gap: self.row_gap,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireAgentSidebarToken {
    StateIcon,
    StateText,
    Machine,
    Workspace,
    Tab,
    Pane,
    Agent,
    TerminalTitle,
    TerminalTitleStripped,
    Custom(String),
    Styled {
        token: Box<Self>,
        style: WireSidebarTokenStyle,
        rules: Vec<WireSidebarTokenRule>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireSpaceSidebarToken {
    StateIcon,
    StateText,
    Workspace,
    Branch,
    GitStatus,
    Custom(String),
    Styled {
        token: Box<Self>,
        style: WireSidebarTokenStyle,
        rules: Vec<WireSidebarTokenRule>,
    },
}

fn agent_rows_to_wire(rows: &[Vec<AgentSidebarToken>]) -> Vec<Vec<WireAgentSidebarToken>> {
    rows.iter()
        .map(|row| row.iter().map(Into::into).collect())
        .collect()
}

fn agent_rows_from_wire(rows: Vec<Vec<WireAgentSidebarToken>>) -> Vec<Vec<AgentSidebarToken>> {
    rows.into_iter()
        .map(|row| row.into_iter().map(Into::into).collect())
        .collect()
}

fn space_rows_to_wire(rows: &[Vec<SpaceSidebarToken>]) -> Vec<Vec<WireSpaceSidebarToken>> {
    rows.iter()
        .map(|row| row.iter().map(Into::into).collect())
        .collect()
}

fn space_rows_from_wire(rows: Vec<Vec<WireSpaceSidebarToken>>) -> Vec<Vec<SpaceSidebarToken>> {
    rows.into_iter()
        .map(|row| row.into_iter().map(Into::into).collect())
        .collect()
}

impl From<&AgentSidebarToken> for WireAgentSidebarToken {
    fn from(token: &AgentSidebarToken) -> Self {
        match token {
            AgentSidebarToken::StateIcon => Self::StateIcon,
            AgentSidebarToken::StateText => Self::StateText,
            AgentSidebarToken::Machine => Self::Machine,
            AgentSidebarToken::Workspace => Self::Workspace,
            AgentSidebarToken::Tab => Self::Tab,
            AgentSidebarToken::Pane => Self::Pane,
            AgentSidebarToken::Agent => Self::Agent,
            AgentSidebarToken::TerminalTitle => Self::TerminalTitle,
            AgentSidebarToken::TerminalTitleStripped => Self::TerminalTitleStripped,
            AgentSidebarToken::Custom(name) => Self::Custom(name.clone()),
            AgentSidebarToken::Styled {
                token,
                style,
                rules,
            } => Self::Styled {
                token: Box::new(token.as_ref().into()),
                style: (*style).into(),
                rules: rules.iter().map(SidebarTokenRule::to_wire).collect(),
            },
        }
    }
}

impl From<WireAgentSidebarToken> for AgentSidebarToken {
    fn from(token: WireAgentSidebarToken) -> Self {
        match token {
            WireAgentSidebarToken::StateIcon => Self::StateIcon,
            WireAgentSidebarToken::StateText => Self::StateText,
            WireAgentSidebarToken::Machine => Self::Machine,
            WireAgentSidebarToken::Workspace => Self::Workspace,
            WireAgentSidebarToken::Tab => Self::Tab,
            WireAgentSidebarToken::Pane => Self::Pane,
            WireAgentSidebarToken::Agent => Self::Agent,
            WireAgentSidebarToken::TerminalTitle => Self::TerminalTitle,
            WireAgentSidebarToken::TerminalTitleStripped => Self::TerminalTitleStripped,
            WireAgentSidebarToken::Custom(name) => Self::Custom(name),
            WireAgentSidebarToken::Styled {
                token,
                style,
                rules,
            } => Self::Styled {
                token: Box::new((*token).into()),
                style: style.into(),
                rules: rules.into_iter().map(SidebarTokenRule::from_wire).collect(),
            },
        }
    }
}

impl From<&SpaceSidebarToken> for WireSpaceSidebarToken {
    fn from(token: &SpaceSidebarToken) -> Self {
        match token {
            SpaceSidebarToken::StateIcon => Self::StateIcon,
            SpaceSidebarToken::StateText => Self::StateText,
            SpaceSidebarToken::Workspace => Self::Workspace,
            SpaceSidebarToken::Branch => Self::Branch,
            SpaceSidebarToken::GitStatus => Self::GitStatus,
            SpaceSidebarToken::Custom(name) => Self::Custom(name.clone()),
            SpaceSidebarToken::Styled {
                token,
                style,
                rules,
            } => Self::Styled {
                token: Box::new(token.as_ref().into()),
                style: (*style).into(),
                rules: rules.iter().map(SidebarTokenRule::to_wire).collect(),
            },
        }
    }
}

impl From<WireSpaceSidebarToken> for SpaceSidebarToken {
    fn from(token: WireSpaceSidebarToken) -> Self {
        match token {
            WireSpaceSidebarToken::StateIcon => Self::StateIcon,
            WireSpaceSidebarToken::StateText => Self::StateText,
            WireSpaceSidebarToken::Workspace => Self::Workspace,
            WireSpaceSidebarToken::Branch => Self::Branch,
            WireSpaceSidebarToken::GitStatus => Self::GitStatus,
            WireSpaceSidebarToken::Custom(name) => Self::Custom(name),
            WireSpaceSidebarToken::Styled {
                token,
                style,
                rules,
            } => Self::Styled {
                token: Box::new((*token).into()),
                style: style.into(),
                rules: rules.into_iter().map(SidebarTokenRule::from_wire).collect(),
            },
        }
    }
}
