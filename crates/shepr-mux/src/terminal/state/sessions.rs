use super::*;

impl TerminalState {
    pub fn hook_authority(&self) -> Option<&HookAuthority> {
        self.hook_authority.as_ref()
    }

    pub fn persisted_agent_session(
        &self,
    ) -> Option<&shepr_agent::agent::resume::PersistedAgentSession> {
        self.persisted_agent_session.as_ref()
    }

    pub fn set_persisted_agent_session(
        &mut self,
        session: shepr_agent::agent::resume::PersistedAgentSession,
    ) {
        let _ = self.transition_hook_event(HookEvent::RestoreSession(session));
    }

    pub fn set_agent_session_ref_at(
        &mut self,
        origin: ReportOrigin,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.set_agent_session_ref_for_typed_start_source_at(origin, session_ref, seq, None, sample)
    }

    pub fn set_agent_session_ref_for_typed_start_source_at(
        &mut self,
        origin: ReportOrigin,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<shepr_agent::agent::resume::AgentSessionStartSource>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.transition_hook_event(HookEvent::Start {
            origin,
            session_ref,
            seq,
            session_start_source,
            sample: sample.into(),
        })
    }
}

#[cfg(test)]
impl TerminalState {
    // Persistence tests deliberately model malformed authority which no report
    // accepts. Keep that fixture seam out of the production API.
    pub(crate) fn seed_hook_authority_for_test(&mut self, authority: Option<HookAuthority>) {
        self.hook_authority = authority;
    }
}
