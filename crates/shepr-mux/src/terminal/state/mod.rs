pub(crate) use crate::limits::HOOK_SEQUENCE_REANCHOR_AFTER;
use crate::limits::{MAX_HOOK_REPORT_SOURCES, MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// Effective state arbitration is intentionally centralized here. Full lifecycle
// Shepr hook integrations are hook-authoritative while live; screen recovery
// remains only for session-only/custom hook paths and fallback detection.
// Process-exit updates clear matching hook authority before recomputing state.

use shepr_agent::agent::resume::AgentSessionStartSource;
use shepr_agent::detect::{Agent, AgentState};
use shepr_protocol::TerminalId;

#[path = "../metadata.rs"]
mod metadata;
pub use metadata::{AgentMetadata, AgentMetadataReport, EffectivePresentation};

/// Whether a report carrying `seq` is older than the source's last accepted
/// `last_seq` (accepted at `last_accepted_at`). The one ordering rule for
/// every per-source report sequence (hook state and session reports, pane
/// metadata reports, workspace metadata reports in
/// `crate::terminal::metadata_tokens`):
/// a non-increasing `seq` is a straggler unless it arrives
/// [`HOOK_SEQUENCE_REANCHOR_AFTER`] or more after the last acceptance, when
/// it is taken as a clock step and re-anchors the source.
pub(crate) fn report_seq_superseded(
    last_seq: u64,
    last_accepted_at: Option<Instant>,
    seq: u64,
    now: Instant,
) -> bool {
    if seq > last_seq {
        return false;
    }
    !last_accepted_at.is_some_and(|accepted_at| {
        now.saturating_duration_since(accepted_at) >= HOOK_SEQUENCE_REANCHOR_AFTER
    })
}

/// The last accepted sequence of one metadata report source, and when it was
/// accepted (for [`report_seq_superseded`]'s re-anchoring). Pane metadata
/// and workspace metadata token reports both keep one per source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataReportSeq {
    pub seq: u64,
    pub(crate) accepted_at: Instant,
}

impl MetadataReportSeq {
    /// Whether a report carrying `seq`, arriving at `now`, is older than this
    /// accepted one under [`report_seq_superseded`].
    pub(crate) fn supersedes(&self, seq: u64, now: Instant) -> bool {
        report_seq_superseded(self.seq, Some(self.accepted_at), seq, now)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookAuthority {
    pub source: String,
    pub agent_label: String,
    pub state: AgentState,
    pub message: Option<String>,
    // Serde's zero-argument default cannot receive the app clock. Decoding
    // needs a fresh local observation time.
    #[serde(skip, default = "Instant::now")]
    pub reported_at: Instant,
    pub session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SuppressedFullLifecycleHookReport {
    agent_label: String,
    session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    observed_at: Instant,
    reason: FullLifecycleHookSuppressionReason,
    replacement_session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    pending_replacement_report: Option<PendingFullLifecycleHookReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFullLifecycleHookReport {
    authority: HookAuthority,
    seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullLifecycleHookSuppressionReason {
    HookClear,
    ProcessExit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullLifecycleHookReportRoute {
    Accept { reanchor_sequence: bool },
    Ignore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleFullLifecycleHookSession {
    agent_label: String,
    session_ref: shepr_agent::agent::resume::AgentSessionRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResumeNameHold {
    kind: Agent,
    deadline: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveStateChange {
    pub previous_agent_label: Option<String>,
    pub previous_known_agent: Option<Agent>,
    pub previous_state: AgentState,
    pub previous_presentation: EffectivePresentation,
    pub agent_label: Option<String>,
    pub known_agent: Option<Agent>,
    pub state: AgentState,
    pub presentation: EffectivePresentation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalTitleChange {
    pub raw_changed: bool,
    pub stripped_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalStateMutation {
    pub effective_state_change: Option<EffectiveStateChange>,
    pub session_ref_changed: bool,
    pub agent_released: bool,
}

/// Why a saved pane has no running shell. Presentation belongs to the client
/// surface and API boundary, so restore paths retain only the failure facts.
#[derive(Debug)]
pub enum RestoreFailure {
    DirectoryUnavailable {
        path: PathBuf,
    },
    DirectoryUnreadable {
        path: PathBuf,
        error: std::io::Error,
    },
    ShellStartFailed {
        error: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentNameOwner {
    agent_label: String,
    session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecentAgentProcessExit {
    agent: Agent,
    observed_at: Instant,
}

/// Pure state for a server-owned terminal.
///
/// One-to-one with a pane-backed PTY. Terminal identity, cwd, labels and
/// agent metadata live here, not in pane or view state.
pub struct TerminalState {
    pub id: TerminalId,
    cwd: PathBuf,
    pub detected_agent: Option<Agent>,
    pub fallback_state: AgentState,
    fallback_visible_blocker: bool,
    fallback_observed_at: Option<Instant>,
    pub hook_authority: Option<HookAuthority>,
    pub agent_metadata: HashMap<String, AgentMetadata>,
    pub metadata_tokens: crate::terminal::metadata_tokens::MetadataTokens,
    pub persisted_agent_session: Option<shepr_agent::agent::resume::PersistedAgentSession>,
    pub terminal_title: Option<String>,
    pub manual_label: Option<String>,
    pub agent_name: Option<String>,
    agent_name_owner: Option<AgentNameOwner>,
    resume_name_hold: Option<ResumeNameHold>,
    hook_report_sequences: HashMap<String, u64>,
    /// When each source's entry in `hook_report_sequences` was last
    /// accepted; see [`HOOK_SEQUENCE_REANCHOR_AFTER`].
    hook_report_accepted_at: HashMap<String, Instant>,
    /// Only canonical built-in source/label pairs with full-lifecycle
    /// authority can enter this map; custom report sources cannot grow it.
    suppressed_full_lifecycle_hook_reports: HashMap<String, SuppressedFullLifecycleHookReport>,
    /// The source keys have the same restriction, and each source retains at
    /// most `MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE` sessions.
    stale_full_lifecycle_hook_sessions: HashMap<String, Vec<StaleFullLifecycleHookSession>>,
    metadata_report_sequences: HashMap<String, MetadataReportSeq>,
    metadata_report_agents: HashMap<String, Agent>,
    metadata_token_sequence_sources: std::collections::HashSet<String>,
    pub state: AgentState,
    pub last_agent_state_change_seq: Option<u64>,
    revision: u64,
    pub launch_argv: Option<Vec<String>>,
    recent_agent_process_exit: Option<RecentAgentProcessExit>,
    pub pending_agent_resume_plan: Option<shepr_agent::agent::resume::AgentResumePlan>,
    pub restore_error: Option<RestoreFailure>,
}

mod detection;
mod hooks;
mod init;
mod lifecycle;
mod names;
mod presentation;
mod sessions;

#[cfg(test)]
mod tests;
