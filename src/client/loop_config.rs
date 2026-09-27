use super::*;

/// Client behavior resolved once from the launch config.
#[derive(Clone, Copy)]
pub(super) struct ClientSettings {
    pub(super) mouse_scroll_lines: u16,
    pub(super) redraw_on_focus_gained: bool,
    pub(super) host_cursor: shepr_config::HostCursorModeConfig,
    pub(super) pixel_geometry_enabled: bool,
    pub(super) pixel_geometry_fallback: bool,
    pub(super) mouse_capture_active: bool,
    pub(super) manage_ssh_config: bool,
}

impl ClientSettings {
    pub(super) fn from_config(config: &shepr_config::ValidatedConfig) -> Self {
        Self {
            mouse_scroll_lines: u16::try_from(config.ui.mouse_scroll_lines().max(1))
                .unwrap_or(u16::MAX),
            redraw_on_focus_gained: config.ui.redraw_on_focus_gained,
            host_cursor: config.ui.host_cursor,
            pixel_geometry_enabled: false,
            pixel_geometry_fallback: false,
            mouse_capture_active: config.ui.mouse_capture,
            manage_ssh_config: config.remote.manage_ssh_config,
        }
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
