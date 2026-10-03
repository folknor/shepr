/// The detected state of a terminal pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Agent finished, prompt visible, nothing happening.
    Idle,
    /// Agent is actively working/processing.
    Working,
    /// Agent needs human input and is blocked on a response.
    Blocked,
    /// Plain shell or unrecognized program.
    Unknown,
}

pub use shepr_core::agent_state::PresentedAgentState;

impl AgentState {
    /// Collapse an unknown state to idle for user-facing presentation.
    pub const fn presentation_state(self) -> PresentedAgentState {
        match self {
            Self::Idle | Self::Unknown => PresentedAgentState::Idle,
            Self::Working => PresentedAgentState::Working,
            Self::Blocked => PresentedAgentState::Blocked,
        }
    }

    /// Rank agent states for attention, from least to most urgent.
    pub const fn attention_rank(self) -> u8 {
        self.presentation_state().attention_rank()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_state_collapses_unknown_and_attention_rank_orders_states() {
        assert_eq!(
            AgentState::Unknown.presentation_state(),
            PresentedAgentState::Idle
        );
        assert!(AgentState::Blocked.attention_rank() > AgentState::Working.attention_rank());
        assert!(AgentState::Working.attention_rank() > AgentState::Idle.attention_rank());
        assert_eq!(
            AgentState::Unknown.attention_rank(),
            AgentState::Idle.attention_rank()
        );
    }
}
