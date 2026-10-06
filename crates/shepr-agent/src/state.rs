shepr_core::named_enum! {
    /// The detected state of a terminal pane.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum AgentState {
        /// Agent finished, prompt visible, nothing happening.
        Idle => "idle",
        /// Agent is actively working/processing.
        Working => "working",
        /// Agent needs human input and is blocked on a response.
        Blocked => "blocked",
        /// Plain shell or unrecognized program.
        Unknown => "unknown",
    }
}

shepr_core::named_enum! {
    /// An agent state after applying the user-facing presentation policy.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum PresentedAgentState {
        Idle => "idle",
        Working => "working",
        Blocked => "blocked",
    }
}

impl PresentedAgentState {
    /// Rank presented states for attention, from least to most urgent.
    pub const fn attention_rank(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Working => 1,
            Self::Blocked => 2,
        }
    }
}

/// The place of a pane's last change of presented agent state in the order
/// one server saw those changes. A pane whose state never changed has
/// [`Self::NEVER`], which sorts before every change. It orders panes for
/// attention and, as an equality token, tells a client that a pane changed.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct StateChangeSeq(u64);

impl StateChangeSeq {
    /// No change has been seen.
    pub const NEVER: Self = Self(0);

    /// Moves to the next place in the order. The sequence cannot reach the end
    /// of the integer range in a process lifetime; it saturates there.
    pub fn advance(&mut self) {
        self.0 = self.0.saturating_add(1);
    }
}

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

    #[test]
    fn agent_state_spellings_round_trip() {
        for value in AgentState::ALL {
            let spelling = value.to_string();
            assert_eq!(
                serde_json::to_value(value).expect("serialize enum"),
                serde_json::Value::String(spelling.clone())
            );
            assert_eq!(
                serde_json::from_value::<AgentState>(serde_json::Value::String(spelling))
                    .expect("deserialize enum"),
                *value
            );
        }
    }
}
