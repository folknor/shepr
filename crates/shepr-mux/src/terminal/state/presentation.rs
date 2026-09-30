use super::*;

impl TerminalState {
    pub fn border_label(&self, show_agent_labels: bool) -> Option<String> {
        self.manual_label.clone().or_else(|| {
            show_agent_labels
                .then(|| self.effective_agent_label().map(str::to_string))
                .flatten()
        })
    }

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
