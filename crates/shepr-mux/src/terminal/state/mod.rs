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

/// An agent can die before its shell under a session-wide signal or OOM kill.
/// Keep all ownership intact until the live shell outlasts the release window.
/// The first observation after the window always resolves the marker. If pane
/// ownership changed during the window (`ownership_epoch` moved), the exit is
/// void: it performs no release effects, since the new owner is not the agent
/// that exited, and only its genuine detector withdrawal is applied, under the
/// ordinary arbitration.
#[derive(Debug, Clone, Copy)]
struct ProvisionalProcessExit {
    agent: Option<Agent>,
    observed_at: Instant,
    /// `TerminalState::ownership_epoch` when the exit was observed.
    ownership_epoch: u64,
    /// The latest agent-less detector observation inside the window. The
    /// detector withdraws the exited identity right after reporting the exit;
    /// a confirmed release applies that withdrawal after itself.
    deferred: Option<DeferredDetection>,
}

#[derive(Debug, Clone, Copy)]
struct DeferredDetection {
    fallback_state: AgentState,
    visible_blocker: bool,
    observed_at: Instant,
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
    /// Counts changes of pane ownership: the hook authority's identity
    /// (presence, source, agent label, session ref) or the persisted session.
    /// Bumped only in `apply_source_effect`, the one writer of both slots.
    ownership_epoch: u64,
    provisional_process_exit: Option<ProvisionalProcessExit>,
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
