use super::*;

impl AgentOwnership {
    pub(in crate::ownership) fn transition_start(
        &mut self,
        origin: &ReportOrigin,
        session_ref: Option<crate::agent::resume::AgentSessionRef>,
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
        let source = origin.source().clone();
        let agent_label = origin.agent().clone();
        if self.known_agent_label_conflicts_with_detected_agent(origin) {
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
        let known_agent = origin.known_agent();
        let process_present = known_agent.is_some()
            && self.detected_agent == known_agent
            && self.process_evidence.exit().is_none();
        let full_lifecycle_source = origin.is_full_lifecycle();
        let session_anchored = self.hook_authority.as_ref().is_some_and(|authority| {
            authority.origin == *origin && authority.session_ref.is_some()
        }) || self.persisted_agent_session_matches(origin);
        let unsequenced_selection =
            Self::is_unsequenced_opencode_selection(origin, session_start_source, seq);
        let selection_can_reconcile = unsequenced_selection && process_present;
        let start_route = if full_lifecycle_source {
            let empty_source = HookSourceState::default();
            let record = self.hook_sources.get(&source).unwrap_or(&empty_source);
            record.start_route(
                &agent_label,
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
                        .map(|session| session.session_ref.clone())
                });
            if !self.hook_report_sequence_has_room(&source) || !self.prepare_hook_source(&source) {
                return HookOutcome::Rejected(HookRejection::SourceCapacity);
            }
            self.hook_sources
                .entry(source)
                .or_default()
                .transition(HookSourceEvent::ParkStart(
                    SuppressedFullLifecycleHookReport {
                        agent_label,
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
            if !self.hook_report_sequence_has_room(&source) {
                return HookOutcome::Rejected(HookRejection::SourceCapacity);
            }

            let previous_agent_label = self.effective_agent_label().map(str::to_string);
            let previous_state = self.state;
            let previous_session = self.current_session_identity_for_persistence();
            if !self.prepare_hook_source(&source) {
                return HookOutcome::Rejected(HookRejection::SourceCapacity);
            }
            self.hook_sources
                .entry(source.clone())
                .or_default()
                .transition(HookSourceEvent::ParkOrderedStart(
                    SuppressedFullLifecycleHookReport {
                        agent_label: agent_label.clone(),
                        session_ref: None,
                        observed_at: now,
                        pending_start: None,
                        pending_replacement_report: None,
                    },
                    persisted_session,
                    seq,
                    sample,
                ));

            if process_present {
                self.clear_full_lifecycle_hook_suppression_for_detected_agent(
                    None,
                    known_agent,
                    now,
                );
                let current_session = self.current_session_identity_for_persistence();
                return HookOutcome::Applied(AgentOwnershipMutation {
                    effective_state_change: self
                        .recompute_effective_state(previous_agent_label.as_deref(), previous_state),
                    session_ref_changed: previous_session != current_session,
                    agent_released: false,
                });
            }
            return HookOutcome::Parked;
        }
        let session_replacement_allowed = origin.allows_session_replacement(session_start_source);
        let Some(session_agent) = origin.official_agent() else {
            return HookOutcome::Rejected(HookRejection::UnsupportedOrigin);
        };
        let replacing_identity_only_session =
            session_agent
                .descriptor()
                .integration
                .is_some_and(|integration| {
                    integration.capability == crate::agent::IntegrationCapability::IdentityOnly
                })
                && session_replacement_allowed
                && self
                    .current_session_identity_for_persistence()
                    .is_some_and(|current| {
                        origin.owns(&current)
                            && current.session_ref.is_id()
                            && session_ref.is_id()
                            && current.session_ref != session_ref
                    });
        if replacing_identity_only_session && !process_present {
            return HookOutcome::Rejected(HookRejection::ProcessRequired);
        }
        if self
            .conflicting_same_owner_session_ref(origin, &session_ref, session_start_source)
            .is_some()
        {
            return HookOutcome::Rejected(HookRejection::ReplacedSession);
        }
        let replaced_hook_session =
            self.same_owner_full_lifecycle_hook_authority_session_ref(origin, &session_ref);
        // A refused replacement preserves the confirmed live authority and
        // identity. A different ref alone can be delayed cross-talk; releasing
        // authority here would let an unrecognized start withdraw a live agent.
        if replaced_hook_session.is_some() && !session_replacement_allowed {
            return HookOutcome::Rejected(HookRejection::ReplacedSession);
        }

        if !unsequenced_selection && !self.hook_report_order_allows(&source, seq, sample) {
            return HookOutcome::Rejected(HookRejection::OutOfOrder);
        }
        if seq.is_some()
            && (!self.hook_report_sequence_has_room(&source) || !self.prepare_hook_source(&source))
        {
            return HookOutcome::Rejected(HookRejection::SourceCapacity);
        }
        let selection = selection_can_reconcile.then(|| {
            self.current_session_identity_for_persistence()
                .is_some_and(|current| origin.owns(&current) && current.session_ref == session_ref)
        });
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
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
            effective_state_change: self
                .recompute_effective_state(previous_agent_label.as_deref(), previous_state),
            session_ref_changed: previous_session != current_session,
            agent_released: false,
        })
    }
}
