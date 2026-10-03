use super::*;

impl AgentOwnership {
    pub(in crate::ownership) fn transition_report(
        &mut self,
        origin: ReportOrigin,
        state: AgentState,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> HookOutcome {
        let now = sample.monotonic;
        // All official session-only integrations use the same admission path.
        // A state report may contribute its session, but never state authority.
        if !origin.authority_class().admits_state_report() {
            return self.transition_start(
                &origin,
                session_ref,
                seq,
                ReportedSessionStart::Omitted,
                sample,
            );
        }
        let policy = origin.agent().descriptor().hook_session_policy();
        // Preserve the stricter session contract enforced by bundled hook assets.
        if policy.state_requires_session_ref && session_ref.is_none() {
            return HookOutcome::Rejected(HookRejection::MissingSession);
        }
        if let Some(session_ref) = session_ref.as_ref()
            && origin.session(session_ref.clone()).is_none()
        {
            return HookOutcome::Rejected(HookRejection::InvalidSession);
        }
        let source = *origin.source();
        // Codex turn reports carry the id of the session they belong to. One
        // for another session than the current one (a late Stop from a session
        // that /new or /resume replaced) must not overwrite this session's state.
        if policy.state_requires_current_session
            && let Some(incoming) = session_ref.as_ref()
            && self
                .current_session_identity_for_persistence()
                .is_some_and(|current| origin.owns(&current) && current.session_ref() != incoming)
        {
            return HookOutcome::Rejected(HookRejection::ReplacedSession);
        }
        if !origin.is_full_lifecycle()
            && self
                .process_evidence
                .exit()
                .is_some_and(|exit| origin.agent() == exit.agent)
        {
            return HookOutcome::Rejected(HookRejection::ProcessExited);
        }
        if self.origin_conflicts_with_detected_agent(&origin) {
            return HookOutcome::Rejected(HookRejection::DetectedAgentConflict);
        }
        let owner_conflicts = self.current_session_owner_conflicts(&origin);
        let foreground_takeover_allowed = owner_conflicts
            && self.foreground_agent_confirms_hook_authority_takeover(&origin, &session_ref);
        if owner_conflicts && !foreground_takeover_allowed {
            return HookOutcome::Rejected(HookRejection::OwnerConflict);
        }
        // Absence of a session ref means "state for the current generation",
        // never "forget the session". Only a same-owner anchor may be inherited.
        let session_ref = session_ref.or_else(|| {
            self.current_session_identity_for_persistence()
                .filter(|session| origin.owns(session))
                .map(|session| session.session_ref().clone())
        });
        let session_ref = session_ref.map(|session_ref| {
            if origin.is_full_lifecycle() {
                session_ref
            } else {
                self.conflicting_same_owner_session_ref(
                    &origin,
                    &session_ref,
                    ReportedSessionStart::Omitted,
                )
                .unwrap_or(session_ref)
            }
        });
        let reanchor_sequence = match self.route_full_lifecycle_hook_report(
            &origin,
            state,
            &session_ref,
            seq,
            sample,
        ) {
            FullLifecycleHookReportRoute::Accept { reanchor_sequence } => reanchor_sequence,
            FullLifecycleHookReportRoute::Ignore(reason) => return HookOutcome::Rejected(reason),
            FullLifecycleHookReportRoute::Pending => return HookOutcome::Parked,
        };
        if !reanchor_sequence && !self.hook_report_order_allows(&source, seq, sample) {
            return HookOutcome::Rejected(HookRejection::OutOfOrder);
        }

        let previous_agent = self.effective_agent();
        let previous_state = self.state;
        let previous_session = self.current_session_identity_for_persistence();
        if foreground_takeover_allowed {
            self.suppress_current_full_lifecycle_hook_authority(
                FullLifecycleHookSuppressionReason::HookClear,
                now,
            );
        }
        let event = HookSourceEvent::CommitReport {
            authority: HookAuthority {
                origin,
                state,
                reported_at: now,
                session_ref,
            },
            seq,
            sample,
            reanchor: reanchor_sequence,
        };
        let effect = if seq.is_some() || self.hook_sources.contains_key(&source) {
            self.hook_sources
                .entry(source)
                .or_default()
                .transition(event)
        } else {
            HookSourceState::default().transition(event)
        };
        self.apply_source_effect(effect);
        // A committed report naming a session (its own or one inherited
        // above) is a selection, even of the same identity again.
        if self
            .hook_authority
            .as_ref()
            .is_some_and(|authority| authority.session_ref.is_some())
        {
            self.checkpoint_candidate = None;
        }
        let current_session = self.current_session_identity_for_persistence();
        let effective_state_change = self.recompute_effective_state(previous_agent, previous_state);
        HookOutcome::Applied(AgentOwnershipMutation {
            effective_state_change,
            session_ref_changed: previous_session != current_session,
            agent_released: false,
        })
    }
}
