use ratatui::layout::Rect;
use shepr_config::LiveKeybindConfig;
use shepr_config::SidebarCollapsedModeConfig;
use shepr_config::SpacesSidebarConfig;
use shepr_config::theme::Palette;

use shepr_protocol::ClientSurfaceSize;

use crate::shell::sidebar::preferences;

pub(crate) struct ClientShellConfig {
    pub(in crate::shell) sidebar_width: shepr_config::SidebarWidth,
    pub(in crate::shell) sidebar_bounds: shepr_config::SidebarBounds,
    pub(in crate::shell) sidebar_start_collapsed: bool,
    sidebar_collapsed_mode: SidebarCollapsedModeConfig,
    pub(in crate::shell) spaces: SpacesSidebarConfig,
    pub(in crate::shell) agents: shepr_config::AgentsSidebarConfig,
    /// The `ui.agent_panel_sort` setting (or its default) as launched. It only
    /// seeds `ClientShellState::agent_panel_sort_chrome`, which holds the live
    /// sort (a remembered or clicked toggle) that every reader goes through;
    /// nothing writes this field after launch.
    pub(in crate::shell) agent_panel_sort: shepr_config::AgentPanelSortConfig,
    pub(in crate::shell) status_indicators: shepr_config::StatusIndicatorStyle,
    pub(in crate::shell) copy_on_select: bool,
    pub(in crate::shell) palette: Palette,
    pub(in crate::shell) keybinds: LiveKeybindConfig,
    pub(in crate::shell) prompt_new_workspace_name: bool,
    pub(in crate::shell) confirm_close: bool,
    pub(in crate::shell) mouse_capture: bool,
    pub(in crate::shell) mouse_scroll_lines: u16,
    pub(in crate::shell) right_click_passthrough_modifiers: Option<crossterm::event::KeyModifiers>,
    pub(in crate::shell) redraw_on_focus_gained: bool,
    pub(in crate::shell) preferences_path: Option<std::path::PathBuf>,
    pub(in crate::shell) preferences: preferences::ClientChromePreferences,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) struct ClientShellLayout {
    pub(in crate::shell) sidebar: Rect,
    pub(in crate::shell) pane_surface: Rect,
}

impl ClientShellConfig {
    pub(crate) fn from_validated_config(config: &shepr_config::ValidatedClientConfig) -> Self {
        Self::from_config_with_configured(
            config.ui(),
            preferences::ConfiguredChrome::from_validated_config(config),
            config.palette().clone(),
            config.live_keybinds().clone(),
        )
    }

    fn from_config_with_configured(
        config: &shepr_config::ValidatedClientUiConfig,
        configured: preferences::ConfiguredChrome,
        palette: shepr_config::theme::Palette,
        keybinds: LiveKeybindConfig,
    ) -> Self {
        Self {
            sidebar_width: config.sidebar_width(),
            sidebar_bounds: config.sidebar_bounds(),
            sidebar_start_collapsed: *config.sidebar_start_collapsed.value(),
            sidebar_collapsed_mode: config.sidebar_collapsed_mode,
            spaces: config.sidebar.spaces.clone(),
            agents: config.sidebar.agents.clone(),
            agent_panel_sort: *config.agent_panel_sort.value(),
            status_indicators: config.status_indicators,
            copy_on_select: config.copy_on_select,
            palette,
            // One validation pass; the launch already rejected invalid bindings.
            keybinds,
            prompt_new_workspace_name: config.prompt_new_workspace_name,
            confirm_close: config.confirm_close,
            mouse_capture: config.mouse_capture,
            mouse_scroll_lines: config.mouse_scroll_lines.get(),
            right_click_passthrough_modifiers: config.right_click_passthrough_modifiers,
            redraw_on_focus_gained: config.redraw_on_focus_gained,
            preferences_path: None,
            preferences: preferences::ClientChromePreferences::default()
                .without_configured(configured),
        }
    }

    pub(crate) fn with_local_endpoint(
        self,
        state_dir: &std::path::Path,
        socket_path: &std::path::Path,
    ) -> Result<Self, preferences::PreferencesProbeError> {
        let path = preferences::path_for_local_endpoint(state_dir, socket_path);
        preferences::probe_writable(&path)?;
        Ok(self.with_preferences_path(path))
    }

    pub(in crate::shell) fn with_preferences_path(mut self, path: std::path::PathBuf) -> Self {
        let configured = self.preferences.configured;
        self.preferences = preferences::load(&path)
            .unwrap_or_default()
            .without_configured(configured);
        self.preferences_path = Some(path);
        self
    }

    pub(in crate::shell) fn layout(
        &self,
        cols: u16,
        rows: u16,
        sidebar_collapsed: bool,
        sidebar_width: u16,
    ) -> ClientShellLayout {
        // Expanded widths come from `ChromeLayout`, which clamps remembered
        // and dragged widths on entry.
        let sidebar_width = if sidebar_collapsed {
            match self.sidebar_collapsed_mode {
                SidebarCollapsedModeConfig::Compact => 4,
                SidebarCollapsedModeConfig::Hidden => 0,
            }
        } else {
            sidebar_width
        }
        .min(cols.saturating_sub(1));
        let main = Rect::new(sidebar_width, 0, cols.saturating_sub(sidebar_width), rows);

        ClientShellLayout {
            sidebar: Rect::new(0, 0, sidebar_width, rows),
            pane_surface: main,
        }
    }

    pub(crate) fn initial_surface_size(&self, cols: u16, rows: u16) -> ClientSurfaceSize {
        let chrome = self.initial_chrome();
        let surface = self
            .layout(
                cols,
                rows,
                chrome.collapsed.value(),
                chrome.width.value().value(),
            )
            .pane_surface;
        ClientSurfaceSize {
            cols: surface.width.max(1),
            rows: surface.height.max(1),
        }
        .clamped()
    }
}

#[cfg(test)]
use shepr_config::ClientConfig;

#[cfg(test)]
impl ClientShellConfig {
    pub(in crate::shell) fn from_config(config: &ClientConfig) -> Self {
        use shepr_test_fixtures::ValidatedClientConfigFixture as _;
        let validated = shepr_config::ValidatedClientConfig::test_from_config(config.clone(), None);
        Self::from_config_with_configured(
            validated.ui(),
            preferences::ConfiguredChrome::from_validated_config(&validated),
            validated.palette().clone(),
            validated.live_keybinds().clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::ClientShellConfig;
    use super::preferences;
    use crate::shell::state::{ClientShellInput, ClientShellState};
    use shepr_config::ClientConfig;

    use shepr_test_fixtures::*;

    #[test]
    fn initial_surface_size_uses_persisted_endpoint_chrome() {
        let scratch = shepr_test_support::ScratchDir::new("shell-prefs");
        let path = scratch.join("preferences.json");
        preferences::store(
            &path,
            &preferences::ClientChromePreferences {
                sidebar_width: Some(31),
                sidebar_collapsed: Some(true),
                ..preferences::ClientChromePreferences::default()
            },
        )
        .expect("persist endpoint chrome");
        let config = ClientShellConfig::from_config(&ClientConfig::default())
            .with_preferences_path(path.clone());
        let initial_chrome = config.initial_chrome();
        let initial = config.initial_surface_size(100, 30);
        let state = ClientShellState::new(config);
        assert_eq!(initial, state.surface_size(100, 30));
        assert_eq!(state.chrome.width(), initial_chrome.width.value().value());
        assert_eq!(state.chrome.collapsed(), initial_chrome.collapsed.value());
        assert_eq!(state.chrome.split(), initial_chrome.split.value());
        assert_eq!(
            state.chrome.width_origin(),
            crate::shell::sidebar::chrome::ChromeOrigin::Remembered
        );
        assert_eq!(
            state.chrome.collapsed_origin(),
            crate::shell::sidebar::chrome::ChromeOrigin::Remembered
        );
        std::fs::remove_file(path).expect("remove endpoint chrome");
    }

    #[test]
    fn configured_ui_keys_win_over_remembered_chrome() {
        let scratch = shepr_test_support::ScratchDir::new("shell-prefs");
        let path = scratch.join("preferences.json");
        preferences::store(
            &path,
            &preferences::ClientChromePreferences {
                sidebar_width: Some(31),
                sidebar_collapsed: Some(true),
                agent_panel_sort: Some(shepr_config::AgentPanelSortConfig::Priority),
                ..preferences::ClientChromePreferences::default()
            },
        )
        .expect("persist endpoint chrome");

        let mut values = ClientConfig::default();
        values.ui.sidebar_width = Some(24);
        values.ui.agent_panel_sort = Some(shepr_config::AgentPanelSortConfig::Spaces);
        let config = shepr_config::ValidatedClientConfig::test_from_config(
            values,
            Some("[ui]\nsidebar_width = 24\nagent_panel_sort = \"spaces\"\n"),
        );
        let shell_config =
            ClientShellConfig::from_validated_config(&config).with_preferences_path(path.clone());
        let mut state = ClientShellState::new(shell_config);

        // Set keys win; the unset one keeps the remembered toggle.
        assert_eq!(state.chrome.width(), 24);
        assert_eq!(
            state.chrome.width_origin(),
            crate::shell::sidebar::chrome::ChromeOrigin::Configured
        );
        assert!(state.chrome.preferences().sidebar_width.is_none());
        assert_eq!(
            state.agent_panel_sort_chrome.origin(),
            crate::shell::sidebar::chrome::ChromeOrigin::Configured
        );
        assert_eq!(
            state.agent_panel_sort_chrome.value(),
            shepr_config::AgentPanelSortConfig::Spaces
        );
        assert!(state.chrome.collapsed());
        assert_eq!(
            state.chrome.collapsed_origin(),
            crate::shell::sidebar::chrome::ChromeOrigin::Remembered
        );

        // A manual change still applies for the session but is not stored
        // for a value the config owns.
        state.chrome.set_width(30);
        assert_eq!(
            state.chrome.width_origin(),
            crate::shell::sidebar::chrome::ChromeOrigin::Manual
        );
        state.set_agent_panel_sort(shepr_config::AgentPanelSortConfig::Priority);
        assert_eq!(
            state.agent_panel_sort_chrome.origin(),
            crate::shell::sidebar::chrome::ChromeOrigin::Manual
        );
        state.persist_chrome_preferences(&mut ClientShellInput::default());
        let stored = preferences::load(&path).expect("stored chrome");
        assert_eq!(stored.sidebar_width, None);
        assert_eq!(stored.agent_panel_sort, None);
        assert_eq!(stored.sidebar_collapsed, Some(true));
        std::fs::remove_file(path).expect("remove endpoint chrome");
    }

    #[test]
    fn local_endpoint_refuses_unwritable_preferences_directory_at_startup() {
        let scratch = shepr_test_support::ScratchDir::new("shell-prefs-startup-probe");
        let state_dir = scratch.path().join("state");
        std::fs::create_dir_all(&state_dir).expect("create state directory");
        std::fs::write(state_dir.join("client-shell"), b"not a directory")
            .expect("block preferences directory");

        let result = ClientShellConfig::from_validated_config(
            &shepr_config::ValidatedClientConfig::test_default(),
        )
        .with_local_endpoint(&state_dir, std::path::Path::new("/run/shepr/client.sock"));

        let error = result.err().expect("startup probe refuses the state path");
        assert!(error.to_string().contains("client shell state directory"));
    }
}
