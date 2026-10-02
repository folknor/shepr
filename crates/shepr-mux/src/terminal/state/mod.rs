use crate::limits::{MAX_HOOK_REPORT_SOURCES, MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

// Effective state arbitration is intentionally centralized here. Full lifecycle
// Shepr hook integrations are hook-authoritative while live; screen recovery
// remains only for session-only/custom hook paths and fallback detection.
// Confirmed process-exit updates clear matching authority before recomputing state.

use shepr_agent::agent::resume::AgentSessionStartSource;
use shepr_agent::detect::{Agent, AgentState};
use shepr_protocol::TerminalId;

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
    pub source: String,
    pub agent_label: String,
    pub state: AgentState,
    pub reported_at: Instant,
    pub session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveStateChange {
    pub previous_state: AgentState,
    pub state: AgentState,
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

/// Why a saved pane has no running shell. The pane surface renders its
/// `guidance` and `cause`; detect requests include its `Display` text in the
/// existing API error message when a pane has no runtime. Causes are stored as
/// strings when the failure is recorded, so both presentations can borrow
/// them instead of retaining an OS error object.
#[derive(Debug)]
pub enum RestoreFailure {
    DirectoryUnavailable {
        path: PathBuf,
    },
    DirectoryUnreadable {
        path: PathBuf,
        error: String,
    },
    ShellStartFailed {
        error: String,
    },
    /// The saved agent's resume cannot be issued at all (no command to run,
    /// the pane gone from under the attempt), whatever the directory and shell.
    ResumeUnavailable {
        reason: String,
    },
}

impl RestoreFailure {
    pub fn resume_unavailable(reason: impl Into<String>) -> Self {
        Self::ResumeUnavailable {
            reason: reason.into(),
        }
    }

    pub fn directory_unreadable(path: PathBuf, error: &std::io::Error) -> Self {
        Self::DirectoryUnreadable {
            path,
            error: error.to_string(),
        }
    }

    pub fn shell_start_failed(error: &std::io::Error) -> Self {
        Self::ShellStartFailed {
            error: error.to_string(),
        }
    }

    /// What the operator should do about the failure.
    pub fn guidance(&self) -> &'static str {
        match self {
            Self::DirectoryUnavailable { .. } => {
                "Saved directory is unavailable. Restore the directory and restart this session."
            }
            Self::DirectoryUnreadable { .. } => {
                "Saved directory cannot be read. Fix its access and restart this session."
            }
            Self::ShellStartFailed { .. } => {
                "Could not start the saved shell. Fix the shell configuration and restart this session."
            }
            Self::ResumeUnavailable { .. } => {
                "Could not resume the saved agent. Restart this session."
            }
        }
    }

    /// The error behind the failure, when there is one.
    pub fn cause(&self) -> Option<&str> {
        match self {
            Self::DirectoryUnavailable { .. } => None,
            Self::DirectoryUnreadable { error, .. } | Self::ShellStartFailed { error } => {
                Some(error)
            }
            Self::ResumeUnavailable { reason } => Some(reason),
        }
    }
}

impl std::fmt::Display for RestoreFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.guidance())?;
        match self {
            Self::DirectoryUnavailable { path } | Self::DirectoryUnreadable { path, .. } => {
                write!(formatter, " Directory: {}.", path.display())?;
            }
            Self::ShellStartFailed { .. } | Self::ResumeUnavailable { .. } => {}
        }
        if let Some(cause) = self.cause() {
            write!(formatter, " Error: {cause}")?;
        }
        Ok(())
    }
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
    identity: shepr_agent::agent::resume::PersistedAgentSession,
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

/// Pure state for a server-owned terminal.
///
/// One-to-one with a pane-backed PTY. Terminal identity, cwd, labels and
/// agent state live here, not in pane or view state.
pub struct TerminalState {
    pub id: TerminalId,
    cwd: PathBuf,
    pub detected_agent: Option<Agent>,
    pub fallback_state: AgentState,
    fallback_visible_blocker: bool,
    fallback_observed_at: Option<Instant>,
    // State authority and resume ownership can belong to different sources.
    // These pane-wide output slots are written by source machine effects;
    // per-source copies would create competing owners and equality invariants.
    hook_authority: Option<HookAuthority>,
    persisted_agent_session: Option<shepr_agent::agent::resume::PersistedAgentSession>,
    pub terminal_title: Option<String>,
    pub manual_label: Option<String>,
    hook_sources: HashMap<String, HookSourceState>,
    pub state: AgentState,
    pub last_agent_state_change_seq: Option<u64>,
    process_evidence: AgentProcessEvidence,
    checkpoint_candidate: Option<CheckpointCandidate>,
    /// The pane's ending was applied (`transition_pane_exit`). Detector
    /// observations still queued for it change nothing after that: the
    /// child can outlive a failed reader, and a late release would clear the
    /// identity the pane's held checkpoint is saving. Hook reports are not
    /// gated by it: one still in flight from the dead agent can change the
    /// held pane's identity, as it could before this flag existed.
    pane_ended: bool,
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
