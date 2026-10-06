use super::*;

impl AgentOwnership {
    pub(super) fn recompute_effective_state(
        &mut self,
        previous_agent: Option<Agent>,
        previous_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        let effective = self.effective_row();
        let state = effective.state;
        let changed = previous_agent != effective.agent || previous_state != state;

        self.state = state;

        if !changed {
            return None;
        }

        Some(EffectiveStateChange {
            previous_state,
            state,
        })
    }
}

impl AgentOwnership {
    pub fn has_agent(&self) -> bool {
        self.effective_agent().is_some()
    }
}

impl AgentOwnership {
    pub fn state(&self) -> AgentState {
        self.state
    }
    pub fn detected_agent(&self) -> Option<Agent> {
        self.detected_agent
    }
    pub fn fallback_state(&self) -> AgentState {
        self.fallback_state
    }
    /// The arbitration row that decides the effective state, read from the
    /// same `effective_row()` evaluation that `recompute_effective_state`
    /// takes the state from. It is derived live rather than cached next to
    /// `state`: the table is pure and cheap, and a cached owner would go stale
    /// whenever a mutation changed an arbitration input without recomputing,
    /// leaving the detector pause and detect explain on an old answer.
    pub fn state_owner(&self) -> EffectiveStateSource {
        self.effective_row().source
    }
    pub fn last_agent_state_change_seq(&self) -> Option<shepr_agent::StateChangeSeq> {
        self.last_agent_state_change_seq
    }
    pub fn record_agent_state_change_seq(&mut self, seq: shepr_agent::StateChangeSeq) {
        self.last_agent_state_change_seq = Some(seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_equal_state_hook_records_the_winning_source() {
        let mut ownership = AgentOwnership::new();
        let now = Instant::now();
        ownership.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Idle,
            false,
            false,
            now,
        );
        assert_eq!(ownership.state_owner(), EffectiveStateSource::Screen);
        let session = shepr_agent::resume::AgentSessionRef::id("session").expect("session ref");
        let mutation = ownership
            .set_hook_authority_at("shepr:codex", AgentState::Idle, Some(session), None, now)
            .expect("partial-state hook report");
        assert_eq!(mutation.effective_state_change, None);
        assert_eq!(ownership.state_owner(), EffectiveStateSource::Hook);
        assert_eq!(ownership.state(), AgentState::Idle);
    }

    #[test]
    fn equal_state_authority_changes_still_move_the_detector_pause() {
        let mut ownership = AgentOwnership::new();
        let now = Instant::now();
        ownership.set_detected_state_with_screen_signals_at(
            Some(Agent::Omp),
            AgentState::Idle,
            false,
            false,
            now,
        );
        // Only a full-lifecycle source (Omp, not Codex) pauses detection, and
        // it owns the state only once a session anchors it.
        let session = shepr_agent::resume::AgentSessionRef::id("session").expect("session ref");
        ownership.set_persisted_agent_session(
            shepr_agent::resume::PersistedAgentSession::from_report("shepr:omp", session.clone())
                .expect("persisted session"),
        );
        let mutation = ownership
            .set_hook_authority_at("shepr:omp", AgentState::Idle, Some(session), None, now)
            .expect("full lifecycle report");
        assert_eq!(mutation.effective_state_change, None);
        assert_eq!(
            ownership.state_owner(),
            EffectiveStateSource::FullLifecycleHook
        );
        assert!(ownership.full_lifecycle_hook_authority_active());
        ownership.set_pane_process_exit_at(false, now);
        assert_eq!(ownership.state_owner(), EffectiveStateSource::ProcessExit);
        assert!(!ownership.full_lifecycle_hook_authority_active());
    }
}
