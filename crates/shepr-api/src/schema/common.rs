use serde::{Deserialize, Serialize};

pub use shepr_protocol::command::{
    ClientShellSurfaceSetParams, PaneTarget, SplitDirection, WorkspaceTarget,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneAgentState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

pub use shepr_protocol::AgentStatus;
