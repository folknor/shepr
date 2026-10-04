/// Client behavior resolved once from the launch config.
#[derive(Clone, Copy)]
pub(super) struct ClientSettings {
    host_cursor: shepr_config::HostCursorModeConfig,
    mouse_capture_active: bool,
    modify_other_keys_mode: Option<shepr_term::ModifyOtherKeysLevel>,
    clipboard_route: shepr_platform::ClipboardRoute,
}

impl ClientSettings {
    pub(super) fn resolve(
        config: &shepr_config::ValidatedClientConfig,
    ) -> Result<Self, shepr_core::env::EnvError> {
        let modify_other_keys_mode = shepr_termio::host_term::modes::host_modify_other_keys_mode()?;
        Ok(Self::resolve_with_host_preferences(
            config,
            modify_other_keys_mode,
            shepr_platform::ClipboardRoute::from_env(),
        ))
    }

    fn resolve_with_host_preferences(
        config: &shepr_config::ValidatedClientConfig,
        modify_other_keys_mode: Option<shepr_term::ModifyOtherKeysLevel>,
        clipboard_route: shepr_platform::ClipboardRoute,
    ) -> Self {
        let ui = config.ui();
        Self {
            host_cursor: ui.host_cursor,
            mouse_capture_active: ui.mouse_capture,
            modify_other_keys_mode,
            clipboard_route,
        }
    }

    pub(super) fn host_cursor(&self) -> shepr_config::HostCursorModeConfig {
        self.host_cursor
    }

    pub(super) fn mouse_capture_active(&self) -> bool {
        self.mouse_capture_active
    }

    pub(super) fn modify_other_keys_mode(&self) -> Option<shepr_term::ModifyOtherKeysLevel> {
        self.modify_other_keys_mode
    }

    pub(super) fn clipboard_route(&self) -> shepr_platform::ClipboardRoute {
        self.clipboard_route
    }
}

#[cfg(test)]
impl ClientSettings {
    pub(super) fn from_config(config: &shepr_config::ValidatedClientConfig) -> Self {
        Self::resolve_with_host_preferences(config, None, shepr_platform::ClipboardRoute::Osc52)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::ValidatedClientConfigFixture as _;

    #[test]
    fn settings_carry_the_host_preferences() {
        let config = shepr_config::ValidatedClientConfig::test_default();
        let settings = ClientSettings::resolve_with_host_preferences(
            &config,
            Some(shepr_term::ModifyOtherKeysLevel::All),
            shepr_platform::ClipboardRoute::Osc52,
        );
        assert_eq!(
            settings.modify_other_keys_mode(),
            Some(shepr_term::ModifyOtherKeysLevel::All)
        );
        assert_eq!(
            settings.clipboard_route(),
            shepr_platform::ClipboardRoute::Osc52
        );

        let helpers =
            shepr_platform::ClipboardRoute::Helpers(shepr_platform::ClipboardSession::none());
        let settings = ClientSettings::resolve_with_host_preferences(&config, None, helpers);
        assert_eq!(settings.modify_other_keys_mode(), None);
        assert_eq!(settings.clipboard_route(), helpers);
    }
}
