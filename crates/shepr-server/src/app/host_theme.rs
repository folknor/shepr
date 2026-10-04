use super::App;

impl App {
    /// Installs the host's light or dark appearance in every pane. Nothing
    /// drawn reads it: it reaches panes only as their answer to a colour
    /// scheme query and the mode 2031 notification, both written to the
    /// pane's PTY by the runtime here. It is not saved with the session.
    /// So unlike the theme it owes no view change and marks nothing dirty; a
    /// child that repaints in response does so through its own output.
    pub(crate) fn set_host_terminal_appearance_state(
        &mut self,
        report: super::HostAppearanceReport,
    ) -> bool {
        if self.state.host_terminal_appearance == report {
            return false;
        }
        self.state.host_terminal_appearance = report;
        for runtime in self.terminal_runtimes.values() {
            runtime.apply_host_terminal_appearance(report.appearance());
        }
        true
    }

    /// Installs the host theme as every pane's default colours. Pane cells
    /// are drawn with them and the session saves them, so a change marks the
    /// session dirty and returns true; the caller owes the view change, which
    /// moves the view epoch and sends every client through a full pass.
    /// Nothing else reads the theme (projections and client chrome do not).
    #[must_use]
    pub(crate) fn set_host_terminal_theme(
        &mut self,
        theme: shepr_term::host::TerminalTheme,
    ) -> bool {
        if theme.is_empty() {
            return false;
        }
        self.resume_schedule.note_live_theme();
        if theme == self.state.host_terminal_theme {
            return false;
        }
        self.state.host_terminal_theme = theme;
        self.state.mark_session_dirty();
        for runtime in self.terminal_runtimes.values() {
            runtime.apply_host_terminal_theme(theme);
        }
        true
    }
}
