use super::*;

impl AgentOwnership {
    pub fn hook_authority(&self) -> Option<&HookAuthority> {
        self.hook_authority.as_ref()
    }

    pub fn persisted_agent_session(&self) -> Option<&shepr_agent::resume::PersistedAgentSession> {
        self.persisted_agent_session.as_ref()
    }

    pub fn set_persisted_agent_session(
        &mut self,
        session: shepr_agent::resume::PersistedAgentSession,
    ) {
        let _ = self.transition_hook_event(HookEvent::RestoreSession(session));
    }

    pub fn report_session_start_outcome_at(
        &mut self,
        origin: &ReportOrigin,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: ReportedSessionStart,
        sample: impl Into<HookClockSample>,
    ) -> HookOutcome {
        let outcome = self.admit_session_start(
            origin,
            session_ref,
            seq,
            session_start_source,
            sample.into(),
        );
        self.check_hook_invariants();
        outcome
    }

    pub fn set_agent_session_ref_at(
        &mut self,
        origin: ReportOrigin,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<AgentOwnershipMutation> {
        self.set_agent_session_ref_for_typed_start_source_at(
            origin,
            session_ref,
            seq,
            shepr_agent::resume::ReportedSessionStart::Omitted,
            sample,
        )
    }

    pub fn set_agent_session_ref_for_typed_start_source_at(
        &mut self,
        origin: ReportOrigin,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: shepr_agent::resume::ReportedSessionStart,
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
    /// code never calls it; reports enter through `report_hook_outcome_at`.
    pub fn with_initial_hook_authority(mut self, authority: Option<HookAuthority>) -> Self {
        let previous_agent = self.effective_agent();
        let previous_state = self.state;
        self.hook_authority = authority;
        self.recompute_effective_state(previous_agent, previous_state);
        self
    }
}

#[cfg(test)]
impl AgentOwnership {
    pub(super) fn seed_hook_authority_for_test(&mut self, authority: Option<HookAuthority>) {
        let previous_agent = self.effective_agent();
        let previous_state = self.state;
        self.hook_authority = authority;
        self.recompute_effective_state(previous_agent, previous_state);
    }
}
