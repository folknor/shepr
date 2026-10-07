use super::*;

impl AgentOwnership {
    pub fn new() -> Self {
        Self {
            detected_agent: None,
            fallback_state: AgentState::Unknown,
            fallback_visible_blocker: false,
            fallback_observed_at: None,
            detector_observed_at: None,
            hook_authority: None,
            persisted_agent_session: None,
            hook_sources: HashMap::new(),
            state: AgentState::Unknown,
            last_agent_state_change_seq: None,
            process_evidence: AgentProcessEvidence::default(),
            checkpoint_candidate: None,
            replacement_start: None,
            last_unapplied_hook_report: None,
            pane_ended: false,
        }
    }
}

impl Default for AgentOwnership {
    fn default() -> Self {
        Self::new()
    }
}
