use super::*;

impl AgentOwnership {
    pub fn hook_authority(&self) -> Option<&HookAuthority> {
        self.hook_authority.as_ref()
    }

    pub fn persisted_agent_session(&self) -> Option<&crate::agent::resume::PersistedAgentSession> {
        self.persisted_agent_session.as_ref()
    }

    pub fn set_persisted_agent_session(
        &mut self,
        session: crate::agent::resume::PersistedAgentSession,
    ) {
        let _ = self.transition_hook_event(HookEvent::RestoreSession(session));
    }

    pub fn set_agent_session_ref_at(
        &mut self,
        origin: ReportOrigin,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<AgentOwnershipMutation> {
        self.set_agent_session_ref_for_typed_start_source_at(
            origin,
            session_ref,
            seq,
            crate::agent::resume::ReportedSessionStart::Omitted,
            sample,
        )
    }

    pub fn set_agent_session_ref_for_typed_start_source_at(
        &mut self,
        origin: ReportOrigin,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: crate::agent::resume::ReportedSessionStart,
        sample: impl Into<HookClockSample>,
    ) -> Option<AgentOwnershipMutation> {
        self.transition_hook_event(HookEvent::Start {
            origin,
            session_ref,
            seq,
            session_start_source,
            sample: sample.into(),
        })
    }
}

impl AgentOwnership {
    /// Fixture seam: installs `authority` without the report arbitration, so a
    /// test in another crate can model an authority no report is accepted
    /// with (persistence tests use it for malformed identities). Production
    /// code never calls it; reports enter through `set_hook_report_at`.
    pub fn with_initial_hook_authority(mut self, authority: Option<HookAuthority>) -> Self {
        let previous_label = self.effective_agent_label().map(str::to_owned);
        let previous_state = self.state;
        self.hook_authority = authority;
        self.recompute_effective_state(previous_label.as_deref(), previous_state);
        self
    }
}

#[cfg(test)]
impl AgentOwnership {
    pub(super) fn seed_hook_authority_for_test(&mut self, authority: Option<HookAuthority>) {
        let previous_label = self.effective_agent_label().map(str::to_owned);
        let previous_state = self.state;
        self.hook_authority = authority;
        self.recompute_effective_state(previous_label.as_deref(), previous_state);
    }
}
