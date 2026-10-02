use super::*;

mod detection;
mod report;
mod start;

/// The pane contributes shared ownership and detector evidence to each source.
/// Authority and resume identity are pane-wide slots: a sessionless custom
/// source may supply state while an official source still owns the resume id.
/// They must not be duplicated in every source record. All arbitration writes
/// to those slots belong to this machine, including detector and pane exits.
pub(super) enum HookEvent {
    RestoreSession(shepr_agent::agent::resume::PersistedAgentSession),
    Report {
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    },
    Start {
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<AgentSessionStartSource>,
        sample: HookClockSample,
    },
    Detection {
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    },
    PaneExited {
        exit_reason: shepr_platform::ChildExitReason,
        now: Instant,
    },
}

impl TerminalState {
    /// Effects are the only source-table output that writes pane ownership.
    /// Queries and parked reports never pass through a separate commit path.
    /// Any change of the ownership identity moves `ownership_epoch`, which is
    /// what voids a provisional process exit observed under the old owner.
    fn apply_source_effect(&mut self, effect: HookSourceEffects) {
        let before = self.ownership_identity();
        match effect {
            HookSourceEffects::Commit {
                authority,
                persisted,
            } => {
                match authority {
                    AuthorityEffect::Keep => {}
                    AuthorityEffect::Clear => self.hook_authority = None,
                    AuthorityEffect::Set(authority) => self.hook_authority = Some(authority),
                }
                self.persisted_agent_session = persisted;
            }
            HookSourceEffects::ProcessObserved(Some((session, pending))) => {
                self.persisted_agent_session = Some(session);
                if let Some(pending) = pending {
                    self.hook_authority = Some(pending.authority);
                }
            }
            _ => return,
        }
        if self.ownership_identity() != before {
            self.ownership_epoch = self.ownership_epoch.wrapping_add(1);
        }
    }

    /// Who owns the pane: the authority's identity (its state and report time
    /// are not ownership) and the persisted session.
    fn ownership_identity(&self) -> OwnershipIdentity {
        OwnershipIdentity {
            authority: self.hook_authority.as_ref().map(|authority| {
                (
                    authority.source.clone(),
                    authority.agent_label.clone(),
                    authority.session_ref.clone(),
                )
            }),
            persisted: self.persisted_agent_session.clone(),
        }
    }

    #[cfg(not(test))]
    fn check_hook_invariants(&self) {}

    pub(super) fn transition_hook_event(
        &mut self,
        event: HookEvent,
    ) -> Option<TerminalStateMutation> {
        let mutation = match event {
            HookEvent::RestoreSession(session) => {
                self.apply_source_effect(HookSourceEffects::Commit {
                    authority: AuthorityEffect::Keep,
                    persisted: Some(session),
                });
                None
            }
            HookEvent::Report {
                source,
                agent_label,
                state,
                session_ref,
                seq,
                sample,
            } => self.transition_report(source, agent_label, state, session_ref, seq, sample),
            HookEvent::Start {
                source,
                agent_label,
                session_ref,
                seq,
                session_start_source,
                sample,
            } => self.transition_start(
                source,
                agent_label,
                session_ref,
                seq,
                session_start_source,
                sample,
            ),
            HookEvent::Detection {
                agent,
                fallback_state,
                visible_blocker,
                process_exited,
                now,
            } => Some(self.transition_provisional_detection(
                agent,
                fallback_state,
                visible_blocker,
                process_exited,
                now,
            )),
            HookEvent::PaneExited { exit_reason, now } => {
                Some(self.transition_pane_exit(exit_reason, now))
            }
        };
        // Keep this check at the complete event boundary as well as individual
        // source transitions. Production bookkeeping must never panic a server.
        self.check_hook_invariants();
        mutation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HookSequence {
    value: u64,
    accepted_at: Instant,
    accepted_wall_clock: SystemTime,
}

/// Detector process presence is pane evidence, independent of which hook
/// source currently owns state. Provisional detector exits preserve this
/// evidence until confirmation or pane death resolves the ownership.
#[derive(Debug, Clone, Copy, Default)]
pub(super) enum AgentProcessEvidence {
    #[default]
    Available,
    Exited(RecentAgentProcessExit),
}

impl AgentProcessEvidence {
    pub(super) fn exit(self) -> Option<RecentAgentProcessExit> {
        match self {
            Self::Available => None,
            Self::Exited(exit) => Some(exit),
        }
    }
}

/// Ordering, release gating, and retired identities belong to one source.
/// AwaitingProcess gates both confirmed exits and reports racing ahead of
/// initial process detection; it does not claim a previous process existed.
/// AwaitingProcess needs a recognized start and process evidence to reopen.
/// Cleared can also reopen on an identified report for a different session,
/// which re-anchors ordering instead of being checked against it.
/// Suspension has no separate observation here: the detector keeps a stopped
/// process present, so it keeps its open generation and session identity.
/// The selected live session is held by the pane's authority or persisted
/// identity, rather than mirrored here with another equality invariant.
/// The generation variant is the release reason, so they cannot disagree.
/// Under test, every event checks that parked payloads belong to its agent, and retired
/// identities stay bounded. Routing queries return effects without changing
/// any source state.
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

/// Validated pane context is an input, not a second copy of source state.
/// Owner conflicts and integration policy are checked before entering this table.
enum HookSourceEvent<'a> {
    CommitReport {
        authority: HookAuthority,
        seq: Option<u64>,
        sample: HookClockSample,
        reanchor: bool,
        persisted: Option<shepr_agent::agent::resume::PersistedAgentSession>,
    },
    CommitStart {
        seq: Option<u64>,
        sample: HookClockSample,
        selection: Option<bool>,
        session: shepr_agent::agent::resume::PersistedAgentSession,
        replaced: Option<shepr_agent::agent::resume::AgentSessionRef>,
        forget_retired: bool,
        clear_authority: bool,
    },
    Report {
        agent_label: &'a str,
        session_ref: &'a Option<shepr_agent::agent::resume::AgentSessionRef>,
        process_present: bool,
        anchored_session_ref: Option<&'a shepr_agent::agent::resume::AgentSessionRef>,
        authority_session_ref: Option<&'a shepr_agent::agent::resume::AgentSessionRef>,
    },
    Start {
        agent_label: &'a str,
        process_present: bool,
        session_anchored: bool,
        unsequenced_selection: bool,
    },
    Release(
        FullLifecycleHookSuppressionReason,
        SuppressedFullLifecycleHookReport,
    ),
    Activate,
    Select {
        current_session_matches: bool,
    },
    DetectorObservation(Instant),
    ParkStart(
        SuppressedFullLifecycleHookReport,
        shepr_agent::agent::resume::PersistedAgentSession,
    ),
    ParkOrderedStart(
        SuppressedFullLifecycleHookReport,
        shepr_agent::agent::resume::PersistedAgentSession,
        u64,
        HookClockSample,
    ),
    ParkReport(
        SuppressedFullLifecycleHookReport,
        PendingFullLifecycleHookReport,
    ),
    ProcessExited(Instant),
    ProcessObserved,
    RecordSequence(u64, HookClockSample),
    ClearSequence,
    OrderAllows(Option<u64>, HookClockSample),
    Retire(StaleFullLifecycleHookSession),
    Forget(&'a str, &'a shepr_agent::agent::resume::AgentSessionRef),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookStartRoute {
    Commit,
    ParkSelection,
    ParkRecognizedStart,
}

enum HookSourceEffects {
    Commit {
        authority: AuthorityEffect,
        persisted: Option<shepr_agent::agent::resume::PersistedAgentSession>,
    },
    None,
    Report(FullLifecycleHookReportRoute),
    Start(HookStartRoute),
    Activated(Option<SuppressedFullLifecycleHookReport>),
    Parked(bool),
    OrderAllowed(bool),
    DetectorObservationAllowed(bool),
    ProcessObserved(
        Option<(
            shepr_agent::agent::resume::PersistedAgentSession,
            Option<PendingFullLifecycleHookReport>,
        )>,
    ),
}

enum AuthorityEffect {
    Keep,
    Clear,
    Set(HookAuthority),
}

#[derive(PartialEq, Eq)]
struct OwnershipIdentity {
    authority: Option<(
        String,
        String,
        Option<shepr_agent::agent::resume::AgentSessionRef>,
    )>,
    persisted: Option<shepr_agent::agent::resume::PersistedAgentSession>,
}

impl HookSourceState {
    /// Generation/event table. Report, Start and observation queries only decide
    /// routing; capacity, ordering and policy validation must succeed before a commit event. This
    /// keeps rejected reports from evicting records or changing a generation.
    ///
    /// Open + anchored live report accepts; other reports park with an identity.
    /// AwaitingProcess + report parks, and + process requires a pending start.
    /// Cleared + report accepts only a different identified generation; process
    /// evidence reopens it without requiring a start. That distinction is why a
    /// hook clear and a confirmed process exit cannot share one boolean gate.
    /// A future provisional process release can be another event here without
    /// changing the pane's public report/detection entry points.
    /// The pane event boundary validates ownership and policy, then this table
    /// commits generation, ordering and ownership effects together. Public
    /// entry points only adapt their arguments into pane events.
    fn transition(&mut self, event: HookSourceEvent<'_>) -> HookSourceEffects {
        let effects = match (&self.generation, event) {
            (
                _,
                HookSourceEvent::CommitReport {
                    authority,
                    seq,
                    sample,
                    reanchor,
                    persisted,
                },
            ) => {
                if reanchor {
                    self.clear_sequence();
                }
                if let Some(seq) = seq {
                    self.transition(HookSourceEvent::RecordSequence(seq, sample));
                }
                if (authority.session_ref.is_some() || reanchor)
                    && let HookSourceEffects::Activated(Some(released)) =
                        self.transition(HookSourceEvent::Activate)
                    && let Some(session_ref) = released.session_ref
                {
                    self.transition(HookSourceEvent::Retire(StaleFullLifecycleHookSession {
                        agent_label: released.agent_label,
                        session_ref,
                    }));
                }
                HookSourceEffects::Commit {
                    authority: AuthorityEffect::Set(authority),
                    persisted,
                }
            }
            (
                _,
                HookSourceEvent::CommitStart {
                    seq,
                    sample,
                    selection,
                    session,
                    replaced,
                    forget_retired,
                    clear_authority,
                },
            ) => {
                if let Some(current_session_matches) = selection {
                    self.transition(HookSourceEvent::Select {
                        current_session_matches,
                    });
                } else if let Some(seq) = seq {
                    self.transition(HookSourceEvent::RecordSequence(seq, sample));
                }
                let label = session.agent.label();
                if forget_retired {
                    self.transition(HookSourceEvent::Forget(label, &session.session_ref));
                }
                if let Some(session_ref) = replaced {
                    self.transition(HookSourceEvent::Retire(StaleFullLifecycleHookSession {
                        agent_label: label.to_owned(),
                        session_ref,
                    }));
                }
                HookSourceEffects::Commit {
                    authority: if clear_authority {
                        AuthorityEffect::Clear
                    } else {
                        AuthorityEffect::Keep
                    },
                    persisted: Some(session),
                }
            }
            (
                _,
                HookSourceEvent::Report {
                    agent_label,
                    session_ref,
                    process_present,
                    anchored_session_ref,
                    authority_session_ref,
                },
            ) => {
                let stale = session_ref.as_ref().is_some_and(|incoming| {
                    self.stale_sessions.iter().any(|stale| {
                        stale.agent_label == agent_label && &stale.session_ref == incoming
                    })
                });
                let cross_talk = authority_session_ref
                    .zip(session_ref.as_ref())
                    .is_some_and(|(current, incoming)| current != incoming)
                    || (process_present
                        && anchored_session_ref
                            .zip(session_ref.as_ref())
                            .is_some_and(|(anchored, incoming)| anchored != incoming));
                let route = if stale || cross_talk {
                    FullLifecycleHookReportRoute::Ignore
                } else {
                    match &self.generation {
                        HookGeneration::Cleared(released) => {
                            if released.agent_label == agent_label
                                && matches!(
                                    (&released.session_ref, session_ref),
                                    (Some(previous), Some(incoming)) if previous != incoming
                                )
                            {
                                FullLifecycleHookReportRoute::Accept {
                                    reanchor_sequence: true,
                                }
                            } else {
                                FullLifecycleHookReportRoute::Ignore
                            }
                        }
                        HookGeneration::AwaitingProcess(released)
                            if released.agent_label != agent_label =>
                        {
                            FullLifecycleHookReportRoute::Ignore
                        }
                        HookGeneration::Open
                            if process_present
                                && anchored_session_ref.is_some_and(|anchored| {
                                    session_ref
                                        .as_ref()
                                        .is_none_or(|incoming| incoming == anchored)
                                }) =>
                        {
                            FullLifecycleHookReportRoute::Accept {
                                reanchor_sequence: false,
                            }
                        }
                        HookGeneration::Open | HookGeneration::AwaitingProcess(_) => {
                            if session_ref.is_some() {
                                FullLifecycleHookReportRoute::Pending
                            } else {
                                FullLifecycleHookReportRoute::Ignore
                            }
                        }
                    }
                };
                HookSourceEffects::Report(route)
            }
            (
                generation,
                HookSourceEvent::Start {
                    agent_label,
                    process_present,
                    session_anchored,
                    unsequenced_selection,
                },
            ) => {
                let route = if unsequenced_selection {
                    if process_present {
                        HookStartRoute::Commit
                    } else {
                        HookStartRoute::ParkSelection
                    }
                } else if !process_present
                    || !session_anchored
                    || matches!(
                        generation, HookGeneration::AwaitingProcess(released)
                            if released.agent_label == agent_label
                    )
                {
                    HookStartRoute::ParkRecognizedStart
                } else {
                    HookStartRoute::Commit
                };
                HookSourceEffects::Start(route)
            }
            (_, HookSourceEvent::Release(reason, report)) => {
                self.release(reason, report);
                HookSourceEffects::None
            }
            (_, HookSourceEvent::Activate) => HookSourceEffects::Activated(self.activate()),
            (
                generation,
                HookSourceEvent::Select {
                    current_session_matches,
                },
            ) => {
                let new_generation =
                    !matches!(generation, HookGeneration::Open) || !current_session_matches;
                drop(self.activate());
                // Reselecting the current live identity does not revive its
                // older state reports. A new trusted selection resets ordering.
                if new_generation {
                    self.clear_sequence();
                }
                HookSourceEffects::None
            }
            (generation, HookSourceEvent::DetectorObservation(observed_at)) => {
                HookSourceEffects::DetectorObservationAllowed(match generation {
                    HookGeneration::Open => true,
                    HookGeneration::AwaitingProcess(released)
                    | HookGeneration::Cleared(released) => observed_at > released.observed_at,
                })
            }
            (_, HookSourceEvent::ParkOrderedStart(initial, session, seq, sample)) => {
                self.record_sequence(seq, sample);
                self.park_start(initial, session);
                HookSourceEffects::None
            }
            (_, HookSourceEvent::ParkStart(initial, session)) => {
                self.park_start(initial, session);
                HookSourceEffects::None
            }
            (_, HookSourceEvent::ParkReport(initial, pending)) => {
                HookSourceEffects::Parked(self.park_report(initial, pending))
            }
            (_, HookSourceEvent::ProcessExited(now)) => {
                self.process_exited(now);
                HookSourceEffects::None
            }
            (_, HookSourceEvent::ProcessObserved) => {
                HookSourceEffects::ProcessObserved(self.observe_process())
            }
            (_, HookSourceEvent::RecordSequence(value, sample)) => {
                self.record_sequence(value, sample);
                HookSourceEffects::None
            }
            (_, HookSourceEvent::ClearSequence) => {
                self.clear_sequence();
                HookSourceEffects::None
            }
            (_, HookSourceEvent::OrderAllows(seq, sample)) => {
                HookSourceEffects::OrderAllowed(match seq {
                    Some(seq) => !self.sequence.is_some_and(|previous| {
                        previous.supersedes(seq, sample.monotonic, sample.wall)
                    }),
                    None => self.sequence.is_none(),
                })
            }
            (_, HookSourceEvent::Retire(session)) => {
                self.retire(session);
                HookSourceEffects::None
            }
            (_, HookSourceEvent::Forget(label, session)) => {
                self.forget(label, session);
                HookSourceEffects::None
            }
        };
        // A production pane must not panic on its own bookkeeping; the tables
        // are exercised by tests, which assert the invariants after each event.
        self.check_invariants();
        effects
    }

    #[cfg(not(test))]
    fn check_invariants(&self) {}

    fn stale_sessions(&self) -> &[StaleFullLifecycleHookSession] {
        &self.stale_sessions
    }

    fn record_sequence(&mut self, value: u64, sample: HookClockSample) {
        self.sequence = Some(HookSequence {
            value,
            accepted_at: sample.monotonic,
            accepted_wall_clock: sample.wall,
        });
    }

    fn clear_sequence(&mut self) {
        self.sequence = None;
    }

    fn suppressed(&self) -> Option<&SuppressedFullLifecycleHookReport> {
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

    fn release(
        &mut self,
        reason: FullLifecycleHookSuppressionReason,
        report: SuppressedFullLifecycleHookReport,
    ) {
        self.generation = match reason {
            FullLifecycleHookSuppressionReason::AwaitingProcess => {
                HookGeneration::AwaitingProcess(report)
            }
            FullLifecycleHookSuppressionReason::HookClear => HookGeneration::Cleared(report),
        };
    }

    fn activate(&mut self) -> Option<SuppressedFullLifecycleHookReport> {
        match std::mem::take(&mut self.generation) {
            HookGeneration::Open => None,
            HookGeneration::AwaitingProcess(report) | HookGeneration::Cleared(report) => {
                Some(report)
            }
        }
    }

    fn retire(&mut self, session: StaleFullLifecycleHookSession) {
        if self.stale_sessions.contains(&session) {
            return;
        }
        if self.stale_sessions.len() >= MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE {
            self.stale_sessions.remove(0);
        }
        self.stale_sessions.push(session);
    }

    fn forget(&mut self, label: &str, session: &shepr_agent::agent::resume::AgentSessionRef) {
        self.stale_sessions
            .retain(|stale| stale.agent_label != label || &stale.session_ref != session);
    }

    fn park_start(
        &mut self,
        mut initial: SuppressedFullLifecycleHookReport,
        session: shepr_agent::agent::resume::PersistedAgentSession,
    ) {
        if self.suppressed().is_none() {
            initial.pending_start = Some(session);
            self.release(FullLifecycleHookSuppressionReason::AwaitingProcess, initial);
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

    fn park_report(
        &mut self,
        mut initial: SuppressedFullLifecycleHookReport,
        pending: PendingFullLifecycleHookReport,
    ) -> bool {
        if self.suppressed().is_none() {
            initial.pending_replacement_report = Some(pending);
            self.release(FullLifecycleHookSuppressionReason::AwaitingProcess, initial);
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

    fn process_exited(&mut self, now: Instant) {
        let suppressed = match &mut self.generation {
            HookGeneration::AwaitingProcess(suppressed) => suppressed,
            HookGeneration::Cleared(suppressed) if suppressed.pending_start.is_some() => suppressed,
            HookGeneration::Open | HookGeneration::Cleared(_) => return,
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
        if matches!(self.generation, HookGeneration::Cleared(_))
            && let HookGeneration::Cleared(released) = std::mem::take(&mut self.generation)
        {
            // A start parked after a clear remains valid until an exit, not
            // forever. Confirmed exit consumes it and requires another start
            // before subsequent process evidence can install an identity.
            self.generation = HookGeneration::AwaitingProcess(released);
        }
    }

    /// Process evidence alone reopens a hook clear. After an exit it must also
    /// have a recognized pending session start; a parked report is insufficient.
    fn observe_process(
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
            HookGeneration::Cleared(released) if released.pending_start.is_none() => {
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
            HookGeneration::AwaitingProcess(released) | HookGeneration::Cleared(released) => {
                // A clear predates a start subsequently parked in its generation.
                // It does not make that validated start stale. Later process
                // evidence commits it just as it commits an exit-gated start.
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
                if let Some(pending) = pending.as_ref() {
                    self.record_sequence(pending.seq, pending.sample);
                }
                Some((start, pending))
            }
        }
    }
}

impl HookSequence {
    fn supersedes(self, seq: u64, now: Instant, wall_clock: SystemTime) -> bool {
        if seq > self.value {
            return false;
        }
        // An actual reversal of the sampled wall clock corroborates even a
        // small step. Do not lose a final idle report while it catches up.
        if wall_clock < self.accepted_wall_clock {
            return false;
        }
        let monotonic_elapsed = now.saturating_duration_since(self.accepted_at);
        let wall_elapsed = wall_clock
            .duration_since(self.accepted_wall_clock)
            .unwrap_or_default();
        // Silence is not evidence of a clock step. Re-anchor only when the
        // server's wall clock has fallen behind its monotonic clock by the
        // threshold. Smaller elapsed-time discrepancies without an actual
        // wall-clock reversal can also be clock slew or sampling skew; they
        // do not prove a step and must not admit a racing older report.
        // Reporter stamps have differing units, so they are never
        // subtracted from a server clock or an observation Instant.
        monotonic_elapsed.saturating_sub(wall_elapsed) < crate::limits::HOOK_SEQUENCE_REANCHOR_AFTER
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SuppressedFullLifecycleHookReport {
    agent_label: String,
    session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    observed_at: Instant,
    pending_start: Option<shepr_agent::agent::resume::PersistedAgentSession>,
    pending_replacement_report: Option<PendingFullLifecycleHookReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFullLifecycleHookReport {
    authority: HookAuthority,
    seq: u64,
    sample: HookClockSample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullLifecycleHookSuppressionReason {
    HookClear,
    AwaitingProcess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullLifecycleHookReportRoute {
    Accept { reanchor_sequence: bool },
    Ignore,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleFullLifecycleHookSession {
    agent_label: String,
    session_ref: shepr_agent::agent::resume::AgentSessionRef,
}

impl TerminalState {
    fn warn_unrecognized_hook_identity(&self, source: &str, agent_label: &str) {
        // Custom reports remain usable; this warning only makes their unknown owner visible.
        if shepr_agent::agent::AgentSource::from_pair(source, agent_label).is_none() {
            tracing::warn!(
                pane_id = ?self.id,
                source = %source,
                agent_label = %agent_label,
                "hook report uses an unrecognized source or agent label"
            );
        }
    }

    fn hook_authority_not_newer_than(&self, observed_at: Instant) -> bool {
        self.hook_authority
            .as_ref()
            .is_none_or(|authority| authority.reported_at <= observed_at)
    }

    fn hook_authority_conflicts_with_detected_agent(&self, detected_agent: Option<Agent>) -> bool {
        let Some(detected_agent) = detected_agent else {
            return false;
        };
        self.hook_authority.as_ref().is_some_and(|authority| {
            Agent::parse_canonical_label(&authority.agent_label)
                .is_some_and(|hook_agent| hook_agent != detected_agent)
        })
    }

    fn should_ignore_detected_state_under_full_lifecycle_hook(
        &self,
        detected_agent: Option<Agent>,
        process_exited: bool,
    ) -> bool {
        self.full_lifecycle_hook_authority_active()
            && !process_exited
            && !self.hook_authority_conflicts_with_detected_agent(detected_agent)
    }

    fn persisted_agent_session_matches(&self, source: &str, agent: &str) -> bool {
        let Some(source) = shepr_agent::agent::AgentSource::from_pair(source, agent) else {
            return false;
        };
        let Some(agent) = source.agent() else {
            return false;
        };
        self.persisted_agent_session
            .as_ref()
            .is_some_and(|session| session.source == source && session.agent == agent)
    }

    fn suppress_current_full_lifecycle_hook_authority(
        &mut self,
        reason: FullLifecycleHookSuppressionReason,
        now: Instant,
    ) {
        if let Some((source, agent_label, session_ref)) =
            self.hook_authority.as_ref().and_then(|authority| {
                shepr_agent::detect::full_lifecycle_hook_authority(
                    &authority.source,
                    &authority.agent_label,
                )
                .then(|| {
                    (
                        authority.source.clone(),
                        authority.agent_label.clone(),
                        authority.session_ref.clone(),
                    )
                })
            })
        {
            self.suppress_full_lifecycle_hook_report_with_session_ref(
                source,
                agent_label,
                session_ref,
                reason,
                now,
            );
        }
    }

    fn suppress_full_lifecycle_hook_report_with_session_ref(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        reason: FullLifecycleHookSuppressionReason,
        observed_at: Instant,
    ) {
        // A release must retain its gate even if restored session ownership
        // has no record and report admission is full. Only the finite set of
        // built-in full-lifecycle sources reaches this path; dropping the gate
        // for capacity would let late reports revive a completed process.
        self.hook_sources
            .entry(source)
            .or_default()
            .transition(HookSourceEvent::Release(
                reason,
                SuppressedFullLifecycleHookReport {
                    agent_label,
                    session_ref,
                    observed_at,
                    pending_start: None,
                    pending_replacement_report: None,
                },
            ));
    }

    fn route_full_lifecycle_hook_report(
        &mut self,
        source: &str,
        agent_label: &str,
        state: AgentState,
        session_ref: &Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> FullLifecycleHookReportRoute {
        let reported_at = sample.monotonic;
        if !shepr_agent::detect::full_lifecycle_hook_authority(source, agent_label) {
            return FullLifecycleHookReportRoute::Accept {
                reanchor_sequence: false,
            };
        }
        let known_agent = Agent::parse_canonical_label(agent_label);
        let process_present = known_agent.is_some()
            && self.detected_agent == known_agent
            && self.process_evidence.exit().is_none();
        let anchored_session_ref = self
            .hook_authority
            .as_ref()
            .filter(|authority| authority.source == source && authority.agent_label == agent_label)
            .and_then(|authority| authority.session_ref.as_ref())
            .or_else(|| {
                self.persisted_agent_session
                    .as_ref()
                    .filter(|session| {
                        session.source.as_str() == source && session.agent.label() == agent_label
                    })
                    .map(|session| &session.session_ref)
            });
        let authority_session_ref = self
            .hook_authority
            .as_ref()
            .filter(|authority| authority.source == source && authority.agent_label == agent_label)
            .and_then(|authority| authority.session_ref.as_ref());
        let mut empty_source = HookSourceState::default();
        let record = self
            .hook_sources
            .get_mut(source)
            .unwrap_or(&mut empty_source);
        let HookSourceEffects::Report(route) = record.transition(HookSourceEvent::Report {
            agent_label,
            session_ref,
            process_present,
            anchored_session_ref,
            authority_session_ref,
        }) else {
            return FullLifecycleHookReportRoute::Ignore;
        };
        if route != FullLifecycleHookReportRoute::Pending {
            return route;
        }

        // Session-less state can update an anchored generation, but cannot
        // establish or reopen one: it cannot distinguish startup from a late
        // report belonging to a process that already exited.
        let Some(session_ref) = session_ref.clone() else {
            return FullLifecycleHookReportRoute::Ignore;
        };
        let Some(seq) = seq else {
            return FullLifecycleHookReportRoute::Ignore;
        };
        if !self.hook_report_order_allows(source, Some(seq), sample) {
            return FullLifecycleHookReportRoute::Ignore;
        }

        if !self.hook_report_sequence_has_room(source) || !self.prepare_hook_source(source) {
            return FullLifecycleHookReportRoute::Ignore;
        }
        let previous_session_ref = self
            .persisted_agent_session
            .as_ref()
            .filter(|session| {
                session.source.as_str() == source && session.agent.label() == agent_label
            })
            .map(|session| session.session_ref.clone());
        let pending = PendingFullLifecycleHookReport {
            authority: HookAuthority {
                source: source.to_string(),
                agent_label: agent_label.to_string(),
                state,
                reported_at,
                session_ref: Some(session_ref),
            },
            seq,
            sample,
        };
        let parked = self
            .hook_sources
            .entry(source.to_string())
            .or_default()
            .transition(HookSourceEvent::ParkReport(
                SuppressedFullLifecycleHookReport {
                    agent_label: agent_label.to_string(),
                    session_ref: previous_session_ref,
                    observed_at: reported_at,
                    pending_start: None,
                    pending_replacement_report: None,
                },
                pending,
            ));
        if matches!(parked, HookSourceEffects::Parked(true)) {
            FullLifecycleHookReportRoute::Pending
        } else {
            FullLifecycleHookReportRoute::Ignore
        }
    }

    fn same_owner_full_lifecycle_hook_authority_session_ref(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
    ) -> Option<shepr_agent::agent::resume::AgentSessionRef> {
        let authority = self.hook_authority.as_ref()?;
        if !shepr_agent::detect::full_lifecycle_hook_authority(
            &authority.source,
            &authority.agent_label,
        ) || authority.source != source
            || authority.agent_label != agent_label
        {
            return None;
        }
        authority
            .session_ref
            .as_ref()
            .filter(|current| *current != session_ref)
            .cloned()
    }

    fn clear_full_lifecycle_hook_suppression_for_detected_agent(
        &mut self,
        previous_detected_agent: Option<Agent>,
        detected_agent: Option<Agent>,
    ) {
        let Some(detected_agent) = detected_agent else {
            return;
        };
        if previous_detected_agent == Some(detected_agent) {
            return;
        }
        if !detected_agent.descriptor().full_lifecycle_hook_authority {
            return;
        }
        let Some(source) = detected_agent.integration_source() else {
            return;
        };
        let effect = self
            .hook_sources
            .get_mut(source)
            .map(|record| record.transition(HookSourceEvent::ProcessObserved));
        if let Some(effect) = effect {
            self.apply_source_effect(effect);
        }
    }

    fn detected_state_observed_before_release_suppression(
        &mut self,
        detected_agent: Option<Agent>,
        observed_at: Instant,
    ) -> bool {
        let Some(record) = detected_agent
            .and_then(Agent::integration_source)
            .and_then(|source| self.hook_sources.get_mut(source))
        else {
            return false;
        };
        matches!(
            record.transition(HookSourceEvent::DetectorObservation(observed_at)),
            HookSourceEffects::DetectorObservationAllowed(false)
        )
    }

    fn current_session_owner_conflicts(&self, source: &str, agent_label: &str) -> bool {
        let Some(current) = self.current_session_identity_for_persistence() else {
            return false;
        };
        let Some(agent) = shepr_agent::agent::Agent::parse_canonical_label(agent_label) else {
            return true;
        };
        if Agent::parse_source(source).is_some_and(|source_agent| source_agent != agent) {
            return true;
        }
        current.source.as_str() != source || current.agent != agent
    }

    fn conflicting_same_owner_session_ref(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
        session_start_source: Option<AgentSessionStartSource>,
    ) -> Option<shepr_agent::agent::resume::AgentSessionRef> {
        let source = shepr_agent::agent::AgentSource::from_pair(source, agent_label)?;
        let agent = source.agent()?;
        let current = self.current_session_identity_for_persistence()?;
        (current.source == source
            && current.agent == agent
            && current.session_ref.kind() == shepr_agent::agent::resume::AgentSessionRefKind::Id
            && session_ref.kind() == shepr_agent::agent::resume::AgentSessionRefKind::Id
            && &current.session_ref != session_ref
            && !Self::session_report_allows_session_replacement(
                source.as_str(),
                agent.label(),
                session_start_source,
            ))
        .then_some(current.session_ref)
    }

    fn session_report_allows_session_replacement(
        source: &str,
        agent_label: &str,
        session_start_source: Option<AgentSessionStartSource>,
    ) -> bool {
        let Some(agent) = shepr_agent::agent::AgentSource::from_pair(source, agent_label)
            .and_then(|source| source.agent())
        else {
            return false;
        };
        agent
            .descriptor()
            .hook_session_policy
            .allows_replacement(session_start_source)
    }

    fn session_start_source_is_recognized(
        session_start_source: Option<AgentSessionStartSource>,
    ) -> bool {
        session_start_source.is_some()
    }

    fn is_unsequenced_opencode_selection(
        source: &str,
        agent_label: &str,
        session_start_source: Option<AgentSessionStartSource>,
        seq: Option<u64>,
    ) -> bool {
        seq.is_none()
            && session_start_source == Some(AgentSessionStartSource::Select)
            && shepr_agent::agent::AgentSource::from_pair(source, agent_label)
                .and_then(|source| source.agent())
                .is_some_and(|agent| agent.descriptor().hook_session_policy.unsequenced_selection)
    }
}

impl TerminalState {
    fn known_agent_label_conflicts_with_detected_agent(&self, agent_label: &str) -> bool {
        let Some(detected_agent) = self.detected_agent else {
            return false;
        };
        Agent::parse_canonical_label(agent_label)
            .is_some_and(|hook_agent| hook_agent != detected_agent)
    }

    fn foreground_agent_confirms_different_owner_takeover(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
        session_start_source: Option<AgentSessionStartSource>,
    ) -> bool {
        shepr_agent::agent::AgentSource::from_pair(source, agent_label)
            .and_then(|source| source.agent())
            .is_some_and(|agent| agent.descriptor().hook_session_policy.foreground_takeover)
            && Self::session_start_source_is_recognized(session_start_source)
            && self.foreground_agent_confirms_session_owner(source, agent_label, session_ref)
    }

    fn foreground_agent_confirms_hook_authority_takeover(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &Option<shepr_agent::agent::resume::AgentSessionRef>,
    ) -> bool {
        session_ref.as_ref().is_some_and(|session_ref| {
            self.foreground_agent_confirms_session_owner(source, agent_label, session_ref)
        })
    }

    fn foreground_agent_confirms_session_owner(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
    ) -> bool {
        let Some(detected_agent) = self.detected_agent else {
            return false;
        };
        Agent::parse_canonical_label(agent_label) == Some(detected_agent)
            && shepr_agent::agent::resume::PersistedAgentSession::from_report(
                source,
                agent_label,
                session_ref.clone(),
            )
            .and_then(|session| shepr_agent::agent::resume::plan(&session))
            .is_some()
    }

    fn hook_report_order_allows(
        &mut self,
        source: &str,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> bool {
        let sample = sample.into();
        // Routing queries never insert a source or evict ordering history.
        let mut empty_source = HookSourceState::default();
        let record = self
            .hook_sources
            .get_mut(source)
            .unwrap_or(&mut empty_source);
        matches!(
            record.transition(HookSourceEvent::OrderAllows(seq, sample)),
            HookSourceEffects::OrderAllowed(true)
        )
    }

    /// Capacity validation never mutates. Unprotected records are evicted only
    /// when the validated report commits, so rejection preserves all ordering.
    fn hook_report_sequence_has_room(&self, source: &str) -> bool {
        self.hook_sources.contains_key(source)
            || self.hook_sources.len() < MAX_HOOK_REPORT_SOURCES
            || self
                .hook_sources
                .iter()
                .any(|(source, record)| !self.hook_source_protected(source, record))
    }

    fn hook_source_protected(&self, source: &str, record: &HookSourceState) -> bool {
        self.hook_authority
            .as_ref()
            .is_some_and(|authority| authority.source == source)
            || self
                .persisted_agent_session
                .as_ref()
                .is_some_and(|session| session.source.as_str() == source)
            || record.suppressed().is_some()
            || !record.stale_sessions().is_empty()
    }

    fn prepare_hook_source(&mut self, source: &str) -> bool {
        if !self.hook_sources.contains_key(source)
            && self.hook_sources.len() >= MAX_HOOK_REPORT_SOURCES
        {
            let evict = self
                .hook_sources
                .iter()
                .find(|(source, record)| !self.hook_source_protected(source, record))
                .map(|(source, _)| source.clone());
            let Some(evict) = evict else {
                return false;
            };
            self.hook_sources.remove(&evict);
        }
        true
    }

    fn clear_hook_report_sequence(&mut self, source: &str) {
        if let Some(record) = self.hook_sources.get_mut(source) {
            record.transition(HookSourceEvent::ClearSequence);
        }
    }
}

#[cfg(test)]
impl TerminalState {
    fn accept_hook_report_at(
        &mut self,
        source: &str,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> bool {
        let sample = sample.into();
        if !self.hook_report_order_allows(source, seq, sample) {
            return false;
        }
        match seq {
            Some(seq) => self.record_hook_seq(source.to_string(), seq, sample),
            None => true,
        }
    }

    fn record_hook_seq(&mut self, source: String, seq: u64, sample: HookClockSample) -> bool {
        if !self.hook_report_sequence_has_room(&source) {
            tracing::debug!(source = %source, limit = MAX_HOOK_REPORT_SOURCES,
                "ignoring hook report from a new source: too many sources");
            return false;
        }
        if !self.prepare_hook_source(&source) {
            return false;
        }
        self.hook_sources
            .entry(source)
            .or_default()
            .transition(HookSourceEvent::RecordSequence(seq, sample));
        true
    }
}

#[cfg(test)]
impl TerminalState {
    fn check_hook_invariants(&self) {
        for record in self.hook_sources.values() {
            record.check_invariants();
        }
    }
}

#[cfg(test)]
impl HookSourceState {
    fn sequence_value(&self) -> Option<u64> {
        self.sequence.map(|sequence| sequence.value)
    }

    fn check_invariants(&self) {
        assert!(self.stale_sessions.len() <= MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE);
        if let Some(released) = self.suppressed() {
            assert!(
                released
                    .pending_start
                    .as_ref()
                    .is_none_or(|start| { start.agent.label() == released.agent_label })
            );
            assert!(
                released
                    .pending_replacement_report
                    .as_ref()
                    .is_none_or(|pending| {
                        pending.authority.agent_label == released.agent_label
                            && pending.authority.session_ref.is_some()
                    })
            );
        }
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;
    use shepr_agent::agent::resume::{AgentSessionRef, PersistedAgentSession};
    use std::time::Duration;

    fn sample() -> HookClockSample {
        // clock-io-ok: synthetic observation clock for the transition table.
        HookClockSample {
            monotonic: Instant::now(),
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        }
    }

    #[test]
    fn hook_clock_reanchor_requires_a_corroborated_wall_clock_step() {
        let accepted_at = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let sequence = HookSequence {
            value: 1_000,
            accepted_at,
            accepted_wall_clock: wall,
        };
        let later = accepted_at + Duration::from_secs(10);
        // A request arriving within the hook timeout is still a straggler.
        assert!(sequence.supersedes(999, later, wall + Duration::from_secs(10)));
        assert!(sequence.supersedes(1_000, later, wall + Duration::from_secs(10)));
        // A monotonic/wall-clock discrepancy corroborates a backward clock step.
        assert!(!sequence.supersedes(10, later, wall - Duration::from_secs(1)));
        assert!(sequence.supersedes(10, accepted_at + Duration::from_secs(4), wall));
    }

    fn identity(id: &str) -> AgentSessionRef {
        AgentSessionRef::id(id).expect("test session")
    }

    fn session(id: &str) -> PersistedAgentSession {
        PersistedAgentSession::from_report("shepr:pi", "pi", identity(id))
            .expect("official test session")
    }

    fn release(sample: HookClockSample) -> SuppressedFullLifecycleHookReport {
        SuppressedFullLifecycleHookReport {
            agent_label: "pi".into(),
            session_ref: Some(identity("old")),
            observed_at: sample.monotonic,
            pending_start: None,
            pending_replacement_report: None,
        }
    }

    fn record(row: usize, sample: HookClockSample) -> HookSourceState {
        let mut record = HookSourceState::default();
        match row {
            0 => {}
            1 => {
                record.transition(HookSourceEvent::Release(
                    FullLifecycleHookSuppressionReason::AwaitingProcess,
                    release(sample),
                ));
            }
            2 => {
                record.transition(HookSourceEvent::Release(
                    FullLifecycleHookSuppressionReason::HookClear,
                    release(sample),
                ));
            }
            _ => panic!("unknown table row"),
        }
        record
    }

    #[test]
    fn report_table_preserves_generation_and_routes_each_identity() {
        use FullLifecycleHookReportRoute::{Accept, Ignore, Pending};
        // Columns: no identity, old identity, replacement identity. The process
        // is present and the old identity is the pane's selected session.
        let rows = [
            [
                Accept {
                    reanchor_sequence: false,
                },
                Accept {
                    reanchor_sequence: false,
                },
                Ignore,
            ],
            [Ignore, Pending, Ignore],
            [Ignore, Ignore, Ignore],
        ];
        let clock = sample();
        let anchor = identity("old");
        for (row, expected) in rows.into_iter().enumerate() {
            for (incoming, expected) in [None, Some(identity("old")), Some(identity("new"))]
                .into_iter()
                .zip(expected)
            {
                let mut record = record(row, clock);
                let before = record.clone();
                let HookSourceEffects::Report(route) = record.transition(HookSourceEvent::Report {
                    agent_label: "pi",
                    session_ref: &incoming,
                    process_present: true,
                    anchored_session_ref: Some(&anchor),
                    authority_session_ref: None,
                }) else {
                    panic!("report effect")
                };
                assert_eq!(route, expected, "generation row {row}");
                assert_eq!(record, before, "routing must be a query");
            }
        }
        // Without a process, a hook clear admits a different identified report,
        // while an exit/startup gate only parks it.
        for (row, expected) in [
            Pending,
            Pending,
            Accept {
                reanchor_sequence: true,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let mut record = record(row, clock);
            let incoming = Some(identity("new"));
            let HookSourceEffects::Report(route) = record.transition(HookSourceEvent::Report {
                agent_label: "pi",
                session_ref: &incoming,
                process_present: false,
                anchored_session_ref: Some(&anchor),
                authority_session_ref: None,
            }) else {
                panic!("report effect")
            };
            assert_eq!(route, expected);
        }
    }

    #[test]
    fn start_table_distinguishes_selection_from_recognized_start() {
        let clock = sample();
        for row in 0..3 {
            for process_present in [false, true] {
                for session_anchored in [false, true] {
                    for unsequenced_selection in [false, true] {
                        let mut record = record(row, clock);
                        let before = record.clone();
                        let HookSourceEffects::Start(route) =
                            record.transition(HookSourceEvent::Start {
                                agent_label: "pi",
                                process_present,
                                session_anchored,
                                unsequenced_selection,
                            })
                        else {
                            panic!("start effect")
                        };
                        let expected = match (
                            unsequenced_selection,
                            process_present,
                            session_anchored,
                            row,
                        ) {
                            (true, true, _, _) | (false, true, true, 0 | 2) => {
                                HookStartRoute::Commit
                            }
                            (true, false, _, _) => HookStartRoute::ParkSelection,
                            _ => HookStartRoute::ParkRecognizedStart,
                        };
                        assert_eq!(route, expected);
                        assert_eq!(record, before);
                    }
                }
            }
        }
    }

    #[test]
    fn process_table_requires_start_only_after_exit() {
        let clock = sample();
        for row in 0..3 {
            let mut record = record(row, clock);
            record.transition(HookSourceEvent::RecordSequence(20, clock));
            let HookSourceEffects::ProcessObserved(activation) =
                record.transition(HookSourceEvent::ProcessObserved)
            else {
                panic!("process effect")
            };
            assert!(activation.is_none());
            assert_eq!(record.suppressed().is_some(), row == 1);
            assert_eq!(
                record.sequence_value(),
                if row == 1 { Some(20) } else { None }
            );
            assert_eq!(record.stale_sessions().len(), usize::from(row == 2));
        }
    }

    fn report(id: &str, seq: u64, sample: HookClockSample) -> PendingFullLifecycleHookReport {
        PendingFullLifecycleHookReport {
            authority: HookAuthority {
                source: "shepr:pi".into(),
                agent_label: "pi".into(),
                state: AgentState::Working,
                reported_at: sample.monotonic,
                session_ref: Some(identity(id)),
            },
            seq,
            sample,
        }
    }

    #[test]
    fn activation_commits_only_matching_report_newer_than_start() {
        let clock = sample();
        for (report_id, report_seq, accepted) in
            [("new", 21, true), ("new", 20, false), ("other", 21, false)]
        {
            let mut record = record(1, clock);
            record.transition(HookSourceEvent::RecordSequence(20, clock));
            record.transition(HookSourceEvent::ParkStart(release(clock), session("new")));
            record.transition(HookSourceEvent::ParkReport(
                release(clock),
                report(report_id, report_seq, clock),
            ));
            let HookSourceEffects::ProcessObserved(Some((start, pending))) =
                record.transition(HookSourceEvent::ProcessObserved)
            else {
                panic!("activated start")
            };
            assert_eq!(start, session("new"));
            assert_eq!(pending.is_some(), accepted);
            assert!(record.suppressed().is_none());
            assert_eq!(record.stale_sessions()[0].session_ref, identity("old"));
            assert_eq!(
                record.sequence_value(),
                Some(if accepted { report_seq } else { 20 })
            );
        }
    }

    #[test]
    fn report_commit_reanchors_retires_and_emits_the_pane_write_together() {
        let clock = sample();
        let mut record = record(2, clock);
        record.transition(HookSourceEvent::RecordSequence(100, clock));
        let authority = report("new", 1, clock).authority;
        let effect = record.transition(HookSourceEvent::CommitReport {
            authority: authority.clone(),
            seq: Some(1),
            sample: clock,
            reanchor: true,
            persisted: None,
        });
        assert_eq!(record.sequence_value(), Some(1));
        assert!(record.suppressed().is_none());
        assert_eq!(record.stale_sessions()[0].session_ref, identity("old"));
        let mut terminal = TerminalState::new(TerminalId::alloc(), "/".into());
        terminal.apply_source_effect(effect);
        assert_eq!(terminal.hook_authority, Some(authority));
        assert!(terminal.persisted_agent_session.is_none());
    }

    #[test]
    fn start_commit_replaces_authority_and_revives_only_selected_identity() {
        let clock = sample();
        let mut record = HookSourceState::default();
        record.retire(StaleFullLifecycleHookSession {
            agent_label: "pi".into(),
            session_ref: identity("new"),
        });
        let effect = record.transition(HookSourceEvent::CommitStart {
            seq: Some(21),
            sample: clock,
            selection: None,
            session: session("new"),
            replaced: Some(identity("old")),
            forget_retired: true,
            clear_authority: true,
        });
        assert_eq!(record.sequence_value(), Some(21));
        assert_eq!(record.stale_sessions().len(), 1);
        assert_eq!(record.stale_sessions()[0].session_ref, identity("old"));
        let mut terminal = TerminalState::new(TerminalId::alloc(), "/".into());
        terminal.seed_hook_authority_for_test(Some(report("old", 20, clock).authority));
        terminal.apply_source_effect(effect);
        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.persisted_agent_session, Some(session("new")));
    }

    #[test]
    fn process_after_clear_installs_parked_start_through_the_public_entry_points() {
        let clock = sample();
        let mut terminal = TerminalState::new(TerminalId::alloc(), "/".into());
        terminal.set_persisted_agent_session(session("old"));
        terminal.set_detected_agent_process_at(Agent::Pi, clock.monotonic);
        terminal
            .set_hook_report_at(
                shepr_agent::agent::AgentSource::Official(Agent::Pi),
                "pi".into(),
                AgentState::Working,
                Some(identity("old")),
                Some(10),
                clock,
            )
            .expect("live authority");
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Idle,
            false,
            false,
            clock.monotonic + Duration::from_secs(1),
        );
        terminal.set_detected_state_with_screen_signals_at(
            None,
            AgentState::Idle,
            false,
            false,
            clock.monotonic + Duration::from_secs(2),
        );
        let start_clock = HookClockSample {
            monotonic: clock.monotonic + Duration::from_secs(3),
            wall: clock.wall + Duration::from_secs(3),
        };
        assert!(terminal.hook_authority.is_none());
        assert!(terminal.hook_sources["shepr:pi"].suppressed().is_some());
        terminal
            .set_agent_session_ref_for_typed_start_source_at(
                shepr_agent::agent::AgentSource::Official(Agent::Pi),
                "pi".into(),
                Some(identity("new")),
                Some(20),
                Some(AgentSessionStartSource::Startup),
                start_clock,
            )
            .expect("parked start");
        assert_eq!(
            terminal.current_session_identity_for_persistence(),
            Some(session("old"))
        );
        let mutation = terminal
            .set_detected_agent_process_at(Agent::Pi, clock.monotonic + Duration::from_secs(4));
        assert!(mutation.session_ref_changed);
        assert_eq!(
            terminal.current_session_identity_for_persistence(),
            Some(session("new"))
        );
        assert!(terminal.hook_sources["shepr:pi"].suppressed().is_none());
    }

    #[test]
    fn exit_table_consumes_pending_identity_without_reopening() {
        let clock = sample();
        for row in 0..3 {
            let mut record = record(row, clock);
            record.transition(HookSourceEvent::RecordSequence(20, clock));
            if row == 1 {
                record.transition(HookSourceEvent::ParkStart(release(clock), session("new")));
                record.transition(HookSourceEvent::ParkReport(
                    release(clock),
                    report("other", 21, clock),
                ));
            }
            let before = record.clone();
            record.transition(HookSourceEvent::ProcessExited(
                clock.monotonic + Duration::from_secs(1),
            ));
            if row == 1 {
                let released = record.suppressed().expect("exit remains gated");
                assert_eq!(released.session_ref, Some(identity("new")));
                assert!(released.pending_start.is_none());
                assert!(released.pending_replacement_report.is_none());
                assert_eq!(record.sequence_value(), None);
                assert_eq!(record.stale_sessions()[0].session_ref, identity("old"));
            } else {
                assert_eq!(record, before);
            }
        }
    }

    #[test]
    fn replacing_pending_start_discards_only_conflicting_report() {
        let clock = sample();
        let mut record = record(1, clock);
        record.transition(HookSourceEvent::ParkReport(
            release(clock),
            report("new", 21, clock),
        ));
        for id in ["new", "other"] {
            record.transition(HookSourceEvent::ParkStart(release(clock), session(id)));
            let released = record.suppressed().expect("parked start");
            assert_eq!(released.pending_start, Some(session(id)));
            assert_eq!(released.pending_replacement_report.is_some(), id == "new");
        }
    }

    #[test]
    fn stale_and_live_authority_reject_reports_in_every_generation() {
        let clock = sample();
        let old = identity("old");
        for row in 0..3 {
            for retired in [false, true] {
                let mut record = record(row, clock);
                if retired {
                    record.transition(HookSourceEvent::Retire(StaleFullLifecycleHookSession {
                        agent_label: "pi".into(),
                        session_ref: identity("new"),
                    }));
                }
                let before = record.clone();
                let incoming = Some(identity("new"));
                let HookSourceEffects::Report(route) = record.transition(HookSourceEvent::Report {
                    agent_label: "pi",
                    session_ref: &incoming,
                    process_present: false,
                    anchored_session_ref: None,
                    authority_session_ref: (!retired).then_some(&old),
                }) else {
                    panic!("report effect")
                };
                assert_eq!(route, FullLifecycleHookReportRoute::Ignore);
                assert_eq!(record, before);
            }
        }
    }

    #[test]
    fn ordering_table_reanchors_only_with_clock_evidence_or_explicit_reset() {
        let clock = sample();
        for row in 0..3 {
            let mut record = record(row, clock);
            record.transition(HookSourceEvent::RecordSequence(20, clock));
            let later = HookClockSample {
                monotonic: clock.monotonic + Duration::from_secs(3600),
                wall: clock.wall + Duration::from_secs(3600),
            };
            for (seq, observed, accepted) in [
                (Some(21), later, true),
                (Some(20), later, false),
                (Some(19), later, false),
                (None, later, false),
                (
                    Some(19),
                    HookClockSample {
                        wall: clock.wall - Duration::from_secs(1),
                        ..later
                    },
                    true,
                ),
            ] {
                let before = record.clone();
                assert!(
                    matches!(record.transition(HookSourceEvent::OrderAllows(seq, observed)),
                    HookSourceEffects::OrderAllowed(value) if value == accepted)
                );
                assert_eq!(record, before);
            }
            record.transition(HookSourceEvent::ClearSequence);
            assert!(matches!(
                record.transition(HookSourceEvent::OrderAllows(None, later)),
                HookSourceEffects::OrderAllowed(true)
            ));
        }
    }

    #[test]
    fn pending_report_table_keeps_the_largest_report_sequence() {
        let clock = sample();
        for row in 0..3 {
            let mut record = record(row, clock);
            for (seq, parked) in [(21, true), (20, false), (21, false), (22, true)] {
                assert!(matches!(record.transition(HookSourceEvent::ParkReport(
                    release(clock),
                    report("new", seq, clock),
                )), HookSourceEffects::Parked(value) if value == parked));
            }
            let released = record.suppressed().expect("parked report");
            assert_eq!(
                released
                    .pending_replacement_report
                    .as_ref()
                    .expect("report")
                    .seq,
                22
            );
            assert_eq!(
                matches!(record.generation, HookGeneration::Cleared(_)),
                row == 2
            );
        }
    }

    #[test]
    fn explicit_activation_returns_release_for_caller_retirement() {
        let clock = sample();
        for row in 0..3 {
            let mut record = record(row, clock);
            let previous = record.suppressed().cloned();
            let HookSourceEffects::Activated(released) =
                record.transition(HookSourceEvent::Activate)
            else {
                panic!("activation effect")
            };
            assert_eq!(released, previous);
            assert!(record.suppressed().is_none());
            // Report acceptance retires this payload, while trusted selection
            // intentionally handles retirement through its replacement policy.
            assert!(record.stale_sessions().is_empty());
        }
    }

    #[test]
    fn retired_identity_is_unique_bounded_and_can_be_selected_again() {
        let mut record = HookSourceState::default();
        for index in 0..MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE + 1 {
            let retired = StaleFullLifecycleHookSession {
                agent_label: "pi".into(),
                session_ref: identity(&format!("session-{index}")),
            };
            record.transition(HookSourceEvent::Retire(retired.clone()));
            record.transition(HookSourceEvent::Retire(retired));
        }
        assert_eq!(
            record.stale_sessions().len(),
            MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE
        );
        let selected = record.stale_sessions()[0].session_ref.clone();
        assert_eq!(selected, identity("session-1"));
        record.transition(HookSourceEvent::Forget("pi", &selected));
        assert!(
            !record
                .stale_sessions()
                .iter()
                .any(|retired| retired.session_ref == selected)
        );
    }

    #[test]
    fn cleared_process_observation_installs_start_parked_after_clear() {
        let clock = sample();
        let mut record = record(2, clock);
        record.transition(HookSourceEvent::ParkStart(release(clock), session("new")));
        // A later validated start belongs to the next live generation.
        // The earlier clear cannot revoke it when process evidence arrives.
        let HookSourceEffects::ProcessObserved(activation) =
            record.transition(HookSourceEvent::ProcessObserved)
        else {
            panic!("process effect")
        };
        assert_eq!(activation.map(|(start, _)| start), Some(session("new")));
        assert!(record.suppressed().is_none());
        assert_eq!(record.stale_sessions()[0].session_ref, identity("old"));
    }

    #[test]
    fn exit_after_clear_consumes_parked_start_before_later_process_evidence() {
        let clock = sample();
        let mut record = record(2, clock);
        record.transition(HookSourceEvent::ParkOrderedStart(
            release(clock),
            session("new"),
            20,
            clock,
        ));
        record.transition(HookSourceEvent::ProcessExited(
            clock.monotonic + Duration::from_secs(1),
        ));
        assert!(matches!(
            record.generation,
            HookGeneration::AwaitingProcess(_)
        ));
        assert_eq!(
            record.suppressed().expect("exit gate").session_ref,
            Some(identity("new"))
        );
        assert!(
            record
                .suppressed()
                .expect("exit gate")
                .pending_start
                .is_none()
        );
        assert_eq!(record.sequence_value(), None);
        assert!(matches!(
            record.transition(HookSourceEvent::ProcessObserved),
            HookSourceEffects::ProcessObserved(None)
        ));
        assert!(record.suppressed().is_some());
    }

    #[test]
    fn selection_table_resets_only_a_new_generation() {
        let clock = sample();
        for row in 0..3 {
            for current_session_matches in [false, true] {
                let mut record = record(row, clock);
                record.transition(HookSourceEvent::RecordSequence(20, clock));
                record.transition(HookSourceEvent::Select {
                    current_session_matches,
                });
                assert!(record.suppressed().is_none());
                assert_eq!(
                    record.sequence_value(),
                    if row == 0 && current_session_matches {
                        Some(20)
                    } else {
                        None
                    }
                );
            }
        }
    }

    #[test]
    fn detector_table_rejects_observations_at_or_before_release() {
        let clock = sample();
        for row in 0..3 {
            for offset in [0, 1] {
                let mut record = record(row, clock);
                let before = record.clone();
                let observed = clock.monotonic + Duration::from_secs(offset);
                assert!(
                    matches!(record.transition(HookSourceEvent::DetectorObservation(observed)),
                    HookSourceEffects::DetectorObservationAllowed(allowed)
                        if allowed == (row == 0 || offset > 0))
                );
                assert_eq!(record, before);
            }
        }
    }
}

#[cfg(test)]
impl TerminalState {
    fn suppressed_hook_source(&self, source: &str) -> Option<&SuppressedFullLifecycleHookReport> {
        self.hook_sources.get(source)?.suppressed()
    }

    pub fn set_hook_authority(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.set_hook_authority_at(source, agent_label, state, None, seq, Instant::now())
            .and_then(|mutation| mutation.effective_state_change)
    }

    pub fn set_hook_authority_with_session_ref(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
    ) -> Option<TerminalStateMutation> {
        self.set_hook_authority_at(source, agent_label, state, session_ref, seq, Instant::now())
    }
}

#[cfg(test)]
impl TerminalState {
    pub fn set_agent_session_ref(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
    ) -> Option<TerminalStateMutation> {
        self.set_agent_session_ref_at(source.into(), agent_label, session_ref, seq, Instant::now())
    }

    pub fn set_agent_session_ref_for_session_start(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<&str>,
    ) -> Option<TerminalStateMutation> {
        self.set_agent_session_ref_for_typed_start_source_at(
            source.into(),
            agent_label,
            session_ref,
            seq,
            shepr_agent::agent::resume::normalize_session_start_source(session_start_source),
            Instant::now(),
        )
    }
}

#[cfg(test)]
impl TerminalState {
    pub fn set_detected_state(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        self.set_detected_state_with_visible_blocker(agent, fallback_state, false, false, false)
    }

    pub fn set_detected_state_with_mutation(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> TerminalStateMutation {
        self.set_detected_state_with_screen_signals_at(
            agent,
            fallback_state,
            false,
            false,
            Instant::now(),
        )
    }

    pub fn set_detected_state_with_visible_blocker(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        _ignored_screen_idle: bool,
        process_exited: bool,
    ) -> Option<EffectiveStateChange> {
        self.confirmed_detection_for_test(
            agent,
            fallback_state,
            visible_blocker,
            process_exited,
            Instant::now(),
        )
        .effective_state_change
    }

    fn confirmed_detection_for_test(
        &mut self,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> TerminalStateMutation {
        if process_exited {
            self.set_detected_state_with_screen_signals_at(
                agent,
                state,
                visible_blocker,
                true,
                now,
            );
        }
        self.set_detected_state_with_screen_signals_at(
            agent,
            state,
            visible_blocker,
            process_exited,
            if process_exited {
                now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE
            } else {
                now
            },
        )
    }
}

#[cfg(test)]
mod pane_exit_tests {
    use super::*;
    use shepr_agent::agent::resume::{AgentSessionRef, PersistedAgentSession};
    use shepr_platform::ChildExitReason;

    fn running_terminal() -> TerminalState {
        let mut terminal = TerminalState::new(TerminalId::alloc(), "/".into());
        let session = PersistedAgentSession::from_report(
            "shepr:pi",
            "pi",
            AgentSessionRef::id("interrupted-session").expect("session id"),
        )
        .expect("official session");
        // clock-io-ok: synthetic observation time for this test terminal.
        let now = Instant::now();
        terminal.set_detected_agent_process_at(Agent::Pi, now);
        terminal.seed_hook_authority_for_test(Some(HookAuthority {
            source: "shepr:pi".into(),
            agent_label: "pi".into(),
            state: AgentState::Working,
            reported_at: now,
            session_ref: Some(session.session_ref.clone()),
        }));
        terminal.set_persisted_agent_session(session);
        terminal
    }

    #[test]
    fn checkpointed_pane_exit_keeps_resume_identity_without_new_session_dirtiness() {
        for reason in [
            ChildExitReason::Interrupted,
            ChildExitReason::ReaderIoFailed,
        ] {
            let mut terminal = running_terminal();
            let session = terminal.current_session_identity_for_persistence();
            // clock-io-ok: synthetic exit time for the transition under test.
            let now = Instant::now();
            let mutation = terminal.set_pane_process_exit_at(reason, now);
            assert_eq!(terminal.current_session_identity_for_persistence(), session);
            assert!(!mutation.session_ref_changed);
            assert!(terminal.hook_authority.is_none());
            // Replaying publication must not drop the preserved session.
            terminal.set_pane_process_exit_at(reason, now);
            assert_eq!(terminal.current_session_identity_for_persistence(), session);
        }
    }

    #[test]
    fn agent_exit_under_live_shell_clears_resume_identity() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic exit time for the transition under test.
        let now = Instant::now();
        let mutation = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now,
        );
        assert!(!mutation.session_ref_changed);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_some()
        );
        let confirmed_at = now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE;
        let mutation = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            confirmed_at,
        );
        assert!(mutation.session_ref_changed);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
        // A later interrupted shell exit cannot resurrect a completed agent.
        terminal.set_pane_process_exit_at(ChildExitReason::Interrupted, confirmed_at);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
    }

    #[test]
    fn agent_killed_before_shell_keeps_checkpoint_identity() {
        for reason in [
            ChildExitReason::Interrupted,
            ChildExitReason::ReaderIoFailed,
        ] {
            let mut terminal = running_terminal();
            let session = terminal.current_session_identity_for_persistence();
            // clock-io-ok: synthetic observation times for kill ordering.
            let now = Instant::now();
            terminal.set_detected_state_with_screen_signals_at(
                Some(Agent::Pi),
                AgentState::Idle,
                false,
                true,
                now,
            );
            // The following identity withdrawal is also provisional.
            terminal.set_detected_state_with_screen_signals_at(
                None,
                AgentState::Unknown,
                false,
                false,
                now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE / 2,
            );
            let mutation = terminal.set_pane_process_exit_at(
                reason,
                now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE / 2,
            );
            assert_eq!(terminal.current_session_identity_for_persistence(), session);
            assert!(!mutation.session_ref_changed);
            assert!(terminal.provisional_process_exit.is_none());
        }
    }

    #[test]
    fn later_absence_tick_confirms_live_shell_release() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now,
        );
        let mutation = terminal.set_detected_state_with_screen_signals_at(
            None,
            AgentState::Unknown,
            false,
            false,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE,
        );
        assert!(mutation.agent_released);
        assert!(mutation.session_ref_changed);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
        // The confirming observation itself is applied after the release.
        assert_eq!(terminal.detected_agent, None);
    }

    #[test]
    fn confirmed_release_applies_the_withdrawal_seen_inside_the_window() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now,
        );
        terminal.set_detected_state_with_screen_signals_at(
            None,
            AgentState::Unknown,
            false,
            false,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE / 2,
        );
        assert_eq!(terminal.detected_agent, Some(Agent::Pi));
        // The detector's quiet-shell repeat of the release confirms it.
        let mutation = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE,
        );
        assert!(mutation.agent_released);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
        assert_eq!(terminal.detected_agent, None);
        assert_eq!(terminal.fallback_state, AgentState::Unknown);
    }

    #[test]
    fn replacement_process_cancels_provisional_release() {
        let mut terminal = running_terminal();
        let session = terminal.current_session_identity_for_persistence();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now,
        );
        terminal
            .set_detected_agent_process_at(Agent::Pi, now + std::time::Duration::from_millis(1));
        assert!(terminal.provisional_process_exit.is_none());
        assert_eq!(terminal.current_session_identity_for_persistence(), session);
    }

    #[test]
    fn committed_identity_voids_old_scheduled_release() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now,
        );
        let replacement = PersistedAgentSession::from_report(
            "shepr:pi",
            "pi",
            AgentSessionRef::id("replacement").expect("replacement identity"),
        )
        .expect("official identity");
        terminal.set_persisted_agent_session(replacement.clone());
        let mutation = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE,
        );
        assert!(!mutation.agent_released);
        assert_eq!(terminal.persisted_agent_session(), Some(&replacement));
        // The repeat resolved the marker instead of parking behind it.
        assert!(terminal.provisional_process_exit.is_none());
        terminal.set_detected_state_with_screen_signals_at(
            None,
            AgentState::Unknown,
            false,
            false,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE * 2,
        );
        assert_eq!(terminal.persisted_agent_session(), Some(&replacement));
    }

    /// A terminal whose agent is known only to the detector and a persisted
    /// session, with no hook authority to arbitrate detector observations.
    fn detector_only_terminal() -> (TerminalState, Instant) {
        let mut terminal = TerminalState::new(TerminalId::alloc(), "/".into());
        // clock-io-ok: synthetic observation time for this test terminal.
        let now = Instant::now();
        terminal.set_detected_agent_process_at(Agent::Pi, now);
        terminal.set_persisted_agent_session(
            PersistedAgentSession::from_report(
                "shepr:pi",
                "pi",
                AgentSessionRef::id("first").expect("session id"),
            )
            .expect("official session"),
        );
        (terminal, now + std::time::Duration::from_millis(1))
    }

    #[test]
    fn an_ownership_change_inside_the_window_no_longer_freezes_detection() {
        let (mut terminal, now) = detector_only_terminal();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now,
        );
        let replacement = PersistedAgentSession::from_report(
            "shepr:pi",
            "pi",
            AgentSessionRef::id("replacement").expect("replacement identity"),
        )
        .expect("official identity");
        terminal.set_persisted_agent_session(replacement.clone());
        terminal.set_detected_state_with_screen_signals_at(
            None,
            AgentState::Unknown,
            false,
            false,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE / 2,
        );
        // The detector's one republish after the window resolves the voided
        // exit and applies the withdrawal it deferred; nothing is released.
        let mutation = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE,
        );
        assert!(!mutation.agent_released);
        assert!(terminal.provisional_process_exit.is_none());
        assert_eq!(terminal.detected_agent, None);
        assert_eq!(terminal.fallback_state, AgentState::Unknown);
        assert_eq!(terminal.persisted_agent_session(), Some(&replacement));
        // Later observations flow again.
        terminal.set_detected_state_with_screen_signals_at(
            None,
            AgentState::Working,
            false,
            false,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE * 2,
        );
        assert_eq!(terminal.fallback_state, AgentState::Working);
    }

    #[test]
    fn a_confirmed_release_without_a_withdrawal_still_drops_the_exited_agent() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now,
        );
        // Only the detector's quiet-shell repeat arrives: no withdrawal.
        let mutation = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE,
        );
        assert!(mutation.agent_released);
        assert!(terminal.process_evidence.exit().is_some());
        assert_eq!(terminal.state, AgentState::Idle);
        assert!(terminal.effective_agent_label().is_none());
    }

    #[test]
    fn a_state_report_inside_the_window_does_not_void_the_release() {
        let mut terminal = TerminalState::new(TerminalId::alloc(), "/".into());
        // clock-io-ok: synthetic observation and report times.
        let now = Instant::now();
        terminal.set_detected_agent_process_at(Agent::Pi, now);
        let report = |terminal: &mut TerminalState, at: Instant| {
            terminal.set_hook_authority_at(
                "custom-hook".into(),
                "pi".into(),
                AgentState::Working,
                None,
                None,
                HookClockSample {
                    monotonic: at,
                    wall: std::time::SystemTime::now(),
                },
            );
        };
        report(&mut terminal, now + std::time::Duration::from_millis(1));
        let exit_at = now + std::time::Duration::from_millis(2);
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            exit_at,
        );
        // The same authority reports again inside the window: state only, so
        // ownership is unchanged and the release is confirmed, but the
        // authority is newer than the exit and survives it.
        let epoch = terminal.ownership_epoch;
        report(
            &mut terminal,
            exit_at + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE / 2,
        );
        assert_eq!(terminal.ownership_epoch, epoch);
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            exit_at + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE,
        );
        assert!(terminal.provisional_process_exit.is_none());
        assert!(terminal.process_evidence.exit().is_some());
        assert!(
            terminal
                .hook_authority
                .as_ref()
                .is_some_and(|authority| authority.source == "custom-hook")
        );
    }

    #[test]
    fn clearing_the_authority_moves_the_ownership_epoch() {
        let mut terminal = running_terminal();
        let epoch = terminal.ownership_epoch;
        let persisted = terminal.persisted_agent_session.clone();
        terminal.apply_source_effect(HookSourceEffects::Commit {
            authority: AuthorityEffect::Keep,
            persisted: persisted.clone(),
        });
        assert_eq!(terminal.ownership_epoch, epoch, "no change, no move");
        terminal.apply_source_effect(HookSourceEffects::Commit {
            authority: AuthorityEffect::Clear,
            persisted,
        });
        assert_ne!(terminal.ownership_epoch, epoch);
    }

    #[test]
    fn normal_shell_exit_during_provisional_window_clears_identity() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector and shell exit times.
        let now = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            now,
        );
        let mutation = terminal.set_pane_process_exit_at(ChildExitReason::Exited, now);
        assert!(mutation.session_ref_changed);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
        assert!(terminal.provisional_process_exit.is_none());
    }

    #[test]
    fn ordinary_pane_exit_clears_resume_identity() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic exit time for the transition under test.
        let now = Instant::now();
        let mutation = terminal.set_pane_process_exit_at(ChildExitReason::Exited, now);
        assert!(mutation.session_ref_changed);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod behaviour_tests;
