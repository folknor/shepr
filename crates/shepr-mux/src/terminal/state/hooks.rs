use super::*;
use shepr_agent::agent::ReportOrigin;

impl TerminalState {
    // Keep the associated-function path used by persistence iterator adapters.
    pub fn current_session_identity_for_persistence(
        &self,
    ) -> Option<shepr_agent::agent::resume::PersistedAgentSession> {
        self.ownership.current_session_identity_for_persistence()
    }

    /// String-input seam retained for fixtures. Live reports enter ownership
    /// through the typed `set_hook_report_at` method.
    pub fn set_hook_authority_at(
        &mut self,
        source: &str,
        agent_label: &str,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.ownership.set_hook_report_at(
            ReportOrigin::parse(source, agent_label).ok()?,
            state,
            session_ref,
            seq,
            sample.into(),
        )
    }
}
