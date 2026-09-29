use super::*;

/// Client behavior resolved once from the launch config.
#[derive(Clone, Copy)]
pub(super) struct ClientSettings {
    mouse_scroll_lines: u16,
    redraw_on_focus_gained: bool,
    host_cursor: shepr_config::HostCursorModeConfig,
    pixel_geometry_fallback: bool,
    mouse_capture_active: bool,
    manage_ssh_config: bool,
    modify_other_keys_mode: Option<shepr_vt::ModifyOtherKeysLevel>,
    prefers_osc52_clipboard: bool,
}

impl ClientSettings {
    pub(super) fn resolve(
        config: &shepr_config::ValidatedConfig,
        launch_mode: &super::ClientLaunchMode,
    ) -> Result<Self, shepr_core::env::EnvError> {
        let modify_other_keys_mode = match launch_mode {
            super::ClientLaunchMode::Shell => shepr_termio::input::host_modify_other_keys_mode()?,
            super::ClientLaunchMode::Attach { .. } => None,
        };
        Ok(Self::resolve_with_host_preferences(
            config,
            launch_mode,
            modify_other_keys_mode,
            shepr_platform::prefers_osc52_clipboard(),
        ))
    }

    fn resolve_with_host_preferences(
        config: &shepr_config::ValidatedConfig,
        launch_mode: &super::ClientLaunchMode,
        modify_other_keys_mode: Option<shepr_vt::ModifyOtherKeysLevel>,
        prefers_osc52_clipboard: bool,
    ) -> Self {
        let ui = config.ui();
        let pixel_geometry_fallback = matches!(launch_mode, super::ClientLaunchMode::Shell);
        Self {
            mouse_scroll_lines: ui.mouse_scroll_lines.get(),
            redraw_on_focus_gained: ui.redraw_on_focus_gained,
            host_cursor: ui.host_cursor,
            pixel_geometry_fallback,
            mouse_capture_active: ui.mouse_capture,
            manage_ssh_config: config.remote().manage_ssh_config,
            modify_other_keys_mode,
            prefers_osc52_clipboard,
        }
    }

    #[cfg(test)]
    pub(super) fn from_config(config: &shepr_config::ValidatedConfig) -> Self {
        Self::resolve_with_host_preferences(config, &super::ClientLaunchMode::Shell, None, false)
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

    pub(super) fn pixel_geometry_fallback(&self) -> bool {
        self.pixel_geometry_fallback
    }

    pub(super) fn mouse_capture_active(&self) -> bool {
        self.mouse_capture_active
    }

    pub(super) fn manage_ssh_config(&self) -> bool {
        self.manage_ssh_config
    }

    pub(super) fn modify_other_keys_mode(&self) -> Option<shepr_vt::ModifyOtherKeysLevel> {
        self.modify_other_keys_mode
    }

    pub(super) fn prefers_osc52_clipboard(&self) -> bool {
        self.prefers_osc52_clipboard
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
        let shell = ClientSettings::resolve_with_host_preferences(
            &config,
            &super::super::ClientLaunchMode::Shell,
            Some(shepr_vt::ModifyOtherKeysLevel::All),
            true,
        );
        assert!(shell.pixel_geometry_fallback());
        assert_eq!(
            shell.modify_other_keys_mode(),
            Some(shepr_vt::ModifyOtherKeysLevel::All)
        );
        assert!(shell.prefers_osc52_clipboard());

        let attach = ClientSettings::resolve_with_host_preferences(
            &config,
            &super::super::ClientLaunchMode::Attach {
                terminal_id: shepr_protocol::TerminalId::alloc(),
                takeover: false,
                escape: super::super::AttachEscapeState::default(),
            },
            None,
            false,
        );
        assert!(!attach.pixel_geometry_fallback());
        assert_eq!(attach.modify_other_keys_mode(), None);
        assert!(!attach.prefers_osc52_clipboard());
    }
}
