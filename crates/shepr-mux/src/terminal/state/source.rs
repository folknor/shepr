use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HookSequence {
    value: u64,
    accepted_at: Instant,
    accepted_wall_clock: SystemTime,
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
/// Under test, every event checks that release reasons match their
/// generation, parked payloads belong to that generation's agent, and retired
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
pub(super) enum HookSourceEvent<'a> {
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
    Release(SuppressedFullLifecycleHookReport),
    Activate,
    Select {
        current_session_matches: bool,
    },
    DetectorObservation(Instant),
    ParkStart(
        SuppressedFullLifecycleHookReport,
        shepr_agent::agent::resume::PersistedAgentSession,
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
pub(super) enum HookStartRoute {
    Commit,
    ParkSelection,
    ParkRecognizedStart,
}

pub(super) enum HookSourceEffects {
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
    /// Pane-wide owner selection and agent policy stay at those entry points,
    /// so integration fixes can be ported without rewriting generation storage.
    pub(super) fn transition(&mut self, event: HookSourceEvent<'_>) -> HookSourceEffects {
        let effects = match (&self.generation, event) {
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
            (_, HookSourceEvent::Release(report)) => {
                self.release(report);
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

    pub(super) fn stale_sessions(&self) -> &[StaleFullLifecycleHookSession] {
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

    fn release(&mut self, report: SuppressedFullLifecycleHookReport) {
        self.generation = match report.reason {
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

    fn park_report(
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

    fn process_exited(&mut self, now: Instant) {
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
    pub(super) sample: HookClockSample,
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

#[cfg(test)]
impl HookSourceState {
    pub(super) fn sequence_value(&self) -> Option<u64> {
        self.sequence.map(|sequence| sequence.value)
    }

    fn check_invariants(&self) {
        assert!(self.stale_sessions.len() <= MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE);
        match &self.generation {
            HookGeneration::Open => {}
            HookGeneration::AwaitingProcess(released) => {
                assert_eq!(
                    released.reason,
                    FullLifecycleHookSuppressionReason::AwaitingProcess
                );
            }
            HookGeneration::Cleared(released) => {
                assert_eq!(
                    released.reason,
                    FullLifecycleHookSuppressionReason::HookClear
                );
            }
        }
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
mod tests {
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
        PersistedAgentSession::from_report("shepr:claude", "claude", identity(id))
            .expect("official test session")
    }

    fn release(
        reason: FullLifecycleHookSuppressionReason,
        sample: HookClockSample,
    ) -> SuppressedFullLifecycleHookReport {
        SuppressedFullLifecycleHookReport {
            agent_label: "claude".into(),
            session_ref: Some(identity("old")),
            observed_at: sample.monotonic,
            reason,
            pending_start: None,
            pending_replacement_report: None,
        }
    }

    fn record(row: usize, sample: HookClockSample) -> HookSourceState {
        let mut record = HookSourceState::default();
        match row {
            0 => {}
            1 => {
                record.transition(HookSourceEvent::Release(release(
                    FullLifecycleHookSuppressionReason::AwaitingProcess,
                    sample,
                )));
            }
            2 => {
                record.transition(HookSourceEvent::Release(release(
                    FullLifecycleHookSuppressionReason::HookClear,
                    sample,
                )));
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
                    agent_label: "claude",
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
                agent_label: "claude",
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
                                agent_label: "claude",
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
                source: "shepr:claude".into(),
                agent_label: "claude".into(),
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
            record.transition(HookSourceEvent::ParkStart(
                release(FullLifecycleHookSuppressionReason::AwaitingProcess, clock),
                session("new"),
            ));
            record.transition(HookSourceEvent::ParkReport(
                release(FullLifecycleHookSuppressionReason::AwaitingProcess, clock),
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
        }
    }

    #[test]
    fn exit_table_consumes_pending_identity_without_reopening() {
        let clock = sample();
        for row in 0..3 {
            let mut record = record(row, clock);
            record.transition(HookSourceEvent::RecordSequence(20, clock));
            if row == 1 {
                record.transition(HookSourceEvent::ParkStart(
                    release(FullLifecycleHookSuppressionReason::AwaitingProcess, clock),
                    session("new"),
                ));
                record.transition(HookSourceEvent::ParkReport(
                    release(FullLifecycleHookSuppressionReason::AwaitingProcess, clock),
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
            release(FullLifecycleHookSuppressionReason::AwaitingProcess, clock),
            report("new", 21, clock),
        ));
        for id in ["new", "other"] {
            record.transition(HookSourceEvent::ParkStart(
                release(FullLifecycleHookSuppressionReason::AwaitingProcess, clock),
                session(id),
            ));
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
                        agent_label: "claude".into(),
                        session_ref: identity("new"),
                    }));
                }
                let before = record.clone();
                let incoming = Some(identity("new"));
                let HookSourceEffects::Report(route) = record.transition(HookSourceEvent::Report {
                    agent_label: "claude",
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
                    release(FullLifecycleHookSuppressionReason::AwaitingProcess, clock),
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
                released.reason,
                if row == 2 {
                    FullLifecycleHookSuppressionReason::HookClear
                } else {
                    FullLifecycleHookSuppressionReason::AwaitingProcess
                }
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
                agent_label: "claude".into(),
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
        record.transition(HookSourceEvent::Forget("claude", &selected));
        assert!(
            !record
                .stale_sessions()
                .iter()
                .any(|retired| retired.session_ref == selected)
        );
    }

    #[test]
    fn cleared_process_observation_discards_parked_start_without_installing_it() {
        let clock = sample();
        let mut record = record(2, clock);
        record.transition(HookSourceEvent::ParkStart(
            release(FullLifecycleHookSuppressionReason::AwaitingProcess, clock),
            session("new"),
        ));
        // A hook clear retains its distinct generation when a start is parked.
        // Process observation reopens that generation and retires its old id;
        // it does not install the parked start as an exit-gated observation does.
        let HookSourceEffects::ProcessObserved(activation) =
            record.transition(HookSourceEvent::ProcessObserved)
        else {
            panic!("process effect")
        };
        assert!(activation.is_none());
        assert!(record.suppressed().is_none());
        assert_eq!(record.stale_sessions()[0].session_ref, identity("old"));
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
