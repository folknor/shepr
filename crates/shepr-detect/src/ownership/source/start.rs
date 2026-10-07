use super::*;

impl AgentOwnership {
    pub(in crate::ownership) fn transition_start(
        &mut self,
        origin: &ReportOrigin,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: ReportedSessionStart,
        sample: impl Into<HookClockSample>,
    ) -> HookOutcome {
        let sample = sample.into();
        let now = sample.monotonic;
        let Some(session_ref) = session_ref else {
            return HookOutcome::Rejected(HookRejection::MissingSession);
        };
        // The reducer validates references even for non-API callers.
        let Some(persisted_session) = origin.session(session_ref.clone()) else {
            return HookOutcome::Rejected(HookRejection::InvalidSession);
        };
        let source = *origin.source();
        let agent = origin.agent();
        if self.origin_conflicts_with_detected_agent(origin) {
            return HookOutcome::Rejected(HookRejection::DetectedAgentConflict);
        }
        let owner_conflicts = self.current_session_owner_conflicts(origin);
        let foreground_takeover_allowed = owner_conflicts
            && self.foreground_agent_confirms_different_owner_takeover(
                origin,
                &session_ref,
                session_start_source,
            );
        if owner_conflicts && !foreground_takeover_allowed {
            return HookOutcome::Rejected(HookRejection::OwnerConflict);
        }
        let process_present =
            self.detected_agent == Some(agent) && self.process_evidence.exit().is_none();
        let full_lifecycle_source = origin.is_full_lifecycle();
        let session_anchored = self.hook_authority.as_ref().is_some_and(|authority| {
            authority.origin == *origin && authority.session_ref.is_some()
        }) || self.persisted_agent_session_matches(origin);
        let unsequenced_selection =
            Self::is_unsequenced_selection(origin, session_start_source, seq);
        let selection_can_reconcile = unsequenced_selection && process_present;
        let start_route = if full_lifecycle_source {
            let empty_source = HookSourceState::default();
            let record = self.hook_sources.get(&source).unwrap_or(&empty_source);
            record.start_route(
                agent,
                process_present,
                session_anchored,
                unsequenced_selection,
            )
        } else {
            HookStartRoute::Commit
        };
        if start_route == HookStartRoute::ParkSelection {
            let previous_session_ref = self
                .hook_authority
                .as_ref()
                .filter(|authority| authority.origin == *origin)
                .and_then(|authority| authority.session_ref.clone())
                .or_else(|| {
                    self.persisted_agent_session
                        .as_ref()
                        .filter(|session| origin.owns(session))
                        .map(|session| session.session_ref().clone())
                });
            self.hook_sources
                .entry(source)
                .or_default()
                .transition(HookSourceEvent::ParkStart(
                    SuppressedFullLifecycleHookReport {
                        agent,
                        session_ref: previous_session_ref,
                        observed_at: now,
                        pending_start: None,
                        pending_replacement_report: None,
                    },
                    persisted_session,
                ));
            return HookOutcome::Parked;
        }
        if start_route == HookStartRoute::ParkRecognizedStart {
            if !Self::session_start_source_is_recognized(session_start_source) {
                return HookOutcome::Rejected(HookRejection::UnrecognizedStart);
            }
            let Some(seq) = seq else {
                return HookOutcome::Rejected(HookRejection::MissingSequence);
            };
            if !self.hook_report_order_allows(&source, Some(seq), sample) {
                return HookOutcome::Rejected(HookRejection::OutOfOrder);
            }

            let previous_agent = self.effective_agent();
            let previous_state = self.state;
            let previous_session = self.current_session_identity_for_persistence();
            self.hook_sources.entry(source).or_default().transition(
                HookSourceEvent::ParkOrderedStart(
                    SuppressedFullLifecycleHookReport {
                        agent,
                        session_ref: None,
                        observed_at: now,
                        pending_start: None,
                        pending_replacement_report: None,
                    },
                    persisted_session,
                    seq,
                    sample,
                ),
            );

            if process_present {
                self.clear_full_lifecycle_hook_suppression_for_detected_agent(
                    None,
                    Some(agent),
                    now,
                );
                let current_session = self.current_session_identity_for_persistence();
                return HookOutcome::Applied(AgentOwnershipMutation {
                    effective_state_change: self
                        .recompute_effective_state(previous_agent, previous_state),
                    session_ref_changed: previous_session != current_session,
                    agent_released: false,
                });
            }
            return HookOutcome::Parked;
        }
        let session_replacement_allowed = origin.allows_session_replacement(session_start_source);
        let replacing_identity_only_session =
            agent.descriptor().integration.is_some_and(|integration| {
                integration.capability == shepr_agent::IntegrationCapability::IdentityOnly
            }) && session_replacement_allowed
                && self
                    .current_session_identity_for_persistence()
                    .is_some_and(|current| {
                        origin.owns(&current)
                            && current.session_ref().is_id()
                            && session_ref.is_id()
                            && current.session_ref() != &session_ref
                    });
        if replacing_identity_only_session && !process_present {
            return HookOutcome::Rejected(HookRejection::ProcessRequired);
        }
        if self
            .conflicting_same_owner_session_ref(origin, &session_ref, session_start_source)
            .is_some()
        {
            return self.refuse_replaced_session_start(
                origin,
                persisted_session,
                seq,
                session_start_source,
                sample,
                process_present,
            );
        }
        let replaced_hook_session =
            self.same_owner_full_lifecycle_hook_authority_session_ref(origin, &session_ref);
        // A refused replacement preserves the confirmed live authority and
        // identity. A different ref alone can be delayed cross-talk; releasing
        // authority here would let an unrecognized start withdraw a live agent.
        if replaced_hook_session.is_some() && !session_replacement_allowed {
            return self.refuse_replaced_session_start(
                origin,
                persisted_session,
                seq,
                session_start_source,
                sample,
                process_present,
            );
        }

        if !unsequenced_selection && !self.hook_report_order_allows(&source, seq, sample) {
            return HookOutcome::Rejected(HookRejection::OutOfOrder);
        }
        let selection = selection_can_reconcile.then(|| {
            self.current_session_identity_for_persistence()
                .is_some_and(|current| {
                    origin.owns(&current) && current.session_ref() == &session_ref
                })
        });
        let previous_agent = self.effective_agent();
        let previous_state = self.state;
        let previous_session = self.current_session_identity_for_persistence();
        let replacement_clears_authority = session_replacement_allowed
            && self.hook_authority.as_ref().is_some_and(|authority| {
                authority.origin == *origin
                    && authority
                        .session_ref
                        .as_ref()
                        .is_some_and(|current| current != &session_ref)
            });
        if replaced_hook_session.is_none() && foreground_takeover_allowed {
            self.suppress_current_full_lifecycle_hook_authority(
                FullLifecycleHookSuppressionReason::HookClear,
                now,
            );
        }
        let needs_record = seq.is_some()
            || replaced_hook_session.is_some()
            || self.hook_sources.contains_key(&source);
        let event = HookSourceEvent::CommitStart {
            seq,
            sample,
            selection,
            session: persisted_session,
            clear_authority: replacement_clears_authority
                || replaced_hook_session.is_some()
                || foreground_takeover_allowed,
            replaced: replaced_hook_session,
            forget_retired: session_replacement_allowed || foreground_takeover_allowed,
        };
        let effect = if needs_record {
            self.hook_sources
                .entry(source)
                .or_default()
                .transition(event)
        } else {
            // An unsequenced selection with no ordering or retired identity
            // needs only the pane output, not a new source history record.
            HookSourceState::default().transition(event)
        };
        self.apply_source_effect(effect);
        // A committed start is a selection, even of the same identity again.
        self.checkpoint_candidate = None;
        let current_session = self.current_session_identity_for_persistence();
        HookOutcome::Applied(AgentOwnershipMutation {
            effective_state_change: self.recompute_effective_state(previous_agent, previous_state),
            session_ref_changed: previous_session != current_session,
            agent_released: false,
        })
    }

    /// A start that would replace this owner's session without a permitted
    /// replacement source. A `startup` that arrives while the detector holds
    /// a live process of its agent may be that process's relaunch, which the
    /// detector has not probed yet: it is held (`ReplacementStart`) rather
    /// than dropped, and reads as parked. Any other such start is refused.
    /// The held start has not been ordered yet; one its source's ordering
    /// already refuses is refused here, as it would be after the exit.
    fn refuse_replaced_session_start(
        &mut self,
        origin: &ReportOrigin,
        session: shepr_agent::resume::PersistedAgentSession,
        seq: Option<u64>,
        session_start_source: ReportedSessionStart,
        sample: HookClockSample,
        process_present: bool,
    ) -> HookOutcome {
        let relaunch_candidate = process_present
            && session_start_source
                == ReportedSessionStart::Known(AgentSessionStartSource::Startup)
            && self.hook_report_order_allows(origin.source(), seq, sample);
        if !relaunch_candidate {
            return HookOutcome::Rejected(HookRejection::ReplacedSession);
        }
        self.replacement_start = Some(ReplacementStart {
            origin: *origin,
            session,
            seq,
            session_start_source,
            received: sample,
            exit_observed_at: None,
        });
        HookOutcome::Parked
    }
}
