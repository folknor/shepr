use super::*;

pub(super) fn merged_config_diagnostic(
    local: Option<&str>,
    endpoint: Option<&str>,
) -> Option<String> {
    match (local, endpoint) {
        (Some(local), Some(endpoint)) if local == endpoint => {
            Some(format!("client + endpoint: {local}"))
        }
        (Some(local), Some(endpoint)) => Some(format!("client: {local}\nendpoint: {endpoint}")),
        (Some(local), None) => Some(local.to_owned()),
        (None, Some(endpoint)) => Some(endpoint.to_owned()),
        (None, None) => None,
    }
}

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
        };
        if let Err(error) = preferences::store(path, &preferences) {
            self.set_endpoint_error(error);
            outcome.repaint = true;
        }
    }
}

impl ClientShellConfig {
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            sidebar_width: config.ui.sidebar_width,
            sidebar_min_width: config.ui.sidebar_min_width,
            sidebar_max_width: config.ui.sidebar_max_width,
            sidebar_start_collapsed: config.ui.sidebar_start_collapsed,
            sidebar_collapsed_mode: config.ui.sidebar_collapsed_mode,
            tab_bar_position: config.ui.tab_bar_position,
            hide_tab_bar_when_single_tab: config.ui.hide_tab_bar_when_single_tab,
            spaces: config.ui.sidebar.spaces.clone(),
            agents: config.ui.sidebar.agents.clone(),
            agent_panel_sort: config.ui.agent_panel_sort,
            status_indicators: config.ui.status_indicators,
            copy_on_select: config.ui.copy_on_select,
            palette: crate::app::palette_from_config(config),
            keybinds: config
                .live_keybinds_with_diagnostics()
                .map(|(keybinds, _diagnostics)| keybinds)
                .unwrap_or_else(|_diagnostics| LiveKeybindConfig {
                    prefix: config.prefix_key(),
                    keybinds: config.keybinds(),
                }),
            local_keys: config.keys.clone(),
            keybinding_source: ClientShellKeybindingSource::Local,
            prompt_new_tab_name: config.ui.prompt_new_tab_name,
            prompt_new_workspace_name: config.ui.prompt_new_workspace_name,
            confirm_close: config.ui.confirm_close,
            mouse_capture: config.ui.mouse_capture,
            mouse_scroll_lines: config.ui.mouse_scroll_lines(),
            right_click_passthrough_modifiers: config.ui.right_click_passthrough_modifiers(),
            redraw_on_focus_gained: config.ui.redraw_on_focus_gained,
            preferences_path: None,
            preferences: preferences::ClientChromePreferences::default(),
            startup_config_diagnostic: None,
        }
    }

    pub(crate) fn with_startup_config_diagnostic(mut self, diagnostic: Option<String>) -> Self {
        self.startup_config_diagnostic = diagnostic;
        self
    }

    pub(crate) fn with_keybinding_source(mut self, source: ClientShellKeybindingSource) -> Self {
        self.keybinding_source = source;
        self
    }

    pub(crate) fn uses_endpoint_keybindings(&self) -> bool {
        self.keybinding_source == ClientShellKeybindingSource::Endpoint
    }

    pub(crate) fn with_local_endpoint(self, socket_path: &std::path::Path) -> Self {
        self.with_preferences_path(preferences::path_for_local_endpoint(socket_path))
    }

    pub(super) fn with_preferences_path(mut self, path: std::path::PathBuf) -> Self {
        self.preferences = preferences::load(&path).unwrap_or_default();
        self.preferences_path = Some(path);
        self
    }

    pub(super) fn apply_snapshot_keybindings(
        &mut self,
        profile: Option<&str>,
    ) -> Result<(), String> {
        let keybinds = match self.keybinding_source {
            ClientShellKeybindingSource::Endpoint => crate::config::keybindings_from_profile_toml(
                profile.ok_or("endpoint did not publish its keybindings")?,
            )?,
            ClientShellKeybindingSource::RemoteLocal => return Ok(()),
            ClientShellKeybindingSource::Local => {
                let config = crate::config::Config {
                    keys: self.local_keys.clone(),
                    ..Default::default()
                };
                config
                    .live_keybinds_with_diagnostics()
                    .map(|(keybinds, _diagnostics)| keybinds)
                    .map_err(|diagnostics| diagnostics.join("; "))?
            }
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
        let sidebar_width = if sidebar_collapsed {
            match self.sidebar_collapsed_mode {
                SidebarCollapsedModeConfig::Compact => 4,
                SidebarCollapsedModeConfig::Hidden => 0,
            }
        } else {
            let (min, max) = crate::config::validated_sidebar_bounds(
                self.sidebar_min_width,
                self.sidebar_max_width,
            )
            .unwrap_or((18, 36));
            sidebar_width.clamp(min, max)
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
        let (min_width, max_width) =
            crate::config::validated_sidebar_bounds(self.sidebar_min_width, self.sidebar_max_width)
                .unwrap_or((18, 36));
        let sidebar_width = self
            .preferences
            .sidebar_width
            .unwrap_or(self.sidebar_width)
            .clamp(min_width, max_width);
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

    #[test]
    fn initial_surface_size_uses_persisted_endpoint_chrome() {
        let path = std::env::temp_dir().join(format!(
            "shepr-initial-shell-preferences-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
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
}
