//! Internal app events delivered via channel.
//!
//! Background tasks (PTY child watchers, future hook listeners, etc.) send
//! events to the main loop through this channel. No polling needed.

use std::path::PathBuf;
use std::time::Instant;

use crate::detect::{Agent, AgentState};
use crate::layout::PaneId;

pub(crate) mod git;
use self::git::automatic_workspace_label;
pub use self::git::{
    GitSpaceMetadata, GitStatusCacheEntry, GitStatusRefreshDemand, derive_label_from_cwd,
    fallback_label_from_cwd, git_status_cache_key, git_status_snapshot_for_cwd_with_demand,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AheadBehind {
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatus {
    pub workspace_id: String,
    pub resolved_identity_cwd: PathBuf,
    pub status_cache_key: PathBuf,
    pub demand: GitStatusRefreshDemand,
    pub auto_label: String,
    pub branch: Option<String>,
    pub ahead_behind: Option<AheadBehind>,
    pub space: Option<GitSpaceMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatusSnapshot {
    pub auto_label: String,
    pub branch: Option<String>,
    pub ahead_behind: Option<AheadBehind>,
    pub space: Option<GitSpaceMetadata>,
}

impl WorkspaceGitStatusSnapshot {
    pub fn into_workspace_status(
        self,
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
        status_cache_key: PathBuf,
        demand: GitStatusRefreshDemand,
    ) -> WorkspaceGitStatus {
        let auto_label = self
            .space
            .as_ref()
            .map(|space| automatic_workspace_label(&resolved_identity_cwd, &space.repo_root))
            .unwrap_or_else(|| fallback_label_from_cwd(&resolved_identity_cwd));
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd,
            status_cache_key,
            demand,
            auto_label,
            branch: self.branch,
            ahead_behind: self.ahead_behind,
            space: self.space,
        }
    }
}

/// An event from a background task to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    /// A pane's child process exited.
    PaneDied {
        pane_id: PaneId,
        exit_reason: crate::platform::ChildExitReason,
    },
    /// Process detection identified an agent before its screen state was confirmed.
    AgentProcessDetected {
        pane_id: PaneId,
        agent: Agent,
        observed_at: Instant,
    },
    /// An agent-specific prompt signal changed during managed startup.
    AgentPromptObserved {
        pane_id: PaneId,
        agent: Agent,
        ready: bool,
    },
    /// Fallback detector state changed in a pane.
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        observed_at: Instant,
    },
    /// Hook-authoritative agent state was reported for a pane.
    HookStateReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
    },
    /// Agent session identity was reported without state authority.
    AgentSessionReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        session_start_source: Option<crate::agent::resume::AgentSessionStartSource>,
    },
    /// Display-only agent metadata was reported for a pane.
    HookMetadataReported {
        pane_id: PaneId,
        source: String,
        agent_label: Option<String>,
        applies_to_source: Option<String>,
        title: Option<String>,
        display_agent: Option<String>,
        state_labels: std::collections::HashMap<String, String>,
        clear_title: bool,
        clear_display_agent: bool,
        clear_state_labels: bool,
        seq: Option<u64>,
        ttl: Option<std::time::Duration>,
    },
    /// Hook authority was explicitly cleared for a pane.
    HookAuthorityCleared {
        pane_id: PaneId,
        source: Option<String>,
        seq: Option<u64>,
    },
    /// The current detected agent gracefully released this pane back to the shell.
    HookAgentReleased {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        known_agent: Option<Agent>,
        seq: Option<u64>,
    },
    /// A pane child emitted a valid OSC 52 clipboard write. The main loop
    /// re-emits it through shepr's own clipboard writer.
    ClipboardWrite { content: Vec<u8> },
    /// A pane child reported its shell current directory through terminal
    /// metadata such as OSC 7.
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: std::path::PathBuf,
    },
    /// Background git status refresh completed for workspaces.
    GitStatusRefreshed {
        results: Vec<WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
    },
    /// A configured tab bar status command finished.
    TabBarCommandFinished {
        segment_index: usize,
        result: Result<Option<String>, String>,
    },
}
