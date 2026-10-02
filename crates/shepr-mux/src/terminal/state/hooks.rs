use super::*;

impl TerminalState {
    pub fn set_hook_report_at(
        &mut self,
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> Option<TerminalStateMutation> {
        self.transition_hook_event(HookEvent::Report {
            origin,
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
            && let Some(session) = authority.origin.session(session_ref.clone())
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
        source: &str,
        agent_label: &str,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.set_hook_report_at(
            ReportOrigin::parse(source, agent_label).ok()?,
            state,
            session_ref,
            seq,
            sample.into(),
        )
    }
}
