use crate::limits::MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE;
use std::collections::HashMap;
use std::time::{Instant, SystemTime};

// Effective state arbitration is intentionally centralized here. Full lifecycle
// Shepr hook integrations are hook-authoritative while live; screen recovery
// remains only for session-only and partial-state hook paths and fallback
// detection. Confirmed process-exit updates clear matching authority before
// recomputing state.

use shepr_agent::resume::{AgentSessionStartSource, ReportedSessionStart};
use shepr_agent::{Agent, AgentSource, AgentState, ReportOrigin};

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
    pub session_ref: Option<shepr_agent::resume::AgentSessionRef>,
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

shepr_core::named_enum! {
    /// Why a hook report was dropped. The snake_case spelling is the detect
    /// explain payload's.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum HookRejection {
        MissingSession => "missing_session",
        InvalidSession => "invalid_session",
        ReplacedSession => "replaced_session",
        ProcessExited => "process_exited",
        DetectedAgentConflict => "detected_agent_conflict",
        OwnerConflict => "owner_conflict",
        LifecycleGate => "lifecycle_gate",
        RetiredSession => "retired_session",
        CrossTalk => "cross_talk",
        UnrecognizedStart => "unrecognized_start",
        MissingSequence => "missing_sequence",
        OutOfOrder => "out_of_order",
        ProcessRequired => "process_required",
    }
}

impl HookRejection {
    /// Whether a bundled shepr hook violated a report contract. Other
    /// rejections describe arbitration races or stale evidence and are routine.
    pub fn is_integration_fault(self) -> bool {
        matches!(
            self,
            Self::MissingSession
                | Self::InvalidSession
                | Self::MissingSequence
                | Self::UnrecognizedStart
        )
    }
}

/// Which hook report an admission outcome answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookReportKind {
    /// A state report naming `AgentState`.
    State(AgentState),
    /// A session start report, with the start source it carried.
    SessionStart(ReportedSessionStart),
}

/// What became of a hook report that did not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnappliedHookDisposition {
    /// Held by its source until what it awaits arrives.
    Parked(ParkedHookAwaiting),
    Rejected(HookRejection),
}

/// What a parked hook report is held for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkedHookAwaiting {
    /// A session start of its agent. A parked state report with no parked
    /// start has no lifetime, and process evidence alone never promotes it: a
    /// late report from a process that already exited would otherwise reopen
    /// its source.
    SessionStart,
    /// Process evidence for its agent, until `expires_at` on the server's
    /// monotonic clock: a parked start, or a state report riding one.
    Process { expires_at: Instant },
}

/// The pane's most recent hook report that was parked or rejected, kept so
/// detect explain can say why a report changed nothing. A later applied
/// report from the same source clears it, as does process evidence promoting
/// a parked report of that source; a later unapplied report replaces it.
/// Whether a parked report is still parked, and what it awaits, is judged
/// where it is read (`AgentOwnership::last_unapplied_hook_report`), against
/// what its source still holds: an exit that consumed it, a start that
/// replaced it, or the expiry of the start it rode leaves it gone without
/// waiting for the next process observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnappliedHookReport {
    pub origin: ReportOrigin,
    pub kind: HookReportKind,
    pub seq: Option<u64>,
    pub session_ref: Option<shepr_agent::resume::AgentSessionRef>,
    /// The server's clock pair when the report was admitted.
    pub received: HookClockSample,
    pub disposition: UnappliedHookDisposition,
}

/// The stored form of `UnappliedHookReport`: a parked report keeps no
/// disposition of its own, since what it awaits is read from its source.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnappliedHookRecord {
    origin: ReportOrigin,
    kind: HookReportKind,
    seq: Option<u64>,
    session_ref: Option<shepr_agent::resume::AgentSessionRef>,
    received: HookClockSample,
    /// `None` while parked.
    rejection: Option<HookRejection>,
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
    identity: shepr_agent::resume::PersistedAgentSession,
    observed_at: Instant,
}

pub use crate::limits::{REPLACEMENT_START_EXIT_WINDOW, REPLACEMENT_START_PRESENCE_GAP};

/// A `startup` refused because the detector still held a live process of the
/// same agent with another session, held for a relaunch.
///
/// For most agents `startup` is not a session replacement: a nested run of the
/// agent (one the pane's agent starts as a tool) sends one too, and must not
/// take over the pane's conversation. But a relaunch (`claude; claude`, or
/// quitting and restarting between two process probes) also sends its startup
/// before the detector has seen the old process go, and refusing it outright
/// loses the new session: the exit that follows clears the old one and nothing
/// carries the new one. So the start is held instead of dropped. When the
/// detector reports that agent's exit within `REPLACEMENT_START_EXIT_WINDOW`
/// of the start's arrival, then the replacement process's presence within
/// `REPLACEMENT_START_PRESENCE_GAP` of that exit, the start is admitted again,
/// as it would have been had it arrived after the exit. A nested run leaves
/// the foreground process alone, so no exit follows and it is never admitted.
///
/// Anything else discards it: another start from its source (which supersedes
/// it, held or not), a detector observation of another agent, a second exit,
/// a window running out, a restored identity, or the pane's ending.
///
/// Two outcomes remain. A nested start followed, within the windows, by the
/// user quitting and relaunching the agent, whose own startup arrives only
/// after the detector reported the relaunch, selects the nested session and
/// leaves the relaunch's start held behind it. And a relaunch the detector
/// does not confirm within the windows loses its session as an unheld start
/// would: the exit still clears the old session, so the pane holds none
/// rather than the wrong one, and a full-lifecycle source waits for its next
/// start. A save or a pane ending before the exit still holds the old session.
#[derive(Debug, Clone)]
struct ReplacementStart {
    origin: ReportOrigin,
    session: shepr_agent::resume::PersistedAgentSession,
    seq: Option<u64>,
    session_start_source: ReportedSessionStart,
    received: HookClockSample,
    /// When the detector reported the held-against process's exit.
    exit_observed_at: Option<Instant>,
}

impl ReplacementStart {
    /// The latest observation that can still advance this start: the exit's,
    /// until one is seen, then the replacement presence's.
    fn deadline(&self) -> Instant {
        let (from, window) = match self.exit_observed_at {
            None => (self.received.monotonic, REPLACEMENT_START_EXIT_WINDOW),
            Some(exit) => (exit, REPLACEMENT_START_PRESENCE_GAP),
        };
        from.checked_add(window).unwrap_or(from)
    }
}

/// Why a terminal's saved identity is being resolved, for the checkpoint
/// candidate: an ordinary save never uses it.
#[derive(Debug, Clone, Copy)]
pub enum CheckpointContext {
    /// A pane ending that needs a checkpoint was recorded at `ended_at`.
    PaneEnding { ended_at: Instant },
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
    persisted_agent_session: Option<shepr_agent::resume::PersistedAgentSession>,
    // Sequence numbers, release gates and retired identities belong to one
    // integration source.
    hook_sources: HashMap<AgentSource, HookSourceState>,
    state: AgentState,
    last_agent_state_change_seq: Option<shepr_agent::StateChangeSeq>,
    process_evidence: AgentProcessEvidence,
    checkpoint_candidate: Option<CheckpointCandidate>,
    /// A refused `startup` held for a relaunch the detector has not yet
    /// confirmed. It is not ownership: nothing reads it but the detector
    /// observations that may admit it and the parked-report diagnostic.
    replacement_start: Option<ReplacementStart>,
    /// Diagnostic only: the last report that changed nothing and why. No
    /// arbitration reads it.
    last_unapplied_hook_report: Option<UnappliedHookRecord>,
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

#[cfg(test)]
mod spelling_tests {
    use super::*;

    #[test]
    fn hook_rejection_spellings_round_trip() {
        for value in HookRejection::ALL {
            let spelling = value.to_string();
            assert_eq!(
                toml::Value::try_from(value).expect("serialize enum"),
                toml::Value::String(spelling.clone())
            );
            assert_eq!(
                toml::Value::String(spelling)
                    .try_into::<HookRejection>()
                    .expect("deserialize enum"),
                *value
            );
        }
    }
}
