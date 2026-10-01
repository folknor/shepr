use super::App;

impl App {
    pub(crate) fn set_host_terminal_appearance_state(
        &mut self,
        appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        explicit: bool,
    ) -> bool {
        if self.state.host_terminal_appearance == appearance
            && self.state.host_terminal_appearance_explicit == explicit
        {
            return false;
        }
        self.state.host_terminal_appearance = appearance;
        self.state.host_terminal_appearance_explicit = explicit;
        for runtime in self.terminal_runtimes.values() {
            runtime.apply_host_terminal_appearance(appearance);
        }
        true
    }

    pub(crate) fn set_host_terminal_theme(
        &mut self,
        theme: shepr_termio::host_term::theme::TerminalTheme,
    ) -> bool {
        if theme.is_empty() {
            return false;
        }
        self.live_host_theme_reported = true;
        if theme == self.state.host_terminal_theme {
            return false;
        }
        self.state.host_terminal_theme = theme;
        self.state.mark_session_dirty();
        for runtime in self.terminal_runtimes.values() {
            runtime.apply_host_terminal_theme(theme);
        }
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }
}
