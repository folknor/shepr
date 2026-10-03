/// An agent state after applying the user-facing presentation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentedAgentState {
    Idle,
    Working,
    Blocked,
}

impl PresentedAgentState {
    /// Canonical user-facing state spelling.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }

    /// Rank presented states for attention, from least to most urgent.
    pub const fn attention_rank(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Working => 1,
            Self::Blocked => 2,
        }
    }
}
