use super::*;

/// Client behavior resolved once from the launch config.
#[derive(Clone, Copy)]
pub(super) struct ClientSettings {
    mouse_scroll_lines: u16,
    redraw_on_focus_gained: bool,
    host_cursor: shepr_config::HostCursorModeConfig,
    pixel_geometry_enabled: bool,
    pixel_geometry_fallback: bool,
    mouse_capture_active: bool,
    manage_ssh_config: bool,
}

impl ClientSettings {
    pub(super) fn resolve(
        config: &shepr_config::ValidatedConfig,
        launch_mode: &super::ClientLaunchMode,
    ) -> Self {
        let ui = config.ui();
        let (pixel_geometry_enabled, pixel_geometry_fallback) = match launch_mode {
            super::ClientLaunchMode::Shell => (true, true),
            super::ClientLaunchMode::Attach { .. } => (true, false),
        };
        Self {
            mouse_scroll_lines: ui.mouse_scroll_lines.get(),
            redraw_on_focus_gained: ui.redraw_on_focus_gained,
            host_cursor: ui.host_cursor,
            pixel_geometry_enabled,
            pixel_geometry_fallback,
            mouse_capture_active: ui.mouse_capture,
            manage_ssh_config: config.remote().manage_ssh_config,
        }
    }

    #[cfg(test)]
    pub(super) fn from_config(config: &shepr_config::ValidatedConfig) -> Self {
        Self::resolve(config, &super::ClientLaunchMode::Shell)
    }

    pub(super) fn mouse_scroll_lines(&self) -> u16 {
        self.mouse_scroll_lines
    }

    pub(super) fn redraw_on_focus_gained(&self) -> bool {
        self.redraw_on_focus_gained
    }

    pub(super) fn host_cursor(&self) -> shepr_config::HostCursorModeConfig {
        self.host_cursor
    }

    pub(super) fn pixel_geometry_enabled(&self) -> bool {
        self.pixel_geometry_enabled
    }

    pub(super) fn pixel_geometry_fallback(&self) -> bool {
        self.pixel_geometry_fallback
    }

    pub(super) fn mouse_capture_active(&self) -> bool {
        self.mouse_capture_active
    }

    pub(super) fn manage_ssh_config(&self) -> bool {
        self.manage_ssh_config
    }
}

pub(super) struct ClientLoopConfig {
    pub(super) role: super::handshake::ClientProcessRole,
    pub(super) settings: ClientSettings,
    pub(super) host_escape_disambiguation_active: bool,
    pub(super) initial_host_input: Vec<u8>,
    pub(super) paths: shepr_config::AppPaths,
    pub(super) local_socket_path: std::path::PathBuf,
    pub(super) shell_config: Option<shell::ClientShellConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::ValidatedConfigFixture as _;

    #[test]
    fn settings_resolve_geometry_for_the_launch_mode() {
        let config = shepr_config::ValidatedConfig::test_default();
        let shell = ClientSettings::resolve(&config, &super::super::ClientLaunchMode::Shell);
        assert!(shell.pixel_geometry_enabled());
        assert!(shell.pixel_geometry_fallback());

        let attach = ClientSettings::resolve(
            &config,
            &super::super::ClientLaunchMode::Attach {
                terminal_id: shepr_protocol::TerminalId::test_new("term_test"),
                takeover: false,
                escape: super::super::AttachEscapeState::default(),
            },
        );
        assert!(attach.pixel_geometry_enabled());
        assert!(!attach.pixel_geometry_fallback());
    }
}
