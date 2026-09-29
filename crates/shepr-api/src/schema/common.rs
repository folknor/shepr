use serde::{Deserialize, Serialize};

pub use shepr_protocol::command::{
    ClientShellSurfaceSetParams, PaneTarget, SplitDirection, TabTarget, WorkspaceTarget,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EmptyParams {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneAgentState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

pub use shepr_protocol::AgentStatus;
