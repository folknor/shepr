//! Internal app events delivered via channel.
//!
//! PTY child watchers, detectors, hook reports and the Git refresh send events
//! to the main loop through this channel. No polling needed.

use std::time::Instant;

use crate::git::{GitStatusCacheEntry, WorkspaceGitStatus};
use shepr_agent::detect::{Agent, AgentState};
use shepr_core::layout::PaneId;

/// An event from a background task to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    /// A pane's child process exited.
    PaneDied {
        pane_id: PaneId,
        exit_reason: shepr_platform::ChildExitReason,
    },
    /// Process detection identified an agent before its screen state was confirmed.
    AgentProcessDetected {
        pane_id: PaneId,
        agent: Agent,
        observed_at: Instant,
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
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    },
    /// Agent session identity was reported without state authority.
    AgentSessionReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        session_start_source: Option<shepr_agent::agent::resume::AgentSessionStartSource>,
    },
    /// A pane child emitted a valid OSC 52 clipboard write. The main loop
    /// re-emits it to the clients viewing `pane_id`.
    ClipboardWrite { pane_id: PaneId, content: Vec<u8> },
    /// A pane child reported its shell current directory through terminal
    /// metadata such as OSC 7.
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: crate::UsableCwd,
    },
    /// Background git status refresh completed for workspaces.
    GitStatusRefreshed {
        results: Vec<WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
    },
}
