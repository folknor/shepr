use super::*;

impl AgentOwnership {
    pub(super) fn recompute_effective_state(
        &mut self,
        previous_agent_label: Option<&str>,
        previous_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        let effective = self.effective_agent();
        let state = effective.state;
        let agent_label = effective.label;

        if previous_agent_label == agent_label && previous_state == state {
            return None;
        }

        self.state = state;
        Some(EffectiveStateChange {
            previous_state,
            state,
        })
    }
}

impl AgentOwnership {
    pub fn has_agent(&self) -> bool {
        self.effective_agent_label().is_some()
    }
}
