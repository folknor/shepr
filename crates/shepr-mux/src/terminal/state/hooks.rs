use super::*;
use shepr_agent::agent::ReportOrigin;

impl TerminalState {
    /// A live hook state report. The outcome says whether it was applied,
    /// parked until process evidence, or rejected and why.
    pub fn report_hook_outcome_at(
        &mut self,
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> shepr_agent::ownership::HookOutcome {
        self.ownership
            .report_hook_outcome_at(origin, state, session_ref, seq, sample)
    }

    /// A live session start report, with the same admission outcome.
    pub fn report_session_start_outcome_at(
        &mut self,
        origin: &ReportOrigin,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: shepr_agent::agent::resume::ReportedSessionStart,
        sample: HookClockSample,
    ) -> shepr_agent::ownership::HookOutcome {
        self.ownership.report_session_start_outcome_at(
            origin,
            session_ref,
            seq,
            session_start_source,
            sample,
        )
    }

    /// String-input seam retained for fixtures. Live reports enter ownership
    /// through the typed `report_hook_outcome_at` method.
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
