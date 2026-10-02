use super::*;

impl AgentOwnership {
    pub fn set_detected_agent_process_at(
        &mut self,
        agent: Agent,
        now: Instant,
    ) -> AgentOwnershipMutation {
        self.set_detected_state_with_screen_signals_at(
            Some(agent),
            AgentState::Unknown,
            false,
            false,
            now,
        )
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
    ) -> AgentOwnershipMutation {
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
    ) -> AgentOwnershipMutation {
        self.transition_hook_event(HookEvent::PaneExited {
            exit_reason,
            now: ended_at,
        })
        .unwrap_or_default()
    }
}
