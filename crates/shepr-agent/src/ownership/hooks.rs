use super::*;

impl AgentOwnership {
    pub fn set_hook_report_at(
        &mut self,
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> Option<AgentOwnershipMutation> {
        self.transition_hook_event(HookEvent::Report {
            origin,
            state,
            session_ref,
            seq,
            sample,
        })
    }

    pub fn report_hook_outcome_at(
        &mut self,
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> HookOutcome {
        let outcome = self.transition_report(origin, state, session_ref, seq, sample);
        self.check_hook_invariants();
        outcome
    }

    pub fn current_session_identity_for_persistence(
        &self,
    ) -> Option<crate::agent::resume::PersistedAgentSession> {
        if let Some(authority) = self.hook_authority.as_ref()
            && let Some(session_ref) = authority.session_ref.as_ref()
            && let Some(session) = authority.origin.session(session_ref.clone())
        {
            return Some(session);
        }
        self.persisted_agent_session.clone()
    }
}

#[cfg(test)]
impl AgentOwnership {
    /// Convenience seam for fixtures, taking the source as a string. The event
    /// reducer uses the typed report entry point.
    pub fn set_hook_authority_at(
        &mut self,
        source: &str,
        agent_label: &str,
        state: AgentState,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<AgentOwnershipMutation> {
        self.set_hook_report_at(
            ReportOrigin::parse(source, agent_label).ok()?,
            state,
            session_ref,
            seq,
            sample.into(),
        )
    }
}
