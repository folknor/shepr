use super::*;

impl TerminalState {
    pub fn set_persisted_agent_session(
        &mut self,
        session: shepr_agent::agent::resume::PersistedAgentSession,
    ) {
        let _ = self.transition_hook_event(HookEvent::RestoreSession(session));
    }

    pub fn set_agent_session_ref_at(
        &mut self,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.set_agent_session_ref_for_typed_start_source_at(
            source,
            agent_label,
            session_ref,
            seq,
            None,
            sample,
        )
    }

    pub fn set_agent_session_ref_for_typed_start_source_at(
        &mut self,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<shepr_agent::agent::resume::AgentSessionStartSource>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.transition_hook_event(HookEvent::Start {
            source,
            agent_label,
            session_ref,
            seq,
            session_start_source,
            sample: sample.into(),
        })
    }
}
