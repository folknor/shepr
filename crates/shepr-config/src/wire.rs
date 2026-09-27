use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    ActionKeybinds, AgentPanelSortConfig, BindingConfig, HostCursorModeConfig, IndexedKeybind,
    Keybinds, NewTerminalCwdConfig, PaneBordersConfig, RightClickPassthroughModifierConfig,
    SidebarCollapsedModeConfig, SidebarTokenRule, StatusIndicatorStyle, TabBarPositionConfig,
    TabBarRightEntryConfig, ThemeConfig,
    keybinds::{BindingTrigger, KeyCombo, KeybindValidation, NavigateKeybinds, ResolvedBinding},
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

const KEY_BINDING_COUNT: usize = 51;

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

/// A terminal color represented without packing a tag into a scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum PaletteColor {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl PaletteColor {
    fn from_ratatui(color: ratatui::style::Color) -> Self {
        match color {
            ratatui::style::Color::Reset => Self::Reset,
            ratatui::style::Color::Black => Self::Black,
            ratatui::style::Color::Red => Self::Red,
            ratatui::style::Color::Green => Self::Green,
            ratatui::style::Color::Yellow => Self::Yellow,
            ratatui::style::Color::Blue => Self::Blue,
            ratatui::style::Color::Magenta => Self::Magenta,
            ratatui::style::Color::Cyan => Self::Cyan,
            ratatui::style::Color::Gray => Self::Gray,
            ratatui::style::Color::DarkGray => Self::DarkGray,
            ratatui::style::Color::LightRed => Self::LightRed,
            ratatui::style::Color::LightGreen => Self::LightGreen,
            ratatui::style::Color::LightYellow => Self::LightYellow,
            ratatui::style::Color::LightBlue => Self::LightBlue,
            ratatui::style::Color::LightMagenta => Self::LightMagenta,
            ratatui::style::Color::LightCyan => Self::LightCyan,
            ratatui::style::Color::White => Self::White,
            ratatui::style::Color::Indexed(index) => Self::Indexed(index),
            ratatui::style::Color::Rgb(red, green, blue) => Self::Rgb(red, green, blue),
        }
    }

    fn to_ratatui(self) -> ratatui::style::Color {
        match self {
            Self::Reset => ratatui::style::Color::Reset,
            Self::Black => ratatui::style::Color::Black,
            Self::Red => ratatui::style::Color::Red,
            Self::Green => ratatui::style::Color::Green,
            Self::Yellow => ratatui::style::Color::Yellow,
            Self::Blue => ratatui::style::Color::Blue,
            Self::Magenta => ratatui::style::Color::Magenta,
            Self::Cyan => ratatui::style::Color::Cyan,
            Self::Gray => ratatui::style::Color::Gray,
            Self::DarkGray => ratatui::style::Color::DarkGray,
            Self::LightRed => ratatui::style::Color::LightRed,
            Self::LightGreen => ratatui::style::Color::LightGreen,
            Self::LightYellow => ratatui::style::Color::LightYellow,
            Self::LightBlue => ratatui::style::Color::LightBlue,
            Self::LightMagenta => ratatui::style::Color::LightMagenta,
            Self::LightCyan => ratatui::style::Color::LightCyan,
            Self::White => ratatui::style::Color::White,
            Self::Indexed(index) => ratatui::style::Color::Indexed(index),
            Self::Rgb(red, green, blue) => ratatui::style::Color::Rgb(red, green, blue),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WirePalette {
    accent: PaletteColor,
    panel_bg: PaletteColor,
    sidebar_bg: PaletteColor,
    active_row_bg: PaletteColor,
    selection_bg: PaletteColor,
    surface0: PaletteColor,
    surface1: PaletteColor,
    surface_dim: PaletteColor,
    overlay0: PaletteColor,
    overlay1: PaletteColor,
    text: PaletteColor,
    subtext0: PaletteColor,
    mauve: PaletteColor,
    green: PaletteColor,
    yellow: PaletteColor,
    red: PaletteColor,
    blue: PaletteColor,
    teal: PaletteColor,
    peach: PaletteColor,
}

impl From<&crate::theme::Palette> for WirePalette {
    fn from(palette: &crate::theme::Palette) -> Self {
        Self {
            accent: PaletteColor::from_ratatui(palette.accent),
            panel_bg: PaletteColor::from_ratatui(palette.panel_bg),
            sidebar_bg: PaletteColor::from_ratatui(palette.sidebar_bg),
            active_row_bg: PaletteColor::from_ratatui(palette.active_row_bg),
            selection_bg: PaletteColor::from_ratatui(palette.selection_bg),
            surface0: PaletteColor::from_ratatui(palette.surface0),
            surface1: PaletteColor::from_ratatui(palette.surface1),
            surface_dim: PaletteColor::from_ratatui(palette.surface_dim),
            overlay0: PaletteColor::from_ratatui(palette.overlay0),
            overlay1: PaletteColor::from_ratatui(palette.overlay1),
            text: PaletteColor::from_ratatui(palette.text),
            subtext0: PaletteColor::from_ratatui(palette.subtext0),
            mauve: PaletteColor::from_ratatui(palette.mauve),
            green: PaletteColor::from_ratatui(palette.green),
            yellow: PaletteColor::from_ratatui(palette.yellow),
            red: PaletteColor::from_ratatui(palette.red),
            blue: PaletteColor::from_ratatui(palette.blue),
            teal: PaletteColor::from_ratatui(palette.teal),
            peach: PaletteColor::from_ratatui(palette.peach),
        }
    }
}

impl From<WirePalette> for crate::theme::Palette {
    fn from(palette: WirePalette) -> Self {
        Self {
            accent: palette.accent.to_ratatui(),
            panel_bg: palette.panel_bg.to_ratatui(),
            sidebar_bg: palette.sidebar_bg.to_ratatui(),
            active_row_bg: palette.active_row_bg.to_ratatui(),
            selection_bg: palette.selection_bg.to_ratatui(),
            surface0: palette.surface0.to_ratatui(),
            surface1: palette.surface1.to_ratatui(),
            surface_dim: palette.surface_dim.to_ratatui(),
            overlay0: palette.overlay0.to_ratatui(),
            overlay1: palette.overlay1.to_ratatui(),
            text: palette.text.to_ratatui(),
            subtext0: palette.subtext0.to_ratatui(),
            mauve: palette.mauve.to_ratatui(),
            green: palette.green.to_ratatui(),
            yellow: palette.yellow.to_ratatui(),
            red: palette.red.to_ratatui(),
            blue: palette.blue.to_ratatui(),
            teal: palette.teal.to_ratatui(),
            peach: palette.peach.to_ratatui(),
        }
    }
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
pub(super) struct WireKeybindCache {
    prefix: WireKeyCombo,
    keybinds: WireKeybinds,
}

impl WireKeybindCache {
    pub(super) fn from_validation(validation: &KeybindValidation) -> Result<Self, String> {
        Ok(Self {
            prefix: WireKeyCombo::from_combo(validation.prefix)?,
            keybinds: WireKeybinds::from_keybinds(&validation.keybinds)?,
        })
    }

    pub(super) fn into_validation(self) -> Result<KeybindValidation, String> {
        Ok(KeybindValidation {
            prefix_diag: None,
            prefix: self.prefix.into_combo()?,
            keybind_diags: Vec::new(),
            keybinds: self.keybinds.into_keybinds()?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireKeybinds {
    groups: Vec<WireKeybindGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireKeybindGroup {
    Action(Vec<WireResolvedBinding>),
    Indexed(Vec<WireIndexedKeybind>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireResolvedBinding {
    trigger: WireBindingTrigger,
    label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireIndexedKeybind {
    trigger: WireBindingTrigger,
    label: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireBindingTrigger {
    Direct(WireKeyCombo),
    Prefix(WireKeyCombo),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct WireKeyCombo {
    code: WireKeyCode,
    modifiers: u8,
}

impl WireKeyCombo {
    fn from_combo((code, modifiers): KeyCombo) -> Result<Self, String> {
        Ok(Self {
            code: code.try_into()?,
            modifiers: modifiers.bits(),
        })
    }

    fn into_combo(self) -> Result<KeyCombo, String> {
        let modifiers =
            crossterm::event::KeyModifiers::from_bits(self.modifiers).ok_or_else(|| {
                format!(
                    "resolved keybinding has invalid modifier bits {}",
                    self.modifiers
                )
            })?;
        Ok((self.code.into(), modifiers))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireKeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Function(u8),
    Character(char),
    Null,
    Escape,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    KeypadBegin,
}

impl TryFrom<crossterm::event::KeyCode> for WireKeyCode {
    type Error = String;

    fn try_from(code: crossterm::event::KeyCode) -> Result<Self, Self::Error> {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Backspace => Ok(Self::Backspace),
            KeyCode::Enter => Ok(Self::Enter),
            KeyCode::Left => Ok(Self::Left),
            KeyCode::Right => Ok(Self::Right),
            KeyCode::Up => Ok(Self::Up),
            KeyCode::Down => Ok(Self::Down),
            KeyCode::Home => Ok(Self::Home),
            KeyCode::End => Ok(Self::End),
            KeyCode::PageUp => Ok(Self::PageUp),
            KeyCode::PageDown => Ok(Self::PageDown),
            KeyCode::Tab => Ok(Self::Tab),
            KeyCode::BackTab => Ok(Self::BackTab),
            KeyCode::Delete => Ok(Self::Delete),
            KeyCode::Insert => Ok(Self::Insert),
            KeyCode::F(number) => Ok(Self::Function(number)),
            KeyCode::Char(character) => Ok(Self::Character(character)),
            KeyCode::Null => Ok(Self::Null),
            KeyCode::Esc => Ok(Self::Escape),
            KeyCode::CapsLock => Ok(Self::CapsLock),
            KeyCode::ScrollLock => Ok(Self::ScrollLock),
            KeyCode::NumLock => Ok(Self::NumLock),
            KeyCode::PrintScreen => Ok(Self::PrintScreen),
            KeyCode::Pause => Ok(Self::Pause),
            KeyCode::Menu => Ok(Self::Menu),
            KeyCode::KeypadBegin => Ok(Self::KeypadBegin),
            KeyCode::Media(_) | KeyCode::Modifier(_) => {
                Err("resolved keybinding contains an unsupported terminal key".to_owned())
            }
        }
    }
}

impl From<WireKeyCode> for crossterm::event::KeyCode {
    fn from(code: WireKeyCode) -> Self {
        match code {
            WireKeyCode::Backspace => Self::Backspace,
            WireKeyCode::Enter => Self::Enter,
            WireKeyCode::Left => Self::Left,
            WireKeyCode::Right => Self::Right,
            WireKeyCode::Up => Self::Up,
            WireKeyCode::Down => Self::Down,
            WireKeyCode::Home => Self::Home,
            WireKeyCode::End => Self::End,
            WireKeyCode::PageUp => Self::PageUp,
            WireKeyCode::PageDown => Self::PageDown,
            WireKeyCode::Tab => Self::Tab,
            WireKeyCode::BackTab => Self::BackTab,
            WireKeyCode::Delete => Self::Delete,
            WireKeyCode::Insert => Self::Insert,
            WireKeyCode::Function(number) => Self::F(number),
            WireKeyCode::Character(character) => Self::Char(character),
            WireKeyCode::Null => Self::Null,
            WireKeyCode::Escape => Self::Esc,
            WireKeyCode::CapsLock => Self::CapsLock,
            WireKeyCode::ScrollLock => Self::ScrollLock,
            WireKeyCode::NumLock => Self::NumLock,
            WireKeyCode::PrintScreen => Self::PrintScreen,
            WireKeyCode::Pause => Self::Pause,
            WireKeyCode::Menu => Self::Menu,
            WireKeyCode::KeypadBegin => Self::KeypadBegin,
        }
    }
}

impl TryFrom<BindingTrigger> for WireBindingTrigger {
    type Error = String;

    fn try_from(trigger: BindingTrigger) -> Result<Self, Self::Error> {
        Ok(match trigger {
            BindingTrigger::Direct(combo) => Self::Direct(WireKeyCombo::from_combo(combo)?),
            BindingTrigger::Prefix(combo) => Self::Prefix(WireKeyCombo::from_combo(combo)?),
        })
    }
}

impl TryFrom<WireBindingTrigger> for BindingTrigger {
    type Error = String;

    fn try_from(trigger: WireBindingTrigger) -> Result<Self, Self::Error> {
        Ok(match trigger {
            WireBindingTrigger::Direct(combo) => Self::Direct(combo.into_combo()?),
            WireBindingTrigger::Prefix(combo) => Self::Prefix(combo.into_combo()?),
        })
    }
}

impl TryFrom<&ResolvedBinding> for WireResolvedBinding {
    type Error = String;

    fn try_from(binding: &ResolvedBinding) -> Result<Self, Self::Error> {
        Ok(Self {
            trigger: binding.trigger.try_into()?,
            label: binding.label.clone(),
        })
    }
}

impl TryFrom<WireResolvedBinding> for ResolvedBinding {
    type Error = String;

    fn try_from(binding: WireResolvedBinding) -> Result<Self, Self::Error> {
        Ok(Self {
            trigger: binding.trigger.try_into()?,
            label: binding.label,
        })
    }
}

impl TryFrom<&IndexedKeybind> for WireIndexedKeybind {
    type Error = String;

    fn try_from(binding: &IndexedKeybind) -> Result<Self, Self::Error> {
        Ok(Self {
            trigger: binding.trigger.try_into()?,
            label: binding.label.clone(),
        })
    }
}

impl TryFrom<WireIndexedKeybind> for IndexedKeybind {
    type Error = String;

    fn try_from(binding: WireIndexedKeybind) -> Result<Self, Self::Error> {
        Ok(Self {
            trigger: binding.trigger.try_into()?,
            label: binding.label,
        })
    }
}

fn action_group(bindings: &ActionKeybinds) -> Result<WireKeybindGroup, String> {
    Ok(WireKeybindGroup::Action(
        bindings
            .bindings
            .iter()
            .map(WireResolvedBinding::try_from)
            .collect::<Result<_, _>>()?,
    ))
}

fn indexed_group(bindings: &[IndexedKeybind]) -> Result<WireKeybindGroup, String> {
    Ok(WireKeybindGroup::Indexed(
        bindings
            .iter()
            .map(WireIndexedKeybind::try_from)
            .collect::<Result<_, _>>()?,
    ))
}

fn take_action_group(
    groups: &mut std::vec::IntoIter<WireKeybindGroup>,
) -> Result<ActionKeybinds, String> {
    match groups.next() {
        Some(WireKeybindGroup::Action(bindings)) => Ok(ActionKeybinds {
            bindings: bindings
                .into_iter()
                .map(ResolvedBinding::try_from)
                .collect::<Result<_, _>>()?,
        }),
        Some(WireKeybindGroup::Indexed(_)) => {
            Err("resolved config keybinding group has the wrong shape".to_owned())
        }
        None => Err("resolved config is missing a keybinding group".to_owned()),
    }
}

fn take_indexed_group(
    groups: &mut std::vec::IntoIter<WireKeybindGroup>,
) -> Result<Vec<IndexedKeybind>, String> {
    match groups.next() {
        Some(WireKeybindGroup::Indexed(bindings)) => {
            bindings.into_iter().map(IndexedKeybind::try_from).collect()
        }
        Some(WireKeybindGroup::Action(_)) => {
            Err("resolved config keybinding group has the wrong shape".to_owned())
        }
        None => Err("resolved config is missing a keybinding group".to_owned()),
    }
}

impl WireKeybinds {
    fn from_keybinds(keybinds: &Keybinds) -> Result<Self, String> {
        Ok(Self {
            groups: vec![
                action_group(&keybinds.navigate.workspace_up)?,
                action_group(&keybinds.navigate.workspace_down)?,
                action_group(&keybinds.navigate.pane_left)?,
                action_group(&keybinds.navigate.pane_down)?,
                action_group(&keybinds.navigate.pane_up)?,
                action_group(&keybinds.navigate.pane_right)?,
                action_group(&keybinds.help)?,
                action_group(&keybinds.new_workspace)?,
                action_group(&keybinds.rename_workspace)?,
                action_group(&keybinds.close_workspace)?,
                action_group(&keybinds.workspace_picker)?,
                action_group(&keybinds.goto)?,
                action_group(&keybinds.detach)?,
                action_group(&keybinds.previous_workspace)?,
                action_group(&keybinds.next_workspace)?,
                action_group(&keybinds.previous_agent)?,
                action_group(&keybinds.next_agent)?,
                indexed_group(&keybinds.focus_agent)?,
                action_group(&keybinds.new_tab)?,
                action_group(&keybinds.rename_tab)?,
                action_group(&keybinds.previous_tab)?,
                action_group(&keybinds.next_tab)?,
                action_group(&keybinds.move_tab_previous)?,
                action_group(&keybinds.move_tab_next)?,
                indexed_group(&keybinds.switch_tab)?,
                indexed_group(&keybinds.switch_workspace)?,
                action_group(&keybinds.close_tab)?,
                action_group(&keybinds.rename_pane)?,
                action_group(&keybinds.clear_pane)?,
                action_group(&keybinds.copy_mode)?,
                action_group(&keybinds.focus_pane_left)?,
                action_group(&keybinds.focus_pane_down)?,
                action_group(&keybinds.focus_pane_up)?,
                action_group(&keybinds.focus_pane_right)?,
                action_group(&keybinds.swap_pane_left)?,
                action_group(&keybinds.swap_pane_down)?,
                action_group(&keybinds.swap_pane_up)?,
                action_group(&keybinds.swap_pane_right)?,
                action_group(&keybinds.cycle_pane_next)?,
                action_group(&keybinds.cycle_pane_previous)?,
                action_group(&keybinds.last_pane)?,
                action_group(&keybinds.split_vertical)?,
                action_group(&keybinds.split_horizontal)?,
                action_group(&keybinds.close_pane)?,
                action_group(&keybinds.zoom)?,
                action_group(&keybinds.resize_mode)?,
                action_group(&keybinds.resize_pane_left)?,
                action_group(&keybinds.resize_pane_down)?,
                action_group(&keybinds.resize_pane_up)?,
                action_group(&keybinds.resize_pane_right)?,
                action_group(&keybinds.toggle_sidebar)?,
            ],
        })
    }

    fn into_keybinds(self) -> Result<Keybinds, String> {
        if self.groups.len() != KEY_BINDING_COUNT {
            return Err(format!(
                "resolved config has {} keybinding groups; expected {KEY_BINDING_COUNT}",
                self.groups.len()
            ));
        }
        let mut groups = self.groups.into_iter();
        let keybinds = Keybinds {
            navigate: NavigateKeybinds {
                workspace_up: take_action_group(&mut groups)?,
                workspace_down: take_action_group(&mut groups)?,
                pane_left: take_action_group(&mut groups)?,
                pane_down: take_action_group(&mut groups)?,
                pane_up: take_action_group(&mut groups)?,
                pane_right: take_action_group(&mut groups)?,
            },
            help: take_action_group(&mut groups)?,
            new_workspace: take_action_group(&mut groups)?,
            rename_workspace: take_action_group(&mut groups)?,
            close_workspace: take_action_group(&mut groups)?,
            workspace_picker: take_action_group(&mut groups)?,
            goto: take_action_group(&mut groups)?,
            detach: take_action_group(&mut groups)?,
            previous_workspace: take_action_group(&mut groups)?,
            next_workspace: take_action_group(&mut groups)?,
            previous_agent: take_action_group(&mut groups)?,
            next_agent: take_action_group(&mut groups)?,
            focus_agent: take_indexed_group(&mut groups)?,
            new_tab: take_action_group(&mut groups)?,
            rename_tab: take_action_group(&mut groups)?,
            previous_tab: take_action_group(&mut groups)?,
            next_tab: take_action_group(&mut groups)?,
            move_tab_previous: take_action_group(&mut groups)?,
            move_tab_next: take_action_group(&mut groups)?,
            switch_tab: take_indexed_group(&mut groups)?,
            switch_workspace: take_indexed_group(&mut groups)?,
            close_tab: take_action_group(&mut groups)?,
            rename_pane: take_action_group(&mut groups)?,
            clear_pane: take_action_group(&mut groups)?,
            copy_mode: take_action_group(&mut groups)?,
            focus_pane_left: take_action_group(&mut groups)?,
            focus_pane_down: take_action_group(&mut groups)?,
            focus_pane_up: take_action_group(&mut groups)?,
            focus_pane_right: take_action_group(&mut groups)?,
            swap_pane_left: take_action_group(&mut groups)?,
            swap_pane_down: take_action_group(&mut groups)?,
            swap_pane_up: take_action_group(&mut groups)?,
            swap_pane_right: take_action_group(&mut groups)?,
            cycle_pane_next: take_action_group(&mut groups)?,
            cycle_pane_previous: take_action_group(&mut groups)?,
            last_pane: take_action_group(&mut groups)?,
            split_vertical: take_action_group(&mut groups)?,
            split_horizontal: take_action_group(&mut groups)?,
            close_pane: take_action_group(&mut groups)?,
            zoom: take_action_group(&mut groups)?,
            resize_mode: take_action_group(&mut groups)?,
            resize_pane_left: take_action_group(&mut groups)?,
            resize_pane_down: take_action_group(&mut groups)?,
            resize_pane_up: take_action_group(&mut groups)?,
            resize_pane_right: take_action_group(&mut groups)?,
            toggle_sidebar: take_action_group(&mut groups)?,
        };
        if groups.next().is_some() {
            return Err("resolved config has extra keybinding groups".to_owned());
        }
        Ok(keybinds)
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
