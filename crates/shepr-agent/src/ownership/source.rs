use super::*;

mod detection;
mod report;
mod start;

/// The pane contributes shared ownership and detector evidence to each source.
/// Authority and resume identity are pane-wide slots: one source may supply
/// state while another still owns the resume id (a promoted parked start, a
/// restored identity). They must not be duplicated in every source record. All arbitration writes
/// to those slots belong to this machine, including detector and pane exits.
pub(super) enum HookEvent {
    RestoreSession(crate::agent::resume::PersistedAgentSession),
    Report {
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    },
    Start {
        origin: ReportOrigin,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: ReportedSessionStart,
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

impl AgentOwnership {
    /// Effects are the only source-table output that writes pane ownership.
    /// Queries and parked reports never pass through a separate commit path.
    /// A generic `Commit` is not a selection (detector withdrawals and pane
    /// exits commit too), so it leaves the checkpoint candidate alone; the
    /// selection paths discard it themselves. A parked start promoted by
    /// process evidence is a selection.
    fn apply_source_effect(&mut self, effect: HookSourceEffects) {
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
                self.checkpoint_candidate = None;
                self.persisted_agent_session = Some(session);
                if let Some(pending) = pending {
                    self.hook_authority = Some(pending.authority);
                }
            }
            _ => {}
        }
    }

    #[cfg(not(test))]
    pub(super) fn check_hook_invariants(&self) {}

    pub(super) fn transition_hook_event(
        &mut self,
        event: HookEvent,
    ) -> Option<AgentOwnershipMutation> {
        let mutation = match event {
            HookEvent::RestoreSession(session) => {
                self.checkpoint_candidate = None;
                self.apply_source_effect(HookSourceEffects::Commit {
                    authority: AuthorityEffect::Keep,
                    persisted: Some(session),
                });
                None
            }
            HookEvent::Report {
                origin,
                state,
                session_ref,
                seq,
                sample,
            } => self
                .transition_report(origin, state, session_ref, seq, sample)
                .into_mutation(),
            HookEvent::Start {
                origin,
                session_ref,
                seq,
                session_start_source,
                sample,
            } => self
                .transition_start(&origin, session_ref, seq, session_start_source, sample)
                .into_mutation(),
            HookEvent::Detection {
                agent,
                fallback_state,
                visible_blocker,
                process_exited,
                now,
            } => Some(self.transition_detector_observation(
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
    // Hook reports contain no process handle. Only nearby, newer detector
    // evidence can attribute a parked identity to a process of this agent.
    pending_start_at: Option<Instant>,
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
    /// A committed report empties the persisted slot; the pane's identity is
    /// read from the authority's session from then on.
    CommitReport {
        authority: HookAuthority,
        seq: Option<u64>,
        sample: HookClockSample,
        reanchor: bool,
    },
    CommitStart {
        seq: Option<u64>,
        sample: HookClockSample,
        selection: Option<bool>,
        session: crate::agent::resume::PersistedAgentSession,
        replaced: Option<crate::agent::resume::AgentSessionRef>,
        forget_retired: bool,
        clear_authority: bool,
    },
    Release(
        FullLifecycleHookSuppressionReason,
        SuppressedFullLifecycleHookReport,
    ),
    Activate,
    Select {
        current_session_matches: bool,
    },
    ParkStart(
        SuppressedFullLifecycleHookReport,
        crate::agent::resume::PersistedAgentSession,
    ),
    ParkOrderedStart(
        SuppressedFullLifecycleHookReport,
        crate::agent::resume::PersistedAgentSession,
        u64,
        HookClockSample,
    ),
    ParkReport(
        SuppressedFullLifecycleHookReport,
        PendingFullLifecycleHookReport,
    ),
    ProcessExited(Instant),
    ProcessObserved(Instant),
    RecordSequence(u64, HookClockSample),
    ClearSequence,
    Retire(StaleFullLifecycleHookSession),
    Forget(Agent, &'a crate::agent::resume::AgentSessionRef),
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
        persisted: Option<crate::agent::resume::PersistedAgentSession>,
    },
    None,
    Activated(Option<SuppressedFullLifecycleHookReport>),
    Parked(bool),
    ProcessObserved(
        Option<(
            crate::agent::resume::PersistedAgentSession,
            Option<PendingFullLifecycleHookReport>,
        )>,
    ),
}

enum AuthorityEffect {
    Keep,
    Clear,
    Set(HookAuthority),
}

impl HookSourceState {
    fn report_route(
        &self,
        agent: Agent,
        session_ref: &Option<crate::agent::resume::AgentSessionRef>,
        process_present: bool,
        anchored_session_ref: Option<&crate::agent::resume::AgentSessionRef>,
        authority_session_ref: Option<&crate::agent::resume::AgentSessionRef>,
    ) -> FullLifecycleHookReportRoute {
        let stale = session_ref.as_ref().is_some_and(|incoming| {
            self.stale_sessions
                .iter()
                .any(|stale| stale.agent == agent && &stale.session_ref == incoming)
        });
        let cross_talk = authority_session_ref
            .zip(session_ref.as_ref())
            .is_some_and(|(current, incoming)| current != incoming)
            || (process_present
                && anchored_session_ref
                    .zip(session_ref.as_ref())
                    .is_some_and(|(anchored, incoming)| anchored != incoming));
        if stale {
            FullLifecycleHookReportRoute::Ignore(HookRejection::RetiredSession)
        } else if cross_talk {
            FullLifecycleHookReportRoute::Ignore(HookRejection::CrossTalk)
        } else {
            match &self.generation {
                HookGeneration::Cleared(released) => {
                    if released.agent == agent
                        && matches!(
                            (&released.session_ref, session_ref),
                            (Some(previous), Some(incoming)) if previous != incoming
                        )
                    {
                        FullLifecycleHookReportRoute::Accept {
                            reanchor_sequence: true,
                        }
                    } else {
                        FullLifecycleHookReportRoute::Ignore(HookRejection::LifecycleGate)
                    }
                }
                HookGeneration::AwaitingProcess(released) if released.agent != agent => {
                    FullLifecycleHookReportRoute::Ignore(HookRejection::LifecycleGate)
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
                        FullLifecycleHookReportRoute::Ignore(HookRejection::LifecycleGate)
                    }
                }
            }
        }
    }

    fn start_route(
        &self,
        agent: Agent,
        process_present: bool,
        session_anchored: bool,
        unsequenced_selection: bool,
    ) -> HookStartRoute {
        if unsequenced_selection {
            if process_present {
                HookStartRoute::Commit
            } else {
                HookStartRoute::ParkSelection
            }
        } else if !process_present
            || !session_anchored
            || matches!(
                &self.generation, HookGeneration::AwaitingProcess(released)
                    if released.agent == agent
            )
        {
            HookStartRoute::ParkRecognizedStart
        } else {
            HookStartRoute::Commit
        }
    }

    fn order_allows(&self, seq: Option<u64>, sample: HookClockSample) -> bool {
        match seq {
            Some(seq) => !self
                .sequence
                .is_some_and(|previous| previous.supersedes(seq, sample.monotonic, sample.wall)),
            None => self.sequence.is_none(),
        }
    }

    fn detector_observation_allows(&self, observed_at: Instant) -> bool {
        self.suppressed()
            .is_none_or(|released| observed_at > released.observed_at)
    }

    /// Generation/event table. Routing queries use shared methods; capacity,
    /// ordering and policy validation must succeed before a commit event.
    /// Rejected reports cannot evict records or change a generation.
    ///
    /// Open + anchored live report accepts; other reports park with an identity.
    /// AwaitingProcess + report parks, and + process requires a pending start.
    /// Cleared + report accepts only a different identified generation; process
    /// evidence reopens it without requiring a start. That distinction is why a
    /// hook clear and a process exit cannot share one boolean gate.
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
                        agent: released.agent,
                        session_ref,
                    }));
                }
                HookSourceEffects::Commit {
                    authority: AuthorityEffect::Set(authority),
                    persisted: None,
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
                if forget_retired {
                    self.transition(HookSourceEvent::Forget(session.agent, &session.session_ref));
                }
                if let Some(session_ref) = replaced {
                    self.transition(HookSourceEvent::Retire(StaleFullLifecycleHookSession {
                        agent: session.agent,
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
            (_, HookSourceEvent::ProcessObserved(now)) => {
                HookSourceEffects::ProcessObserved(self.observe_process(now))
            }
            (_, HookSourceEvent::RecordSequence(value, sample)) => {
                self.record_sequence(value, sample);
                HookSourceEffects::None
            }
            (_, HookSourceEvent::ClearSequence) => {
                self.clear_sequence();
                HookSourceEffects::None
            }
            (_, HookSourceEvent::Retire(session)) => {
                self.retire(session);
                HookSourceEffects::None
            }
            (_, HookSourceEvent::Forget(agent, session)) => {
                self.forget(agent, session);
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
        self.pending_start_at = None;
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

    fn forget(&mut self, agent: Agent, session: &crate::agent::resume::AgentSessionRef) {
        self.stale_sessions
            .retain(|stale| stale.agent != agent || &stale.session_ref != session);
    }

    fn park_start(
        &mut self,
        mut initial: SuppressedFullLifecycleHookReport,
        session: crate::agent::resume::PersistedAgentSession,
    ) {
        self.pending_start_at = Some(initial.observed_at);
        if self.suppressed().is_none() {
            // The start itself opens this suppression: no exit or clear bounds
            // it, so its detector floor must not be the start's own instant.
            // The process that sent the start existed before the hook did, and
            // a detector tick stamped before the hook was applied may be the
            // first to see it; refusing that tick loses the start until some
            // later state change republishes presence. Observations older
            // than the pane's last detector observation or its recorded exit
            // are refused before reaching the source record, so the only
            // floor left here is the attribution window itself.
            initial.observed_at = initial
                .observed_at
                .checked_sub(PARKED_START_LIFETIME)
                .unwrap_or(initial.observed_at);
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
        // A queued old exit cannot consume a start observed after that exit.
        if self
            .pending_start_at
            .is_some_and(|started_at| now <= started_at)
        {
            return;
        }
        self.pending_start_at = None;
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
                agent: suppressed.agent,
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
        now: Instant,
    ) -> Option<(
        crate::agent::resume::PersistedAgentSession,
        Option<PendingFullLifecycleHookReport>,
    )> {
        // An observation sampled before the start arrived still attributes it:
        // the detector stamps a tick when it begins, before its probe, and a
        // hook can be applied while that tick's event is queued. Such an
        // observation names the process that sent the start, and no further
        // presence transition follows for it. Refusing observations older
        // than the start would be wrong in principle, not only racy: the
        // process that sends a start hook necessarily existed before the hook
        // did, so its first presence sample may predate the start. The lower
        // bound that matters is the exit or clear that parked the start;
        // observations older than that are refused before this point
        // (`transition_detector_observation`). Exact attribution would need a
        // process id on both inputs, and hook reports carry none.
        if let Some(started_at) = self.pending_start_at
            && now.saturating_duration_since(started_at) > PARKED_START_LIFETIME
        {
            // Expiry discards the entire pending selection, including a
            // report for it. Presence may reopen a clear, but cannot
            // resurrect an expired exit-gated session.
            if let Some(released) = self.suppressed_mut() {
                released.pending_start = None;
                released.pending_replacement_report = None;
            }
            self.pending_start_at = None;
            self.sequence = None;
        }
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
                        agent: released.agent,
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
                self.pending_start_at = None;
                let stale = released
                    .session_ref
                    .as_ref()
                    .filter(|old| *old != &start.session_ref)
                    .map(|old| StaleFullLifecycleHookSession {
                        agent: start.agent,
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
                self.forget(start.agent, &start.session_ref);
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

use crate::limits::PARKED_START_LIFETIME;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SuppressedFullLifecycleHookReport {
    agent: Agent,
    session_ref: Option<crate::agent::resume::AgentSessionRef>,
    observed_at: Instant,
    pending_start: Option<crate::agent::resume::PersistedAgentSession>,
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
    Ignore(HookRejection),
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleFullLifecycleHookSession {
    agent: Agent,
    session_ref: crate::agent::resume::AgentSessionRef,
}

impl AgentOwnership {
    fn hook_authority_not_newer_than(&self, observed_at: Instant) -> bool {
        self.hook_authority
            .as_ref()
            .is_none_or(|authority| authority.reported_at <= observed_at)
    }

    fn hook_authority_conflicts_with_detected_agent(&self, detected_agent: Option<Agent>) -> bool {
        let Some(detected_agent) = detected_agent else {
            return false;
        };
        self.hook_authority
            .as_ref()
            .is_some_and(|authority| authority.origin.agent() != detected_agent)
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

    fn persisted_agent_session_matches(&self, origin: &ReportOrigin) -> bool {
        self.persisted_agent_session
            .as_ref()
            .is_some_and(|session| origin.owns(session))
    }

    fn suppress_current_full_lifecycle_hook_authority(
        &mut self,
        reason: FullLifecycleHookSuppressionReason,
        now: Instant,
    ) {
        if let Some((origin, session_ref)) = self.hook_authority.as_ref().and_then(|authority| {
            authority
                .origin
                .is_full_lifecycle()
                .then(|| (authority.origin, authority.session_ref.clone()))
        }) {
            self.suppress_full_lifecycle_hook_report_with_session_ref(
                &origin,
                session_ref,
                reason,
                now,
            );
        }
    }

    fn suppress_full_lifecycle_hook_report_with_session_ref(
        &mut self,
        origin: &ReportOrigin,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        reason: FullLifecycleHookSuppressionReason,
        observed_at: Instant,
    ) {
        // A release must retain its gate even if restored session ownership
        // has no record yet: dropping it would let late reports revive a
        // completed process.
        self.hook_sources
            .entry(*origin.source())
            .or_default()
            .transition(HookSourceEvent::Release(
                reason,
                SuppressedFullLifecycleHookReport {
                    agent: origin.agent(),
                    session_ref,
                    observed_at,
                    pending_start: None,
                    pending_replacement_report: None,
                },
            ));
    }

    fn route_full_lifecycle_hook_report(
        &mut self,
        origin: &ReportOrigin,
        state: AgentState,
        session_ref: &Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> FullLifecycleHookReportRoute {
        let reported_at = sample.monotonic;
        if !origin.is_full_lifecycle() {
            return FullLifecycleHookReportRoute::Accept {
                reanchor_sequence: false,
            };
        }
        let process_present =
            self.detected_agent == Some(origin.agent()) && self.process_evidence.exit().is_none();
        let anchored_session_ref = self
            .hook_authority
            .as_ref()
            .filter(|authority| &authority.origin == origin)
            .and_then(|authority| authority.session_ref.as_ref())
            .or_else(|| {
                self.persisted_agent_session
                    .as_ref()
                    .filter(|session| origin.owns(session))
                    .map(|session| &session.session_ref)
            });
        let authority_session_ref = self
            .hook_authority
            .as_ref()
            .filter(|authority| &authority.origin == origin)
            .and_then(|authority| authority.session_ref.as_ref());
        let empty_source = HookSourceState::default();
        let record = self
            .hook_sources
            .get(origin.source())
            .unwrap_or(&empty_source);
        let route = record.report_route(
            origin.agent(),
            session_ref,
            process_present,
            anchored_session_ref,
            authority_session_ref,
        );
        if route != FullLifecycleHookReportRoute::Pending {
            return route;
        }

        // Session-less state can update an anchored generation, but cannot
        // establish or reopen one: it cannot distinguish startup from a late
        // report belonging to a process that already exited.
        let Some(session_ref) = session_ref.clone() else {
            return FullLifecycleHookReportRoute::Ignore(HookRejection::MissingSession);
        };
        let Some(seq) = seq else {
            return FullLifecycleHookReportRoute::Ignore(HookRejection::MissingSequence);
        };
        let source = *origin.source();
        if !self.hook_report_order_allows(&source, Some(seq), sample) {
            return FullLifecycleHookReportRoute::Ignore(HookRejection::OutOfOrder);
        }
        let previous_session_ref = self
            .persisted_agent_session
            .as_ref()
            .filter(|session| origin.owns(session))
            .map(|session| session.session_ref.clone());
        let pending = PendingFullLifecycleHookReport {
            authority: HookAuthority {
                origin: *origin,
                state,
                reported_at,
                session_ref: Some(session_ref),
            },
            seq,
            sample,
        };
        let parked =
            self.hook_sources
                .entry(source)
                .or_default()
                .transition(HookSourceEvent::ParkReport(
                    SuppressedFullLifecycleHookReport {
                        agent: origin.agent(),
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
            FullLifecycleHookReportRoute::Ignore(HookRejection::OutOfOrder)
        }
    }

    fn same_owner_full_lifecycle_hook_authority_session_ref(
        &self,
        origin: &ReportOrigin,
        session_ref: &crate::agent::resume::AgentSessionRef,
    ) -> Option<crate::agent::resume::AgentSessionRef> {
        let authority = self.hook_authority.as_ref()?;
        if !authority.origin.is_full_lifecycle() || &authority.origin != origin {
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
        now: Instant,
    ) {
        let Some(detected_agent) = detected_agent else {
            return;
        };
        if previous_detected_agent == Some(detected_agent) {
            return;
        }
        let Some(origin) = ReportOrigin::official(detected_agent) else {
            return;
        };
        if !origin.is_full_lifecycle() {
            return;
        }
        let effect = self
            .hook_sources
            .get_mut(origin.source())
            .map(|record| record.transition(HookSourceEvent::ProcessObserved(now)));
        if let Some(effect) = effect {
            self.apply_source_effect(effect);
        }
    }

    fn detected_state_observed_before_release_suppression(
        &self,
        detected_agent: Option<Agent>,
        observed_at: Instant,
    ) -> bool {
        let Some(record) = detected_agent
            .and_then(ReportOrigin::official)
            .and_then(|origin| self.hook_sources.get(origin.source()))
        else {
            return false;
        };
        !record.detector_observation_allows(observed_at)
    }

    fn current_session_owner_conflicts(&self, origin: &ReportOrigin) -> bool {
        self.current_session_identity_for_persistence()
            .is_some_and(|current| !origin.owns(&current))
    }

    fn conflicting_same_owner_session_ref(
        &self,
        origin: &ReportOrigin,
        session_ref: &crate::agent::resume::AgentSessionRef,
        session_start_source: ReportedSessionStart,
    ) -> Option<crate::agent::resume::AgentSessionRef> {
        let current = self.current_session_identity_for_persistence()?;
        (origin.owns(&current)
            && current.session_ref.is_id()
            && session_ref.is_id()
            && &current.session_ref != session_ref
            && !origin.allows_session_replacement(session_start_source))
        .then_some(current.session_ref)
    }

    /// Only a known start confirms a start. An omitted source and one this
    /// build does not know are both unconfirmed: neither parks an ordered
    /// start nor confirms a takeover from another owner.
    fn session_start_source_is_recognized(session_start_source: ReportedSessionStart) -> bool {
        matches!(session_start_source, ReportedSessionStart::Known(_))
    }

    fn is_unsequenced_opencode_selection(
        origin: &ReportOrigin,
        session_start_source: ReportedSessionStart,
        seq: Option<u64>,
    ) -> bool {
        seq.is_none()
            && session_start_source == ReportedSessionStart::Known(AgentSessionStartSource::Select)
            && origin
                .agent()
                .descriptor()
                .hook_session_policy()
                .unsequenced_selection
    }
}

impl AgentOwnership {
    fn origin_conflicts_with_detected_agent(&self, origin: &ReportOrigin) -> bool {
        self.detected_agent
            .is_some_and(|detected| origin.agent() != detected)
    }

    fn foreground_agent_confirms_different_owner_takeover(
        &self,
        origin: &ReportOrigin,
        session_ref: &crate::agent::resume::AgentSessionRef,
        session_start_source: ReportedSessionStart,
    ) -> bool {
        origin
            .agent()
            .descriptor()
            .hook_session_policy()
            .foreground_takeover
            && Self::session_start_source_is_recognized(session_start_source)
            && self.foreground_agent_confirms_session_owner(origin, session_ref)
    }

    fn foreground_agent_confirms_hook_authority_takeover(
        &self,
        origin: &ReportOrigin,
        session_ref: &Option<crate::agent::resume::AgentSessionRef>,
    ) -> bool {
        session_ref.as_ref().is_some_and(|session_ref| {
            self.foreground_agent_confirms_session_owner(origin, session_ref)
        })
    }

    fn foreground_agent_confirms_session_owner(
        &self,
        origin: &ReportOrigin,
        session_ref: &crate::agent::resume::AgentSessionRef,
    ) -> bool {
        self.detected_agent == Some(origin.agent()) && origin.session(session_ref.clone()).is_some()
    }

    fn hook_report_order_allows(
        &self,
        source: &AgentSource,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> bool {
        let sample = sample.into();
        // Routing queries never insert a source or evict ordering history.
        let empty_source = HookSourceState::default();
        let record = self.hook_sources.get(source).unwrap_or(&empty_source);
        record.order_allows(seq, sample)
    }

    fn clear_hook_source_sequence(&mut self, source: &AgentSource) {
        if let Some(record) = self.hook_sources.get_mut(source) {
            record.transition(HookSourceEvent::ClearSequence);
        }
    }
}

#[cfg(test)]
impl AgentOwnership {
    fn clear_hook_report_sequence(&mut self, source: &str) {
        self.clear_hook_source_sequence(&AgentSource::parse(source).expect("bundled source"));
    }

    fn accept_hook_report_at(
        &mut self,
        source: &str,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> bool {
        let sample = sample.into();
        let source = AgentSource::parse(source).expect("bundled source");
        if !self.hook_report_order_allows(&source, seq, sample) {
            return false;
        }
        if let Some(seq) = seq {
            self.hook_sources
                .entry(source)
                .or_default()
                .transition(HookSourceEvent::RecordSequence(seq, sample));
        }
        true
    }
}

#[cfg(test)]
impl AgentOwnership {
    pub(super) fn check_hook_invariants(&self) {
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

    fn stale_sessions(&self) -> &[StaleFullLifecycleHookSession] {
        &self.stale_sessions
    }

    fn check_invariants(&self) {
        assert!(self.stale_sessions.len() <= MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE);
        if let Some(released) = self.suppressed() {
            assert!(
                released
                    .pending_start
                    .as_ref()
                    .is_none_or(|start| start.agent == released.agent)
            );
            assert!(
                released
                    .pending_replacement_report
                    .as_ref()
                    .is_none_or(|pending| {
                        pending.authority.origin.agent() == released.agent
                            && pending.authority.session_ref.is_some()
                    })
            );
        }
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;
    use crate::agent::resume::{AgentSessionRef, PersistedAgentSession};
    use std::time::Duration;

    fn sample() -> HookClockSample {
        // clock-io-ok: synthetic observation clock for the transition table.
        HookClockSample {
            monotonic: Instant::now(),
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        }
    }

    #[test]
    fn parked_start_expires_in_both_release_generations() {
        let clock = sample();
        for row in [1, 2] {
            let mut source = record(row, clock);
            source.transition(HookSourceEvent::ParkOrderedStart(
                release(clock),
                session("new"),
                20,
                clock,
            ));
            source.transition(HookSourceEvent::ParkReport(
                release(clock),
                report("new", 21, clock),
            ));
            let effect = source.transition(HookSourceEvent::ProcessObserved(
                clock.monotonic + PARKED_START_LIFETIME + Duration::from_nanos(1),
            ));
            assert!(matches!(effect, HookSourceEffects::ProcessObserved(None)));
            assert!(source.suppressed().is_none_or(|released| {
                released.pending_start.is_none() && released.pending_replacement_report.is_none()
            }));
            assert!(source.pending_start_at.is_none());
            assert!(source.sequence.is_none());
        }
    }

    #[test]
    fn parked_start_can_be_promoted_at_the_deadline() {
        let clock = sample();
        let mut source = record(1, clock);
        source.transition(HookSourceEvent::ParkStart(release(clock), session("new")));
        assert!(matches!(
            source.transition(HookSourceEvent::ProcessObserved(
                clock.monotonic + PARKED_START_LIFETIME,
            )),
            HookSourceEffects::ProcessObserved(Some((start, _))) if start == session("new")
        ));
        assert!(source.pending_start_at.is_none());
    }

    #[test]
    fn an_exit_before_a_parked_start_does_not_consume_it() {
        let clock = sample();
        for row in [1, 2] {
            let mut source = record(row, clock);
            source.transition(HookSourceEvent::ParkStart(release(clock), session("new")));
            let before = source.clone();
            source.transition(HookSourceEvent::ProcessExited(clock.monotonic));
            assert_eq!(source, before);
        }
    }

    #[test]
    fn presence_sampled_before_a_parked_start_still_promotes_it() {
        // The detector stamps a tick before probing, and the start can be
        // applied while that tick's event is queued.
        let clock = sample();
        for row in [1, 2] {
            let mut source = record(row, clock);
            source.transition(HookSourceEvent::ParkStart(release(clock), session("new")));
            assert!(matches!(
                source.transition(HookSourceEvent::ProcessObserved(
                    clock.monotonic - Duration::from_millis(1),
                )),
                HookSourceEffects::ProcessObserved(Some((start, _))) if start == session("new")
            ));
        }
    }

    #[test]
    fn replayed_detector_exit_does_not_consume_a_new_parked_start() {
        let clock = sample();
        let origin = ReportOrigin::official(Agent::Pi).expect("official Pi");
        let mut ownership = AgentOwnership::new();
        ownership.set_detected_agent_process_at(Agent::Pi, clock.monotonic);
        let exited_at = clock.monotonic + Duration::from_secs(1);
        ownership.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            exited_at,
        );
        let start_clock = HookClockSample {
            monotonic: exited_at + Duration::from_secs(1),
            wall: clock.wall + Duration::from_secs(2),
        };
        assert_eq!(
            ownership.report_session_start_outcome_at(
                &origin,
                Some(identity("new")),
                Some(20),
                ReportedSessionStart::Known(AgentSessionStartSource::Startup),
                start_clock,
            ),
            HookOutcome::Parked
        );
        let replay = ownership.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            exited_at,
        );
        assert_eq!(replay, AgentOwnershipMutation::default());
        ownership.set_detected_agent_process_at(
            Agent::Pi,
            start_clock.monotonic + Duration::from_millis(1),
        );
        assert_eq!(
            ownership.current_session_identity_for_persistence(),
            Some(session("new"))
        );
    }

    #[test]
    fn replayed_pane_exit_does_not_consume_a_late_parked_start() {
        let clock = sample();
        let mut ownership = AgentOwnership::new();
        ownership
            .set_pane_process_exit_at(shepr_platform::ChildExitReason::Exited, clock.monotonic);
        let origin = ReportOrigin::official(Agent::Pi).expect("official Pi");
        assert_eq!(
            ownership.report_session_start_outcome_at(
                &origin,
                Some(identity("new")),
                Some(20),
                ReportedSessionStart::Known(AgentSessionStartSource::Startup),
                HookClockSample {
                    monotonic: clock.monotonic + Duration::from_secs(1),
                    ..clock
                },
            ),
            HookOutcome::Parked
        );
        let before = ownership.hook_sources.get(origin.source()).cloned();
        ownership.set_pane_process_exit_at(
            shepr_platform::ChildExitReason::Exited,
            clock.monotonic + Duration::from_secs(2),
        );
        assert_eq!(ownership.hook_sources.get(origin.source()).cloned(), before);
    }

    #[test]
    fn first_presence_sampled_before_a_fresh_start_promotes_it() {
        // A fresh pane: no detector observation yet, so the start parks in an
        // open generation. The detector's tick that first sees the agent was
        // stamped before the start hook was applied.
        let clock = sample();
        let origin = ReportOrigin::official(Agent::Pi).expect("official Pi");
        let mut ownership = AgentOwnership::new();
        let start_clock = HookClockSample {
            monotonic: clock.monotonic + Duration::from_secs(1),
            ..clock
        };
        assert_eq!(
            ownership.report_session_start_outcome_at(
                &origin,
                Some(identity("new")),
                Some(20),
                ReportedSessionStart::Known(AgentSessionStartSource::Startup),
                start_clock,
            ),
            HookOutcome::Parked
        );
        ownership.set_detected_agent_process_at(
            Agent::Pi,
            start_clock.monotonic - Duration::from_millis(1),
        );
        assert_eq!(
            ownership.current_session_identity_for_persistence(),
            Some(session("new"))
        );
    }

    #[test]
    fn hook_outcomes_distinguish_missing_sequence_from_a_parked_start() {
        let clock = sample();
        let origin = ReportOrigin::official(Agent::Pi).expect("official Pi");
        let mut ownership = AgentOwnership::new();
        let source = ReportedSessionStart::Known(AgentSessionStartSource::Startup);
        assert_eq!(
            ownership.report_session_start_outcome_at(
                &origin,
                Some(identity("new")),
                None,
                source,
                clock,
            ),
            HookOutcome::Rejected(HookRejection::MissingSequence)
        );
        assert_eq!(
            ownership.report_session_start_outcome_at(
                &origin,
                Some(identity("new")),
                Some(20),
                source,
                clock,
            ),
            HookOutcome::Parked
        );
    }

    #[test]
    fn unchanged_state_is_applied_and_duplicate_sequence_is_rejected() {
        let clock = sample();
        let origin = ReportOrigin::official(Agent::Codex).expect("Codex integration");
        let mut ownership = AgentOwnership::new();
        assert!(matches!(
            ownership.report_hook_outcome_at(
                origin,
                AgentState::Idle,
                Some(identity("codex")),
                Some(20),
                clock,
            ),
            HookOutcome::Applied(_)
        ));
        assert_eq!(
            ownership.report_hook_outcome_at(
                origin,
                AgentState::Idle,
                Some(identity("codex")),
                Some(21),
                clock,
            ),
            HookOutcome::Applied(AgentOwnershipMutation::default())
        );
        assert_eq!(
            ownership.report_hook_outcome_at(
                origin,
                AgentState::Idle,
                Some(identity("codex")),
                Some(21),
                clock,
            ),
            HookOutcome::Rejected(HookRejection::OutOfOrder)
        );
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
            agent: Agent::Pi,
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
                Ignore(HookRejection::CrossTalk),
            ],
            [
                Ignore(HookRejection::LifecycleGate),
                Pending,
                Ignore(HookRejection::CrossTalk),
            ],
            [
                Ignore(HookRejection::LifecycleGate),
                Ignore(HookRejection::LifecycleGate),
                Ignore(HookRejection::CrossTalk),
            ],
        ];
        let clock = sample();
        let anchor = identity("old");
        for (row, expected) in rows.into_iter().enumerate() {
            for (incoming, expected) in [None, Some(identity("old")), Some(identity("new"))]
                .into_iter()
                .zip(expected)
            {
                let record = record(row, clock);
                let before = record.clone();
                let route = record.report_route(Agent::Pi, &incoming, true, Some(&anchor), None);
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
            let record = record(row, clock);
            let incoming = Some(identity("new"));
            let route = record.report_route(Agent::Pi, &incoming, false, Some(&anchor), None);
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
                        let record = record(row, clock);
                        let before = record.clone();
                        let route = record.start_route(
                            Agent::Pi,
                            process_present,
                            session_anchored,
                            unsequenced_selection,
                        );
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
                record.transition(HookSourceEvent::ProcessObserved(clock.monotonic))
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
                origin: ReportOrigin::parse("shepr:pi", "pi").expect("fixture origin"),
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
                record.transition(HookSourceEvent::ProcessObserved(clock.monotonic))
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
        });
        assert_eq!(record.sequence_value(), Some(1));
        assert!(record.suppressed().is_none());
        assert_eq!(record.stale_sessions()[0].session_ref, identity("old"));
        let mut terminal = AgentOwnership::new();
        terminal.apply_source_effect(effect);
        assert_eq!(terminal.hook_authority, Some(authority));
        assert!(terminal.persisted_agent_session.is_none());
    }

    #[test]
    fn start_commit_replaces_authority_and_revives_only_selected_identity() {
        let clock = sample();
        let mut record = HookSourceState::default();
        record.retire(StaleFullLifecycleHookSession {
            agent: Agent::Pi,
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
        let mut terminal = AgentOwnership::new();
        terminal.seed_hook_authority_for_test(Some(report("old", 20, clock).authority));
        terminal.apply_source_effect(effect);
        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.persisted_agent_session, Some(session("new")));
    }

    #[test]
    fn process_after_clear_installs_parked_start_through_the_public_entry_points() {
        let clock = sample();
        let mut terminal = AgentOwnership::new();
        terminal.set_persisted_agent_session(session("old"));
        terminal.set_detected_agent_process_at(Agent::Pi, clock.monotonic);
        terminal
            .set_hook_report_at(
                ReportOrigin::official(Agent::Pi).expect("Pi integration"),
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
        assert!(
            terminal.hook_sources[ReportOrigin::official(Agent::Pi)
                .expect("Pi integration")
                .source()]
            .suppressed()
            .is_some()
        );
        terminal
            .set_agent_session_ref_for_typed_start_source_at(
                ReportOrigin::official(Agent::Pi).expect("Pi integration"),
                Some(identity("new")),
                Some(20),
                ReportedSessionStart::Known(AgentSessionStartSource::Startup),
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
        assert!(
            terminal.hook_sources[ReportOrigin::official(Agent::Pi)
                .expect("Pi integration")
                .source()]
            .suppressed()
            .is_none()
        );
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
                        agent: Agent::Pi,
                        session_ref: identity("new"),
                    }));
                }
                let before = record.clone();
                let incoming = Some(identity("new"));
                let route = record.report_route(
                    Agent::Pi,
                    &incoming,
                    false,
                    None,
                    (!retired).then_some(&old),
                );
                assert_eq!(
                    route,
                    FullLifecycleHookReportRoute::Ignore(if retired {
                        HookRejection::RetiredSession
                    } else {
                        HookRejection::CrossTalk
                    })
                );
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
                assert_eq!(record.order_allows(seq, observed), accepted);
            }
            record.transition(HookSourceEvent::ClearSequence);
            assert!(record.order_allows(None, later));
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
                agent: Agent::Pi,
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
        record.transition(HookSourceEvent::Forget(Agent::Pi, &selected));
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
            record.transition(HookSourceEvent::ProcessObserved(clock.monotonic))
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
            record.transition(HookSourceEvent::ProcessObserved(clock.monotonic)),
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
                let record = record(row, clock);
                let observed = clock.monotonic + Duration::from_secs(offset);
                assert_eq!(
                    record.detector_observation_allows(observed),
                    row == 0 || offset > 0
                );
            }
        }
    }
}

#[cfg(test)]
impl AgentOwnership {
    fn suppressed_hook_source(&self, source: &str) -> Option<&SuppressedFullLifecycleHookReport> {
        self.hook_sources
            .get(&AgentSource::parse(source)?)?
            .suppressed()
    }

    pub fn set_hook_authority(
        &mut self,
        source: &str,
        agent_label: &str,
        state: AgentState,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.set_hook_authority_at(source, agent_label, state, None, seq, Instant::now())
            .and_then(|mutation| mutation.effective_state_change)
    }

    pub fn set_hook_authority_with_session_ref(
        &mut self,
        source: &str,
        agent_label: &str,
        state: AgentState,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
    ) -> Option<AgentOwnershipMutation> {
        self.set_hook_authority_at(source, agent_label, state, session_ref, seq, Instant::now())
    }
}

#[cfg(test)]
impl AgentOwnership {
    pub fn set_agent_session_ref(
        &mut self,
        source: &str,
        agent_label: &str,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
    ) -> Option<AgentOwnershipMutation> {
        self.set_agent_session_ref_at(
            ReportOrigin::parse(source, agent_label).ok()?,
            session_ref,
            seq,
            Instant::now(),
        )
    }

    pub fn set_agent_session_ref_for_session_start(
        &mut self,
        source: &str,
        agent_label: &str,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<&str>,
    ) -> Option<AgentOwnershipMutation> {
        self.set_agent_session_ref_for_typed_start_source_at(
            ReportOrigin::parse(source, agent_label).ok()?,
            session_ref,
            seq,
            ReportedSessionStart::from_wire(session_start_source),
            Instant::now(),
        )
    }
}

#[cfg(test)]
impl AgentOwnership {
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
    ) -> AgentOwnershipMutation {
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

    /// One detector observation, as the detector reports it: an exit once.
    fn confirmed_detection_for_test(
        &mut self,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> AgentOwnershipMutation {
        self.set_detected_state_with_screen_signals_at(
            agent,
            state,
            visible_blocker,
            process_exited,
            now,
        )
    }
}

#[cfg(test)]
mod pane_exit_tests {
    use super::*;
    use crate::agent::resume::{AgentSessionRef, PersistedAgentSession};
    use shepr_platform::ChildExitReason;

    fn running_terminal() -> AgentOwnership {
        let mut terminal = AgentOwnership::new();
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
            origin: ReportOrigin::parse("shepr:pi", "pi").expect("fixture origin"),
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

    const GRACE: std::time::Duration = crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE;

    /// The detector's exit report for Pi at `at`, then its withdrawal.
    fn pi_exits(terminal: &mut AgentOwnership, at: Instant) -> AgentOwnershipMutation {
        let release = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            at,
        );
        terminal.set_detected_state_with_screen_signals_at(
            None,
            AgentState::Unknown,
            false,
            false,
            at + std::time::Duration::from_millis(1),
        );
        release
    }

    #[test]
    fn an_agent_exit_under_a_live_shell_releases_at_once() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        let mutation = pi_exits(&mut terminal, now);
        assert!(mutation.agent_released);
        assert!(mutation.session_ref_changed);
        assert!(terminal.hook_authority.is_none());
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
        assert!(terminal.effective_agent().is_none());
        assert_eq!(terminal.detected_agent, None);
        // A shell death after the grace finds nothing to bring back.
        terminal.set_pane_process_exit_at(ChildExitReason::Interrupted, now + GRACE * 2);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
    }

    #[test]
    fn an_agent_killed_just_before_its_shell_keeps_the_checkpoint_identity() {
        for reason in [
            ChildExitReason::Interrupted,
            ChildExitReason::ReaderIoFailed,
            ChildExitReason::TerminalClosed,
        ] {
            let mut terminal = running_terminal();
            let session = terminal.current_session_identity_for_persistence();
            // clock-io-ok: synthetic observation times for kill ordering.
            let now = Instant::now();
            pi_exits(&mut terminal, now);
            let mutation = terminal.set_pane_process_exit_at(reason, now + GRACE / 2);
            assert_eq!(
                terminal.current_session_identity_for_persistence(),
                session,
                "{reason:?}"
            );
            // The saved identity changed back, so the session is dirty and no
            // older checkpoint can settle this exit.
            assert!(mutation.session_ref_changed, "{reason:?}");
            assert!(terminal.hook_authority.is_none(), "{reason:?}");
        }
    }

    #[test]
    fn a_detector_exit_after_the_pane_ended_keeps_the_held_identity() {
        let mut terminal = running_terminal();
        let session = terminal.current_session_identity_for_persistence();
        // clock-io-ok: synthetic reader failure and detector times.
        let now = Instant::now();
        // The reader failed while the child still ran: the pane's ending is
        // applied first, holding its identity for the checkpoint.
        terminal.set_pane_process_exit_at(ChildExitReason::ReaderIoFailed, now);
        assert_eq!(terminal.current_session_identity_for_persistence(), session);
        // A detector exit still queued for it changes nothing.
        let mutation = pi_exits(&mut terminal, now + std::time::Duration::from_millis(5));
        assert!(!mutation.session_ref_changed);
        assert_eq!(terminal.current_session_identity_for_persistence(), session);
    }

    #[test]
    fn a_normal_shell_exit_after_the_agent_brings_nothing_back() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic exit times.
        let now = Instant::now();
        pi_exits(&mut terminal, now);
        terminal.set_pane_process_exit_at(ChildExitReason::Exited, now + GRACE / 2);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
    }

    #[test]
    fn a_new_agent_process_discards_the_candidate() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        pi_exits(&mut terminal, now);
        terminal
            .set_detected_agent_process_at(Agent::Pi, now + std::time::Duration::from_millis(5));
        terminal.set_pane_process_exit_at(ChildExitReason::Interrupted, now + GRACE / 2);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none(),
            "the new process is not the one that exited"
        );
    }

    #[test]
    fn an_older_agent_observation_does_not_discard_the_candidate() {
        let mut terminal = running_terminal();
        let session = terminal.current_session_identity_for_persistence();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now() + std::time::Duration::from_secs(1);
        pi_exits(&mut terminal, now);
        terminal
            .set_detected_agent_process_at(Agent::Pi, now - std::time::Duration::from_millis(5));
        terminal.set_pane_process_exit_at(ChildExitReason::Interrupted, now + GRACE / 2);
        assert_eq!(terminal.current_session_identity_for_persistence(), session);
    }

    #[test]
    fn a_selection_after_the_release_discards_the_candidate() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        pi_exits(&mut terminal, now);
        let replacement = PersistedAgentSession::from_report(
            "shepr:pi",
            "pi",
            AgentSessionRef::id("replacement").expect("replacement identity"),
        )
        .expect("official identity");
        terminal.set_persisted_agent_session(replacement.clone());
        // The selection is cleared again; the released identity must not
        // come back in its place.
        terminal.apply_source_effect(HookSourceEffects::Commit {
            authority: AuthorityEffect::Keep,
            persisted: None,
        });
        terminal.set_pane_process_exit_at(ChildExitReason::Interrupted, now + GRACE / 2);
        assert!(
            terminal
                .current_session_identity_for_persistence()
                .is_none()
        );
    }

    #[test]
    fn a_dying_agents_late_session_start_leaves_no_ghost_authority() {
        let mut terminal = running_terminal();
        // clock-io-ok: synthetic detector tick times.
        let now = Instant::now();
        pi_exits(&mut terminal, now);
        // Pi's own `New` reaches the server after its exit was applied.
        terminal.set_agent_session_ref_for_session_start(
            "shepr:pi",
            "pi",
            Some(AgentSessionRef::id("late-new").expect("session id")),
            Some(99),
            Some("new"),
        );
        assert!(terminal.hook_authority.is_none());
        assert!(terminal.effective_agent().is_none());
        assert_eq!(terminal.state, AgentState::Unknown);
    }

    #[test]
    fn a_signal_shutdown_adopts_a_candidate_on_either_side_of_the_signal() {
        // (signal before the release, distance from it, adopted)
        for (before, distance, adopted) in [
            (true, GRACE / 2, true),
            (false, GRACE / 2, true),
            (false, GRACE * 2, false),
            (true, GRACE * 2, false),
        ] {
            let mut terminal = running_terminal();
            let session = terminal.current_session_identity_for_persistence();
            // clock-io-ok: synthetic detector and signal times.
            let now = Instant::now() + std::time::Duration::from_secs(10);
            pi_exits(&mut terminal, now);
            let signaled_at = if before {
                now - distance
            } else {
                now + distance
            };
            let case = format!("before={before} distance={distance:?}");
            assert_eq!(
                terminal.adopt_checkpoint_candidate_for_shutdown(signaled_at),
                adopted,
                "{case}"
            );
            let expected = if adopted { session } else { None };
            assert_eq!(
                terminal.current_session_identity_for_persistence(),
                expected,
                "{case}"
            );
        }
    }

    fn official_session(agent: &str, id: &str) -> PersistedAgentSession {
        PersistedAgentSession::from_report(
            &format!("shepr:{agent}"),
            agent,
            AgentSessionRef::id(id).expect("session id"),
        )
        .expect("official session")
    }

    #[test]
    fn a_sessionless_authority_clear_keeps_the_persisted_identity() {
        let mut terminal = AgentOwnership::new();
        // clock-io-ok: synthetic observation and report times.
        let now = Instant::now();
        terminal.set_detected_agent_process_at(Agent::Claude, now);
        let persisted = official_session("pi", "kept");
        terminal.set_persisted_agent_session(persisted.clone());
        terminal.seed_hook_authority_for_test(Some(HookAuthority {
            origin: ReportOrigin::parse("shepr:claude", "claude").expect("fixture origin"),
            state: AgentState::Working,
            reported_at: now,
            session_ref: None,
        }));
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Idle,
            false,
            false,
            now + std::time::Duration::from_millis(1),
        );
        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.persisted_agent_session(), Some(&persisted));
    }

    #[test]
    fn an_authority_clear_keeps_the_detected_agents_own_identity() {
        let mut terminal = AgentOwnership::new();
        // clock-io-ok: synthetic observation and report times.
        let now = Instant::now();
        terminal.set_detected_agent_process_at(Agent::Claude, now);
        // A parked Pi start promoted on detection, while Claude still holds
        // authority, leaves exactly these two slots.
        let pi = official_session("pi", "pi-session");
        terminal.set_persisted_agent_session(pi.clone());
        terminal.seed_hook_authority_for_test(Some(HookAuthority {
            origin: ReportOrigin::parse("shepr:claude", "claude").expect("fixture origin"),
            state: AgentState::Working,
            reported_at: now,
            session_ref: Some(AgentSessionRef::id("claude-session").expect("session id")),
        }));
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            false,
            now + std::time::Duration::from_millis(1),
        );
        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.persisted_agent_session(), Some(&pi));
    }

    #[test]
    fn an_authority_clear_away_from_its_agent_keeps_its_session() {
        let mut terminal = AgentOwnership::new();
        // clock-io-ok: synthetic observation and report times.
        let now = Instant::now();
        terminal.set_detected_agent_process_at(Agent::Claude, now);
        terminal.set_persisted_agent_session(official_session("claude", "older"));
        terminal.seed_hook_authority_for_test(Some(HookAuthority {
            origin: ReportOrigin::parse("shepr:claude", "claude").expect("fixture origin"),
            state: AgentState::Working,
            reported_at: now,
            session_ref: Some(AgentSessionRef::id("current").expect("session id")),
        }));
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Idle,
            false,
            false,
            now + std::time::Duration::from_millis(1),
        );
        assert!(terminal.hook_authority.is_none());
        assert_eq!(
            terminal.persisted_agent_session(),
            Some(&official_session("claude", "current"))
        );
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
