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

/// Hook reports are ordered per source by the `seq` each hook process takes
/// from its own wall clock (nanoseconds for the shell/python hooks,
/// microseconds for the JS plugins; only ever compared within one source).
/// A report whose `seq` is not above the last accepted one is normally a
/// straggler from a racing hook process and is dropped. Hook processes race
/// over milliseconds, though; a non-increasing `seq` arriving this long after
/// the source's last accepted report means the clock stepped backwards (NTP,
/// resume, a manual change), and dropping would lose every report until the
/// clock caught up again. Such a report is accepted and re-anchors the
/// source's sequence.
pub(crate) const HOOK_SEQUENCE_REANCHOR_AFTER: Duration = Duration::from_secs(5);

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
pub(crate) struct MetadataReportSeq {
    pub(crate) seq: u64,
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
enum ManagedAgentPhase {
    Pending {
        ready_after: Option<Instant>,
        deadline: Instant,
        observed_expected: bool,
    },
    Blocked,
    Active,
    /// Restored from a save with a resume planned, and the resume command not
    /// typed yet: no process exists, so nothing observed about the pane can
    /// confirm or refute the agent. Saved like `Active` (the name must
    /// survive a restart before the resume runs) and left alone by
    /// reconciliation.
    AwaitingResume,
    /// The resume command has been typed into the restored shell. Until the
    /// agent's own process (or a hook report from it) shows up this is only a
    /// hope: a failed command leaves a plain shell. Evidence of the agent
    /// makes it `Active`; reaching `deadline` without any releases the name.
    /// Saved like `Active`: until the deadline the name is still the agent's.
    /// The seeded restore detection (`restored_terminal` marks the resumed
    /// agent detected before any process exists) is deliberately not
    /// evidence here.
    Resuming {
        deadline: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManagedAgent {
    kind: Agent,
    phase: ManagedAgentPhase,
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
pub(crate) struct TerminalTitleChange {
    pub(crate) raw_changed: bool,
    pub(crate) stripped_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalStateMutation {
    pub effective_state_change: Option<EffectiveStateChange>,
    pub session_ref_changed: bool,
    pub agent_released: bool,
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
/// During the migration this is still one-to-one with a pane-backed PTY, but
/// pane/view state no longer owns terminal identity, cwd, labels, or agent
/// metadata.
pub struct TerminalState {
    pub id: TerminalId,
    pub cwd: PathBuf,
    pub detected_agent: Option<Agent>,
    pub fallback_state: AgentState,
    fallback_visible_blocker: bool,
    fallback_observed_at: Option<Instant>,
    pub hook_authority: Option<HookAuthority>,
    pub agent_metadata: HashMap<String, AgentMetadata>,
    pub(crate) metadata_tokens: crate::terminal::metadata_tokens::MetadataTokens,
    pub persisted_agent_session: Option<shepr_agent::agent::resume::PersistedAgentSession>,
    pub terminal_title: Option<String>,
    pub manual_label: Option<String>,
    pub agent_name: Option<String>,
    agent_name_owner: Option<AgentNameOwner>,
    managed_agent: Option<ManagedAgent>,
    prompt_ready_agent: Option<Agent>,
    managed_agent_launch_session: Option<shepr_agent::agent::resume::PersistedAgentSession>,
    hook_report_sequences: HashMap<String, u64>,
    /// When each source's entry in `hook_report_sequences` was last
    /// accepted; see [`HOOK_SEQUENCE_REANCHOR_AFTER`].
    hook_report_accepted_at: HashMap<String, Instant>,
    suppressed_full_lifecycle_hook_reports: HashMap<String, SuppressedFullLifecycleHookReport>,
    stale_full_lifecycle_hook_sessions: HashMap<String, Vec<StaleFullLifecycleHookSession>>,
    metadata_report_sequences: HashMap<String, MetadataReportSeq>,
    metadata_report_agents: HashMap<String, Agent>,
    metadata_token_sequence_sources: std::collections::HashSet<String>,
    pub state: AgentState,
    pub last_agent_state_change_seq: Option<u64>,
    pub revision: u64,
    pub launch_argv: Option<Vec<String>>,
    recent_agent_process_exit: Option<RecentAgentProcessExit>,
    pub pending_agent_resume_plan: Option<shepr_agent::agent::resume::AgentResumePlan>,
    pub restore_error: Option<String>,
}

mod detection;
mod hooks;
mod init;
mod lifecycle;
mod managed;
mod presentation;
mod sessions;

/// Whether a managed agent launch in `state` counts as ready for input.
///
/// `Idle` is ready. An agent configured for prompt observation needs that
/// signal when its screen reports `Unknown`. An agent with no screen
/// manifest (Omp, Mastracode) is never anything but `Unknown` on screen, so
/// without its hook that `Unknown` is as settled as it gets and counts as
/// ready once the launch's settle delay has passed; with the hook, the hook
/// state replaces it and `Idle` applies as usual.
pub(super) fn managed_agent_state_is_ready(
    kind: Agent,
    state: AgentState,
    prompt_observed: bool,
    has_screen_manifest: impl FnOnce(Agent) -> bool,
) -> bool {
    match state {
        AgentState::Idle => true,
        AgentState::Unknown if kind.prompt_observation() => prompt_observed,
        AgentState::Unknown => !has_screen_manifest(kind),
        AgentState::Working | AgentState::Blocked => false,
    }
}

#[cfg(test)]
mod tests;
