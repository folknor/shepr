use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HookSequence {
    pub(super) value: u64,
    pub(super) accepted_at: Instant,
    pub(super) accepted_wall_clock: SystemTime,
}

/// Ordering, release gating, and retired identities belong to one source.
/// AwaitingProcess gates both confirmed exits and reports racing ahead of
/// initial process detection; it does not claim a previous process existed.
/// A recognized session start and process observation reopen that generation.
/// Suspension has no separate observation here: the detector keeps a stopped
/// process present, so it keeps its open generation and session identity.
/// The selected live session is held by the pane's authority or persisted
/// identity, rather than mirrored here with another equality invariant.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct HookSourceState {
    sequence: Option<HookSequence>,
    generation: HookGeneration,
    stale_sessions: Vec<StaleFullLifecycleHookSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum HookGeneration {
    #[default]
    Open,
    AwaitingProcess(SuppressedFullLifecycleHookReport),
    Cleared(SuppressedFullLifecycleHookReport),
}

impl HookSourceState {
    pub(super) fn sequence_value(&self) -> Option<u64> {
        self.sequence.map(|sequence| sequence.value)
    }

    pub(super) fn stale_sessions(&self) -> &[StaleFullLifecycleHookSession] {
        &self.stale_sessions
    }

    pub(super) fn record_sequence(&mut self, value: u64) {
        self.sequence = Some(HookSequence {
            value,
            accepted_at: Instant::now(),
            accepted_wall_clock: SystemTime::now(),
        });
    }

    pub(super) fn clear_sequence(&mut self) {
        self.sequence = None;
    }

    pub(super) fn suppressed(&self) -> Option<&SuppressedFullLifecycleHookReport> {
        match &self.generation {
            HookGeneration::Open => None,
            HookGeneration::AwaitingProcess(report) | HookGeneration::Cleared(report) => {
                Some(report)
            }
        }
    }

    fn suppressed_mut(&mut self) -> Option<&mut SuppressedFullLifecycleHookReport> {
        match &mut self.generation {
            HookGeneration::Open => None,
            HookGeneration::AwaitingProcess(report) | HookGeneration::Cleared(report) => {
                Some(report)
            }
        }
    }

    pub(super) fn release(&mut self, report: SuppressedFullLifecycleHookReport) {
        self.generation = match report.reason {
            FullLifecycleHookSuppressionReason::AwaitingProcess => {
                HookGeneration::AwaitingProcess(report)
            }
            FullLifecycleHookSuppressionReason::HookClear => HookGeneration::Cleared(report),
        };
    }

    pub(super) fn activate(&mut self) -> Option<SuppressedFullLifecycleHookReport> {
        match std::mem::take(&mut self.generation) {
            HookGeneration::Open => None,
            HookGeneration::AwaitingProcess(report) | HookGeneration::Cleared(report) => {
                Some(report)
            }
        }
    }

    pub(super) fn retire(&mut self, session: StaleFullLifecycleHookSession) {
        if self.stale_sessions.contains(&session) {
            return;
        }
        if self.stale_sessions.len() >= MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE {
            self.stale_sessions.remove(0);
        }
        self.stale_sessions.push(session);
    }

    pub(super) fn forget(
        &mut self,
        label: &str,
        session: &shepr_agent::agent::resume::AgentSessionRef,
    ) {
        self.stale_sessions
            .retain(|stale| stale.agent_label != label || &stale.session_ref != session);
    }

    pub(super) fn park_start(
        &mut self,
        mut initial: SuppressedFullLifecycleHookReport,
        session: shepr_agent::agent::resume::PersistedAgentSession,
    ) {
        if self.suppressed().is_none() {
            initial.pending_start = Some(session);
            self.release(initial);
            return;
        }
        if let Some(suppressed) = self.suppressed_mut()
            && suppressed
                .pending_start
                .as_ref()
                .map(|start| &start.session_ref)
                != Some(&session.session_ref)
        {
            if suppressed
                .pending_replacement_report
                .as_ref()
                .is_some_and(|pending| {
                    pending.authority.session_ref.as_ref() != Some(&session.session_ref)
                })
            {
                suppressed.pending_replacement_report = None;
            }
            suppressed.pending_start = Some(session);
        }
    }

    pub(super) fn park_report(
        &mut self,
        mut initial: SuppressedFullLifecycleHookReport,
        pending: PendingFullLifecycleHookReport,
    ) -> bool {
        if self.suppressed().is_none() {
            initial.pending_replacement_report = Some(pending);
            self.release(initial);
            return true;
        }
        if let Some(suppressed) = self.suppressed_mut()
            && suppressed
                .pending_replacement_report
                .as_ref()
                .is_none_or(|previous| pending.seq > previous.seq)
        {
            suppressed.pending_replacement_report = Some(pending);
            return true;
        }
        false
    }

    pub(super) fn process_exited(&mut self, now: Instant) {
        let HookGeneration::AwaitingProcess(suppressed) = &mut self.generation else {
            return;
        };
        let exited = suppressed
            .pending_start
            .take()
            .map(|start| start.session_ref)
            .or_else(|| {
                suppressed
                    .pending_replacement_report
                    .as_ref()
                    .and_then(|pending| pending.authority.session_ref.clone())
            })
            .or_else(|| suppressed.session_ref.clone());
        let stale = suppressed
            .session_ref
            .as_ref()
            .zip(exited.as_ref())
            .filter(|(previous, exited)| previous != exited)
            .map(|(previous, _)| StaleFullLifecycleHookSession {
                agent_label: suppressed.agent_label.clone(),
                session_ref: previous.clone(),
            });
        suppressed.session_ref = exited;
        suppressed.pending_replacement_report = None;
        suppressed.observed_at = now;
        self.sequence = None;
        if let Some(stale) = stale {
            self.retire(stale);
        }
    }

    /// A released generation is reopened only by process evidence. After an
    /// exit, process evidence must also have a recognized pending session start.
    pub(super) fn observe_process(
        &mut self,
    ) -> Option<(
        shepr_agent::agent::resume::PersistedAgentSession,
        Option<PendingFullLifecycleHookReport>,
    )> {
        let start_seq = self.sequence.map(|sequence| sequence.value);
        match &mut self.generation {
            HookGeneration::Open => {
                self.sequence = None;
                None
            }
            HookGeneration::Cleared(_) => {
                if let Some(released) = self.activate()
                    && let Some(session_ref) = released.session_ref
                {
                    self.retire(StaleFullLifecycleHookSession {
                        agent_label: released.agent_label,
                        session_ref,
                    });
                }
                self.sequence = None;
                None
            }
            HookGeneration::AwaitingProcess(released) => {
                // The start was policy-validated before it was parked. Process
                // evidence commits that identity without a fallible conversion.
                let start = released.pending_start.take()?;
                let label = start.agent.label();
                let stale = released
                    .session_ref
                    .as_ref()
                    .filter(|old| *old != &start.session_ref)
                    .map(|old| StaleFullLifecycleHookSession {
                        agent_label: label.to_owned(),
                        session_ref: old.clone(),
                    });
                let pending = released
                    .pending_replacement_report
                    .take()
                    .filter(|pending| {
                        pending.authority.session_ref.as_ref() == Some(&start.session_ref)
                            && start_seq.is_none_or(|seq| pending.seq > seq)
                    });
                self.generation = HookGeneration::Open;
                if let Some(stale) = stale {
                    self.retire(stale);
                }
                self.forget(label, &start.session_ref);
                Some((start, pending))
            }
        }
    }

    pub(super) fn seq_superseded(&self, seq: u64, now: Instant, wall_clock: SystemTime) -> bool {
        self.sequence
            .is_some_and(|previous| previous.supersedes(seq, now, wall_clock))
    }
}

impl HookSequence {
    pub(super) fn supersedes(self, seq: u64, now: Instant, wall_clock: SystemTime) -> bool {
        if seq > self.value {
            return false;
        }
        let monotonic_elapsed = now.saturating_duration_since(self.accepted_at);
        let wall_elapsed = wall_clock
            .duration_since(self.accepted_wall_clock)
            .unwrap_or_default();
        // Silence is not evidence of a clock step. Re-anchor only when the
        // server's wall clock has fallen behind its monotonic clock by the
        // threshold. Reporter stamps have differing units, so they are never
        // subtracted from a server clock or an observation Instant.
        monotonic_elapsed.saturating_sub(wall_elapsed) < crate::limits::HOOK_SEQUENCE_REANCHOR_AFTER
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SuppressedFullLifecycleHookReport {
    pub(super) agent_label: String,
    pub(super) session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    pub(super) observed_at: Instant,
    pub(super) reason: FullLifecycleHookSuppressionReason,
    pub(super) pending_start: Option<shepr_agent::agent::resume::PersistedAgentSession>,
    pub(super) pending_replacement_report: Option<PendingFullLifecycleHookReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingFullLifecycleHookReport {
    pub(super) authority: HookAuthority,
    pub(super) seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FullLifecycleHookSuppressionReason {
    HookClear,
    AwaitingProcess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FullLifecycleHookReportRoute {
    Accept { reanchor_sequence: bool },
    Ignore,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StaleFullLifecycleHookSession {
    pub(super) agent_label: String,
    pub(super) session_ref: shepr_agent::agent::resume::AgentSessionRef,
}
