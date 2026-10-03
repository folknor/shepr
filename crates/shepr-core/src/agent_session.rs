//! The kind of reference an agent session is resumed by.

use serde::{Deserialize, Serialize};

/// Whether an agent session is named by its id or by a file path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    Id,
    Path,
}
