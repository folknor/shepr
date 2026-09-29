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
        previous_agent_label: Option<String>,
        previous_known_agent: Option<Agent>,
        previous_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        let state = if self.visible_blocker_overrides_hook() {
            AgentState::Blocked
        } else {
            self.hook_authority
                .as_ref()
                .filter(|authority| self.hook_authority_is_effective(authority))
                .map_or(self.fallback_state, |authority| authority.state)
        };
        let agent_label = self.effective_agent_label().map(str::to_string);
        let known_agent = self.effective_known_agent();

        if previous_agent_label == agent_label && previous_state == state {
            return None;
        }

        self.state = state;
        Some(EffectiveStateChange {
            previous_agent_label,
            previous_known_agent,
            previous_state,
            agent_label,
            known_agent,
            state,
        })
    }
}
