use super::*;

impl TerminalState {
    pub fn set_hook_report_at(
        &mut self,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> Option<TerminalStateMutation> {
        self.transition_hook_event(HookEvent::Report {
            source,
            agent_label,
            state,
            session_ref,
            seq,
            sample,
        })
    }

    pub fn current_session_identity_for_persistence(
        &self,
    ) -> Option<shepr_agent::agent::resume::PersistedAgentSession> {
        if let Some(authority) = self.hook_authority.as_ref()
            && let Some(session_ref) = authority.session_ref.as_ref()
            && let Some(session) = shepr_agent::agent::resume::PersistedAgentSession::from_report(
                &authority.source,
                &authority.agent_label,
                session_ref.clone(),
            )
        {
            return Some(session);
        }
        self.persisted_agent_session.clone()
    }
}

impl TerminalState {
    /// Convenience seam for fixtures, taking the source as a string. The event
    /// reducer uses the typed report entry point.
    pub fn set_hook_authority_at(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.set_hook_report_at(
            source.into(),
            agent_label,
            state,
            session_ref,
            seq,
            sample.into(),
        )
    }
}
