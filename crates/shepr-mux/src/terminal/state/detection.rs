use super::*;

impl TerminalState {
    pub fn set_detected_agent_process_at(
        &mut self,
        agent: Agent,
        now: Instant,
    ) -> TerminalStateMutation {
        self.set_detected_state_with_screen_signals_at(
            Some(agent),
            AgentState::Unknown,
            false,
            false,
            now,
        )
    }

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
        self.pending_agent_resume_plan = Some(plan);
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
        self.pending_agent_resume_plan = None;
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

    /// A detector observation. A process exit releases the agent at once; the
    /// identity it removes stays available to a checkpoint for a pane death
    /// right after it (see `CheckpointCandidate`).
    pub fn set_detected_state_with_screen_signals_at(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> TerminalStateMutation {
        self.transition_hook_event(HookEvent::Detection {
            agent,
            fallback_state,
            visible_blocker,
            process_exited,
            now,
        })
        .unwrap_or_default()
    }

    /// A pane exit ends hook authority, but an interrupted pane must retain
    /// its resume identity for the checkpoint taken before layout removal:
    /// the one it holds, or one a detector release removed within the grace
    /// before `ended_at`, the time the pane's ending was recorded. Detector
    /// releases are only applied while the pane child is live; once it exits,
    /// this transition decides with the exit's reason.
    pub fn set_pane_process_exit_at(
        &mut self,
        exit_reason: shepr_platform::ChildExitReason,
        ended_at: Instant,
    ) -> TerminalStateMutation {
        self.transition_hook_event(HookEvent::PaneExited {
            exit_reason,
            now: ended_at,
        })
        .unwrap_or_default()
    }
}
