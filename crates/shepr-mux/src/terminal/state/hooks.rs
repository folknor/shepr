use super::*;
use shepr_agent::agent::ReportOrigin;

impl TerminalState {
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
    ) -> Option<shepr_agent::ownership::AgentOwnershipMutation> {
        self.ownership.set_hook_report_at(
            ReportOrigin::parse(source, agent_label).ok()?,
            state,
            session_ref,
            seq,
            sample.into(),
        )
    }
}
