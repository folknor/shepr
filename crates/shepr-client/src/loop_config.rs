/// Client behavior resolved once from the launch config.
#[derive(Clone, Copy)]
pub(super) struct ClientSettings {
    host_cursor: shepr_config::HostCursorModeConfig,
    mouse_capture_active: bool,
    modify_other_keys_mode: Option<shepr_vt::ModifyOtherKeysLevel>,
    prefers_osc52_clipboard: bool,
}

impl ClientSettings {
    pub(super) fn resolve(
        config: &shepr_config::ValidatedClientConfig,
    ) -> Result<Self, shepr_core::env::EnvError> {
        let modify_other_keys_mode = shepr_termio::input::host_modify_other_keys_mode()?;
        Ok(Self::resolve_with_host_preferences(
            config,
            modify_other_keys_mode,
            shepr_platform::prefers_osc52_clipboard(),
        ))
    }

    fn resolve_with_host_preferences(
        config: &shepr_config::ValidatedClientConfig,
        modify_other_keys_mode: Option<shepr_vt::ModifyOtherKeysLevel>,
        prefers_osc52_clipboard: bool,
    ) -> Self {
        let ui = config.ui();
        Self {
            host_cursor: ui.host_cursor,
            mouse_capture_active: ui.mouse_capture,
            modify_other_keys_mode,
            prefers_osc52_clipboard,
        }
    }

    pub(super) fn host_cursor(&self) -> shepr_config::HostCursorModeConfig {
        self.host_cursor
    }

    pub(super) fn mouse_capture_active(&self) -> bool {
        self.mouse_capture_active
    }

    pub(super) fn modify_other_keys_mode(&self) -> Option<shepr_vt::ModifyOtherKeysLevel> {
        self.modify_other_keys_mode
    }

    pub(super) fn prefers_osc52_clipboard(&self) -> bool {
        self.prefers_osc52_clipboard
    }
}

pub(super) struct ClientLoopConfig {
    pub(super) settings: ClientSettings,
    pub(super) host_escape_disambiguation_active: bool,
    pub(super) initial_host_input: Vec<u8>,
    pub(super) paths: shepr_config::AppPaths,
    pub(super) local_mismatch_guidance: std::sync::Arc<str>,
}

#[cfg(test)]
impl ClientSettings {
    pub(super) fn from_config(config: &shepr_config::ValidatedClientConfig) -> Self {
        Self::resolve_with_host_preferences(config, None, false)
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
            Some(shepr_vt::ModifyOtherKeysLevel::All),
            true,
        );
        assert_eq!(
            settings.modify_other_keys_mode(),
            Some(shepr_vt::ModifyOtherKeysLevel::All)
        );
        assert!(settings.prefers_osc52_clipboard());

        let settings = ClientSettings::resolve_with_host_preferences(&config, None, false);
        assert_eq!(settings.modify_other_keys_mode(), None);
        assert!(!settings.prefers_osc52_clipboard());
    }
}
