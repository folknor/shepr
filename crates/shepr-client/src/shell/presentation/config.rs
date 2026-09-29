use super::*;

impl ClientShellState {
    pub(super) fn persist_chrome_preferences(&mut self, outcome: &mut ClientShellInput) {
        let Some(path) = self.config.preferences_path.as_deref() else {
            return;
        };
        let preferences = preferences::ClientChromePreferences {
            sidebar_width: self.sidebar_width_manual.then_some(self.sidebar_width),
            sidebar_section_split: self
                .sidebar_section_split_manual
                .then_some(self.sidebar_section_split),
            sidebar_collapsed: self
                .sidebar_collapsed_manual
                .then_some(self.sidebar_collapsed),
            agent_panel_sort: self
                .agent_panel_sort_manual
                .then_some(self.config.agent_panel_sort),
            configured: preferences::ConfiguredChrome::default(),
        }
        // A value config.toml sets is only a session change: storing it would
        // bring it back if the key were later removed from the config.
        .without_configured(self.config.preferences.configured);
        if let Err(error) = preferences::store(path, &preferences) {
            self.set_endpoint_error(error, self.now);
            outcome.repaint = true;
        }
    }
}

impl ClientShellConfig {
    #[cfg(test)]
    pub fn from_config(config: &Config) -> Self {
        use shepr_test_fixtures::ValidatedConfigFixture as _;
        let validated = shepr_config::ValidatedConfig::test_from_config(config.clone(), None);
        Self::from_config_with_configured(
            validated.ui(),
            preferences::ConfiguredChrome::default(),
            validated.palette().clone(),
            validated.live_keybinds(),
        )
    }

    pub fn from_validated_config(config: &shepr_config::ValidatedConfig) -> Self {
        Self::from_config_with_configured(
            config.ui(),
            preferences::ConfiguredChrome::from_validated_config(config),
            config.palette().clone(),
            config.live_keybinds(),
        )
    }

    fn from_config_with_configured(
        config: &shepr_config::ValidatedUiConfig,
        configured: preferences::ConfiguredChrome,
        palette: shepr_config::theme::Palette,
        keybinds: LiveKeybindConfig,
    ) -> Self {
        Self {
            sidebar_width: config.sidebar_width(),
            sidebar_bounds: config.sidebar_bounds(),
            sidebar_start_collapsed: config.sidebar_start_collapsed,
            sidebar_collapsed_mode: config.sidebar_collapsed_mode,
            tab_bar_position: config.tab_bar_position,
            hide_tab_bar_when_single_tab: config.hide_tab_bar_when_single_tab,
            spaces: config.sidebar.spaces.clone(),
            agents: config.sidebar.agents.clone(),
            agent_panel_sort: config.agent_panel_sort,
            status_indicators: config.status_indicators,
            copy_on_select: config.copy_on_select,
            palette,
            // One validation pass; the launch already rejected invalid bindings.
            keybinds,
            keybinding_source: ClientShellKeybindingSource::RemoteLocal,
            prompt_new_tab_name: config.prompt_new_tab_name,
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

    pub(crate) fn with_keybinding_source(mut self, source: ClientShellKeybindingSource) -> Self {
        self.keybinding_source = source;
        self
    }

    pub(crate) fn uses_endpoint_keybindings(&self) -> bool {
        self.keybinding_source == ClientShellKeybindingSource::Endpoint
    }

    pub(crate) fn with_local_endpoint(
        self,
        state_dir: &std::path::Path,
        socket_path: &std::path::Path,
    ) -> std::io::Result<Self> {
        let path = preferences::path_for_local_endpoint(state_dir, socket_path);
        preferences::probe_writable(&path)?;
        Ok(self.with_preferences_path(path))
    }

    pub(super) fn with_preferences_path(mut self, path: std::path::PathBuf) -> Self {
        let configured = self.preferences.configured;
        self.preferences = preferences::load(&path)
            .unwrap_or_default()
            .without_configured(configured);
        self.preferences_path = Some(path);
        self
    }

    pub(super) fn apply_snapshot_config(
        &mut self,
        config: &shepr_config::ValidatedConfig,
    ) -> Result<(), String> {
        let keybinds = match self.keybinding_source {
            ClientShellKeybindingSource::Endpoint => config.live_keybinds(),
            ClientShellKeybindingSource::RemoteLocal => return Ok(()),
        };
        self.keybinds = keybinds;
        Ok(())
    }

    pub(super) fn layout(
        &self,
        cols: u16,
        rows: u16,
        sidebar_collapsed: bool,
        tab_count: usize,
        sidebar_width: u16,
    ) -> ClientShellLayout {
        // Expanded widths already come from the validated config or an input
        // path that clamps user preferences and drag positions on entry.
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
        let show_tab_bar = rows > 1 && !(self.hide_tab_bar_when_single_tab && tab_count == 1);
        let tab_height = u16::from(show_tab_bar);
        let (tab_bar, pane_surface) = match self.tab_bar_position {
            TabBarPositionConfig::Top => (
                Rect::new(main.x, 0, main.width, tab_height),
                Rect::new(
                    main.x,
                    tab_height,
                    main.width,
                    rows.saturating_sub(tab_height),
                ),
            ),
            TabBarPositionConfig::Bottom => (
                Rect::new(
                    main.x,
                    rows.saturating_sub(tab_height),
                    main.width,
                    tab_height,
                ),
                Rect::new(main.x, 0, main.width, rows.saturating_sub(tab_height)),
            ),
        };

        ClientShellLayout {
            sidebar: Rect::new(0, 0, sidebar_width, rows),
            tab_bar,
            pane_surface,
        }
    }

    pub(crate) fn initial_surface_size(&self, cols: u16, rows: u16) -> ClientSurfaceSize {
        let sidebar_collapsed = self
            .preferences
            .sidebar_collapsed
            .unwrap_or(self.sidebar_start_collapsed);
        let sidebar_width = match self.preferences.sidebar_width {
            Some(width) => self.sidebar_bounds.clamp_width(width),
            None => self.sidebar_width,
        };
        let surface = self
            .layout(cols, rows, sidebar_collapsed, 0, sidebar_width)
            .pane_surface;
        ClientSurfaceSize {
            cols: surface.width.max(1),
            rows: surface.height.max(1),
        }
        .clamped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::*;

    #[test]
    fn snapshot_config_applies_endpoint_keybindings_from_the_validated_value() {
        let local = shepr_config::ValidatedConfig::test_default();
        let mut endpoint = ClientShellConfig::from_validated_config(&local)
            .with_keybinding_source(ClientShellKeybindingSource::Endpoint);
        let remote_source = "[keys]\nprefix = \"ctrl+a\"\n";
        let mut remote_raw = shepr_config::Config::default();
        remote_raw.keys.prefix = "ctrl+a".to_owned();
        let remote =
            shepr_config::ValidatedConfig::test_from_config(remote_raw, Some(remote_source));

        endpoint
            .apply_snapshot_config(&remote)
            .expect("validated endpoint keybindings apply");

        assert_eq!(
            endpoint.keybinds.prefix,
            (
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::CONTROL,
            )
        );
        assert_eq!(
            endpoint.keybinds.keybinds.new_tab.label().as_deref(),
            Some("prefix+c")
        );
    }

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
        let config =
            ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone());
        let initial = config.initial_surface_size(100, 30);
        let state = ClientShellState::new(config);
        assert_eq!(initial, state.surface_size(100, 30));
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

        let mut values = Config::default();
        values.ui.sidebar_width = 24;
        let config = shepr_config::ValidatedConfig::test_from_config(
            values,
            Some("[ui]\nsidebar_width = 24\nagent_panel_sort = \"spaces\"\n"),
        );
        let shell_config =
            ClientShellConfig::from_validated_config(&config).with_preferences_path(path.clone());
        let mut state = ClientShellState::new(shell_config);

        // Set keys win; the unset one keeps the remembered toggle.
        assert_eq!(state.sidebar_width, 24);
        assert!(!state.sidebar_width_manual);
        assert_eq!(
            state.config.agent_panel_sort,
            shepr_config::AgentPanelSortConfig::Spaces
        );
        assert!(state.sidebar_collapsed);

        // A manual change still applies for the session but is not stored
        // for a value the config owns.
        state.sidebar_width = 30;
        state.sidebar_width_manual = true;
        state.config.agent_panel_sort = shepr_config::AgentPanelSortConfig::Priority;
        state.agent_panel_sort_manual = true;
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
            &shepr_config::ValidatedConfig::test_default(),
        )
        .with_local_endpoint(&state_dir, std::path::Path::new("/run/shepr/client.sock"));

        let error = result.err().expect("startup probe refuses the state path");
        assert!(error.to_string().contains("client shell state directory"));
    }
}
