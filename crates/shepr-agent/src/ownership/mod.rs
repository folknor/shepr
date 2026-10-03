use crate::limits::MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE;
use std::collections::HashMap;
use std::time::{Instant, SystemTime};

// Effective state arbitration is intentionally centralized here. Full lifecycle
// Shepr hook integrations are hook-authoritative while live; screen recovery
// remains only for session-only and partial-state hook paths and fallback
// detection. Confirmed process-exit updates clear matching authority before
// recomputing state.

use crate::agent::resume::{AgentSessionStartSource, ReportedSessionStart};
use crate::agent::{AgentSource, ReportOrigin};
use crate::detect::{Agent, AgentState};

/// One caller-sampled clock pair used throughout a report's validation and commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookClockSample {
    pub monotonic: Instant,
    pub wall: SystemTime,
}

/// Runtime hook authority. Observation times are monotonic instants sampled
/// by the server; they are not the reporter's wall-clock sequence numbers.
/// `reported_at` is the loop's report observation time. Detector observations
/// retain their pre-probe timestamp, so an older queued detector observation
/// cannot override a report merely because it is processed later. Sequence
/// ordering uses the caller-sampled monotonic and wall-clock pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookAuthority {
    pub origin: ReportOrigin,
    pub state: AgentState,
    pub reported_at: Instant,
    pub session_ref: Option<crate::agent::resume::AgentSessionRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveStateChange {
    pub previous_state: AgentState,
    pub state: AgentState,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentOwnershipMutation {
    pub effective_state_change: Option<EffectiveStateChange>,
    pub session_ref_changed: bool,
    pub agent_released: bool,
}

/// Admission result, independent of whether applying a report changes the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    Applied(AgentOwnershipMutation),
    Parked,
    Rejected(HookRejection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRejection {
    MissingSession,
    InvalidSession,
    ReplacedSession,
    ProcessExited,
    DetectedAgentConflict,
    OwnerConflict,
    LifecycleGate,
    RetiredSession,
    CrossTalk,
    UnrecognizedStart,
    MissingSequence,
    OutOfOrder,
    ProcessRequired,
}

impl HookOutcome {
    /// Adapt admission to callers that only consume ownership changes.
    pub fn into_mutation(self) -> Option<AgentOwnershipMutation> {
        match self {
            Self::Applied(mutation) => Some(mutation),
            Self::Parked => Some(AgentOwnershipMutation::default()),
            Self::Rejected(_) => None,
        }
    }
}

/// The winning row of the effective-state arbitration table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveStateSource {
    FullLifecycleHook,
    Hook,
    Screen,
    ProcessExit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecentAgentProcessExit {
    agent: Agent,
    observed_at: Instant,
}

/// The resume identity a detector release just removed, kept for one purpose:
/// a group kill (a cgroup stop, a cgroup OOM kill, a signal to the pane's
/// processes) can kill the agent a moment before its shell, and the release
/// the agent's death causes would otherwise leave the checkpoint taken for the
/// shell's death with nothing to resume. It is never ownership: hooks, the
/// sidebar and ordinary saves never see it. Only a checkpoint-requiring pane
/// ending within `AGENT_PROCESS_EXIT_RELEASE_GRACE` after it, or a signal
/// shutdown within that grace on either side of it, turns it back into the
/// pane's saved identity. Any later selection or new agent process discards
/// it. The proximity cannot prove a shared kill: an agent the user quit just
/// before an unrelated shell death is resumed too.
#[derive(Debug, Clone)]
struct CheckpointCandidate {
    identity: crate::agent::resume::PersistedAgentSession,
    observed_at: Instant,
}

/// Why a terminal's saved identity is being resolved, for the checkpoint
/// candidate: an ordinary save never uses it.
#[derive(Debug, Clone, Copy)]
pub enum CheckpointContext {
    /// The pane's child ended for `reason` at `ended_at`.
    PaneEnding {
        reason: shepr_platform::ChildExitReason,
        ended_at: Instant,
    },
    /// The server received its first termination signal at `signaled_at`;
    /// pane deaths after it are not processed, so the final save resolves
    /// candidates near it instead.
    SignalShutdown { signaled_at: Instant },
}

/// Agent identity, hook authority and detector arbitration for one pane.
/// Independent of terminal runtime, geometry and presentation metadata.
pub struct AgentOwnership {
    detected_agent: Option<Agent>,
    fallback_state: AgentState,
    fallback_visible_blocker: bool,
    fallback_observed_at: Option<Instant>,
    // State authority and resume ownership can belong to different sources.
    // These pane-wide output slots are written by source machine effects;
    // per-source copies would create competing owners and equality invariants.
    hook_authority: Option<HookAuthority>,
    persisted_agent_session: Option<crate::agent::resume::PersistedAgentSession>,
    // Sequence numbers, release gates and retired identities belong to one
    // integration source.
    hook_sources: HashMap<AgentSource, HookSourceState>,
    state: AgentState,
    last_agent_state_change_seq: Option<u64>,
    process_evidence: AgentProcessEvidence,
    checkpoint_candidate: Option<CheckpointCandidate>,
    /// The pane's ending was applied (`transition_pane_exit`). Detector
    /// observations still queued for it change nothing after that: the
    /// child can outlive a failed reader, and a late release would clear the
    /// identity the pane's held checkpoint is saving. Hook reports are not
    /// gated by it: one still in flight from the dead agent can change the
    /// held pane's identity, as it could before this flag existed.
    pane_ended: bool,
}

mod detection;
mod effective;
mod hooks;
mod init;
mod lifecycle;
mod sessions;
mod source;

use source::*;

#[cfg(test)]
impl From<Instant> for HookClockSample {
    fn from(monotonic: Instant) -> Self {
        // Synthetic observation times advance both clocks equally unless a test
        // explicitly supplies a clock step.
        std::thread_local! {
            static ORIGIN: Instant = Instant::now();
        }
        ORIGIN.with(|origin| Self {
            monotonic,
            wall: if monotonic >= *origin {
                SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_000_000)
                    + monotonic.duration_since(*origin)
            } else {
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000)
                    - origin.duration_since(monotonic)
            },
        })
    }
}
