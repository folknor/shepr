use serde::{Deserialize, Serialize};

pub use shepr_protocol::AgentStatus;

/// A pane named by its public ID text, as `detect.capture` and
/// `detect.explain` receive it. The text boundary: the handler parses it and
/// answers `pane_not_found` for text that names no pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTarget {
    pub pane_id: String,
}

pub use shepr_agent::detect::AgentState as PaneAgentState;
