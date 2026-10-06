use super::*;

/// A report's description, taken before arbitration consumes its parts.
struct UnappliedHookReportDraft {
    origin: ReportOrigin,
    kind: HookReportKind,
    seq: Option<u64>,
    session_ref: Option<shepr_agent::resume::AgentSessionRef>,
    received: HookClockSample,
}

impl AgentOwnership {
    pub fn set_hook_report_at(
        &mut self,
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> Option<AgentOwnershipMutation> {
        self.transition_hook_event(HookEvent::Report {
            origin,
            state,
            session_ref,
            seq,
            sample,
        })
    }

    pub fn report_hook_outcome_at(
        &mut self,
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> HookOutcome {
        let outcome = self.admit_state_report(origin, state, session_ref, seq, sample);
        self.check_hook_invariants();
        outcome
    }

    /// A state report through arbitration, with its outcome recorded.
    pub(super) fn admit_state_report(
        &mut self,
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> HookOutcome {
        let report = UnappliedHookReportDraft {
            origin,
            kind: HookReportKind::State(state),
            seq,
            session_ref: session_ref.clone(),
            received: sample,
        };
        let outcome = self.transition_report(origin, state, session_ref, seq, sample);
        self.record_hook_outcome(report, &outcome);
        outcome
    }

    /// A session start report through arbitration, with its outcome recorded.
    pub(super) fn admit_session_start(
        &mut self,
        origin: &ReportOrigin,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: ReportedSessionStart,
        sample: HookClockSample,
    ) -> HookOutcome {
        let report = UnappliedHookReportDraft {
            origin: *origin,
            kind: HookReportKind::SessionStart(session_start_source),
            seq,
            session_ref: session_ref.clone(),
            received: sample,
        };
        let outcome = self.transition_start(origin, session_ref, seq, session_start_source, sample);
        self.record_hook_outcome(report, &outcome);
        outcome
    }

    /// The last parked or rejected report, while no later report from its
    /// source has applied, as it stands at `now` on the server's monotonic
    /// clock.
    ///
    /// A parked record is judged here against what its source still holds
    /// (`HookSourceState::parked_awaiting`), not against the outcome it was
    /// admitted with. Several transitions drop a parked report or start
    /// without touching the record: a process exit consumes both, a start for
    /// another session discards a report for a different one, and a parked
    /// start's expiry is only applied when process evidence for its agent
    /// arrives. Each of those leaves the record reading as gone here, exactly
    /// as the source now stands, so a report that can no longer be promoted
    /// is never shown as parked.
    pub fn last_unapplied_hook_report(&self, now: Instant) -> Option<UnappliedHookReport> {
        let last = self.last_unapplied_hook_report.as_ref()?;
        let disposition = match last.rejection {
            Some(reason) => UnappliedHookDisposition::Rejected(reason),
            None => UnappliedHookDisposition::Parked(
                self.hook_sources
                    .get(last.origin.source())?
                    .parked_awaiting(last.kind, last.seq, last.session_ref.as_ref(), now)?,
            ),
        };
        Some(UnappliedHookReport {
            origin: last.origin,
            kind: last.kind,
            seq: last.seq,
            session_ref: last.session_ref.clone(),
            received: last.received,
            disposition,
        })
    }

    /// An unapplied outcome replaces the record; an applied one clears a
    /// record of the same source, whose report the new one supersedes.
    fn record_hook_outcome(&mut self, report: UnappliedHookReportDraft, outcome: &HookOutcome) {
        let rejection = match outcome {
            HookOutcome::Applied(_) => {
                if self
                    .last_unapplied_hook_report
                    .as_ref()
                    .is_some_and(|last| last.origin.source() == report.origin.source())
                {
                    self.last_unapplied_hook_report = None;
                }
                return;
            }
            HookOutcome::Parked => None,
            HookOutcome::Rejected(reason) => Some(*reason),
        };
        self.last_unapplied_hook_report = Some(UnappliedHookRecord {
            origin: report.origin,
            kind: report.kind,
            seq: report.seq,
            session_ref: report.session_ref,
            received: report.received,
            rejection,
        });
    }

    /// Process evidence promoted a parked report or start of `source`: the
    /// parked record it left is resolved.
    pub(super) fn resolve_parked_hook_report(&mut self, source: &shepr_agent::AgentSource) {
        if self
            .last_unapplied_hook_report
            .as_ref()
            .is_some_and(|last| last.rejection.is_none() && last.origin.source() == source)
        {
            self.last_unapplied_hook_report = None;
        }
    }

    pub fn current_session_identity_for_persistence(
        &self,
    ) -> Option<shepr_agent::resume::PersistedAgentSession> {
        if let Some(authority) = self.hook_authority.as_ref()
            && let Some(session_ref) = authority.session_ref.as_ref()
            && let Some(session) = authority.origin.session(session_ref.clone())
        {
            return Some(session);
        }
        self.persisted_agent_session.clone()
    }
}

#[cfg(test)]
impl AgentOwnership {
    /// Convenience seam for fixtures, taking the source as a string. The event
    /// reducer uses the typed report entry point.
    pub fn set_hook_authority_at(
        &mut self,
        source: &str,
        state: AgentState,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<AgentOwnershipMutation> {
        self.set_hook_report_at(
            ReportOrigin::parse(source).ok()?,
            state,
            session_ref,
            seq,
            sample.into(),
        )
    }
}
