use super::App;

impl App {
    /// Installs the host's light or dark appearance in every pane. Nothing
    /// drawn reads it: it reaches panes only as their answer to a colour
    /// scheme query and the mode 2031 notification, both written to the
    /// pane's PTY by the runtime here. It is not saved with the session.
    /// So unlike the theme it requests no render and marks nothing dirty; a
    /// child that repaints in response does so through its own output.
    pub(crate) fn set_host_terminal_appearance_state(
        &mut self,
        appearance: Option<shepr_term::host::HostAppearance>,
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

    /// Installs the host theme as every pane's default colours. Pane cells
    /// are drawn with them and the session saves them, so a change marks the
    /// session dirty and requests a render; the render request moves the
    /// view epoch, sending every client through a full pass. Nothing else
    /// reads the theme (projections and client chrome do not), so callers
    /// need no invalidation of their own, whatever caused the change.
    pub(crate) fn set_host_terminal_theme(
        &mut self,
        theme: shepr_term::host::TerminalTheme,
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
        self.invalidate_shared_view(false);
        true
    }
}
