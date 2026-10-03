use super::*;

impl TerminalState {
    pub fn terminal_title_stripped(&self) -> Option<String> {
        self.terminal_title
            .as_deref()
            .and_then(crate::terminal::stripped_terminal_title)
    }

    pub fn set_terminal_title(&mut self, title: Option<String>) -> TerminalTitleChange {
        if self.terminal_title == title {
            return TerminalTitleChange::default();
        }
        let previous_stripped = self.terminal_title_stripped();
        self.terminal_title = title;
        TerminalTitleChange {
            raw_changed: true,
            stripped_changed: previous_stripped != self.terminal_title_stripped(),
        }
    }

    pub fn with_pending_agent_resume_plan(
        mut self,
        plan: shepr_agent::agent::resume::AgentResumePlan,
    ) -> Self {
        self.agent_resume = AgentResumeState::Planned(plan);
        self
    }

    /// The deferred resume can never run (its directory is gone, its shell
    /// will not start), so the pane stays without any process. Drops the
    /// plan, records why, and withdraws the detection `restored_terminal`
    /// seeded for the resumed agent: no runtime ever existed here, so that
    /// detection is only the seed, and with no detector to ever report the
    /// pane empty it would show an idle agent on a dead pane indefinitely.
    /// The saved session stays, as for any unavailable restored pane, so a
    /// later save writes it back.
    // Failure presentation and persistence are handled by the resume caller.
    // Removing its synthetic detected identity is not a new agent activity
    // transition: Unknown and Idle have the same sidebar presentation.
    pub fn abandon_agent_resume(&mut self, error: super::RestoreFailure, now: Instant) {
        self.agent_resume = AgentResumeState::None;
        self.restore_error = Some(error);
        if self.detected_agent.is_some() {
            let _ = self.set_detected_state_with_screen_signals_at(
                None,
                AgentState::Unknown,
                false,
                false,
                now,
            );
        }
    }
}
