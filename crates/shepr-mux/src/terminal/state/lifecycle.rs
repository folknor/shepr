use super::*;

impl TerminalState {
    pub(super) fn hook_authority_is_effective(&self, authority: &HookAuthority) -> bool {
        !shepr_agent::detect::full_lifecycle_hook_authority(
            &authority.source,
            &authority.agent_label,
        ) || shepr_agent::detect::parse_agent_label(&authority.agent_label).is_none_or(|agent| {
            self.detected_agent == Some(agent) && self.recent_agent_process_exit.is_none()
        })
    }

    pub fn effective_agent_label(&self) -> Option<&str> {
        self.hook_authority
            .as_ref()
            .filter(|authority| self.hook_authority_is_effective(authority))
            .map(|authority| authority.agent_label.as_str())
            .or_else(|| {
                self.recent_agent_process_exit
                    .is_none()
                    .then(|| self.detected_agent.map(shepr_agent::detect::agent_label))
                    .flatten()
            })
    }

    pub fn effective_known_agent(&self) -> Option<Agent> {
        self.effective_agent_label()
            .and_then(shepr_agent::detect::parse_agent_label)
    }

    pub fn unchanged_effective_state_change(&self) -> EffectiveStateChange {
        let agent_label = self.effective_agent_label().map(str::to_string);
        let known_agent = self.effective_known_agent();
        let state = self.state;
        EffectiveStateChange {
            previous_agent_label: agent_label.clone(),
            previous_known_agent: known_agent,
            previous_state: state,
            agent_label,
            known_agent,
            state,
        }
    }

    pub fn full_lifecycle_hook_authority_active(&self) -> bool {
        self.live_full_lifecycle_hook_authority()
    }

    pub(super) fn visible_blocker_overrides_hook(&self) -> bool {
        if self.live_full_lifecycle_hook_authority() {
            return false;
        }
        self.fallback_visible_blocker
            && self.fallback_not_older_than_hook()
            && self.hook_authority.as_ref().is_some_and(|authority| {
                authority.state != AgentState::Blocked
                    && shepr_agent::detect::parse_agent_label(&authority.agent_label)
                        == self.detected_agent
            })
    }

    pub(super) fn live_full_lifecycle_hook_authority(&self) -> bool {
        self.hook_authority.as_ref().is_some_and(|authority| {
            self.hook_authority_is_effective(authority)
                && shepr_agent::detect::full_lifecycle_hook_authority(
                    &authority.source,
                    &authority.agent_label,
                )
        })
    }
}
