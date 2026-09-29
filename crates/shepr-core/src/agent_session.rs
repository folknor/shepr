//! The kind of reference an agent session is resumed by. It lives here, below
//! both agent detection and the wire protocol, because both name it: agents
//! parse and resume sessions by it, and pane info reports it to clients.

use serde::{Deserialize, Serialize};

/// Whether an agent session is named by its id or by a file path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    Id,
    Path,
}
