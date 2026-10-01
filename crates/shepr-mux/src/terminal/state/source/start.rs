use super::*;

impl TerminalState {
    pub(super) fn transition_start(
        &mut self,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<shepr_agent::agent::resume::AgentSessionStartSource>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        let sample = sample.into();
        let now = sample.monotonic;
        let typed_source = source;
        let source = typed_source.to_source_string();
        self.warn_unrecognized_hook_identity(&source, &agent_label);
        // Built-in source names cannot claim another agent. Custom sources
        // retain arbitrary labels, but cannot mint official resume identities.
        if typed_source
            .agent()
            .is_some_and(|agent| agent.label() != agent_label)
        {
            return None;
        }
        let session_ref = session_ref?;
        // Policy validation belongs here too: callers need not have used the
        // API constructor, and a rejected reference cannot alter authority.
        let persisted_session = shepr_agent::agent::resume::PersistedAgentSession::new(
            typed_source.clone(),
            typed_source
                .agent()
                .or_else(|| Agent::parse_canonical_label(&agent_label))?,
            session_ref.clone(),
        )?;
        if self.known_agent_label_conflicts_with_detected_agent(&agent_label) {
            return None;
        }
        let owner_conflicts = self.current_session_owner_conflicts(&source, &agent_label);
        let foreground_takeover_allowed = owner_conflicts
            && self.foreground_agent_confirms_different_owner_takeover(
                &source,
                &agent_label,
                &session_ref,
                session_start_source,
            );
        if owner_conflicts && !foreground_takeover_allowed {
            return None;
        }
        let known_agent = Agent::parse_canonical_label(&agent_label);
        let process_present = known_agent.is_some()
            && self.detected_agent == known_agent
            && self.process_evidence.exit().is_none();
        let full_lifecycle_source =
            shepr_agent::detect::full_lifecycle_hook_authority(&source, &agent_label);
        let session_anchored = self.hook_authority.as_ref().is_some_and(|authority| {
            authority.source == source
                && authority.agent_label == agent_label
                && authority.session_ref.is_some()
        }) || self.persisted_agent_session_matches(&source, &agent_label);
        let unsequenced_selection = Self::is_unsequenced_opencode_selection(
            &source,
            &agent_label,
            session_start_source,
            seq,
        );
        let selection_can_reconcile = unsequenced_selection && process_present;
        let start_route = if full_lifecycle_source {
            let mut empty_source = HookSourceState::default();
            let record = self
                .hook_sources
                .get_mut(&source)
                .unwrap_or(&mut empty_source);
            match record.transition(HookSourceEvent::Start {
                agent_label: &agent_label,
                process_present,
                session_anchored,
                unsequenced_selection,
            }) {
                HookSourceEffects::Start(route) => route,
                _ => return None,
            }
        } else {
            HookStartRoute::Commit
        };
        if start_route == HookStartRoute::ParkSelection {
            let previous_session_ref = self
                .hook_authority
                .as_ref()
                .filter(|authority| {
                    authority.source == source && authority.agent_label == agent_label
                })
                .and_then(|authority| authority.session_ref.clone())
                .or_else(|| {
                    self.persisted_agent_session
                        .as_ref()
                        .filter(|session| {
                            session.source.as_str() == source
                                && session.agent.label() == agent_label
                        })
                        .map(|session| session.session_ref.clone())
                });
            if !self.hook_report_sequence_has_room(&source) || !self.prepare_hook_source(&source) {
                return None;
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
            return Some(TerminalStateMutation::default());
        }
        if start_route == HookStartRoute::ParkRecognizedStart {
            if !Self::session_start_source_is_recognized(session_start_source) {
                return None;
            }
            let seq = seq?;
            if !self.hook_report_order_allows(&source, Some(seq), sample) {
                return None;
            }
            if !self.hook_report_sequence_has_room(&source) {
                return None;
            }

            let previous_agent_label = self.effective_agent_label().map(str::to_string);
            let previous_state = self.state;
            let previous_session = self.current_session_identity_for_persistence();
            if !self.prepare_hook_source(&source) {
                return None;
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
                self.clear_full_lifecycle_hook_suppression_for_detected_agent(None, known_agent);
                let current_session = self.current_session_identity_for_persistence();
                return Some(TerminalStateMutation {
                    effective_state_change: self
                        .recompute_effective_state(previous_agent_label.as_deref(), previous_state),
                    session_ref_changed: previous_session != current_session,
                    agent_released: false,
                });
            }
            return Some(TerminalStateMutation::default());
        }
        let session_replacement_allowed = Self::session_report_allows_session_replacement(
            &source,
            &agent_label,
            session_start_source,
        );
        let session_owner = shepr_agent::agent::AgentSource::from_pair(&source, &agent_label)?;
        let session_agent = session_owner.agent()?;
        let replacing_identity_only_session =
            shepr_agent::detect::session_identity_only_integration(&source, &agent_label)
                && session_replacement_allowed
                && self
                    .current_session_identity_for_persistence()
                    .is_some_and(|current| {
                        current.source == session_owner
                            && current.agent == session_agent
                            && current.session_ref.kind()
                                == shepr_agent::agent::resume::AgentSessionRefKind::Id
                            && session_ref.kind()
                                == shepr_agent::agent::resume::AgentSessionRefKind::Id
                            && current.session_ref != session_ref
                    });
        if replacing_identity_only_session && !process_present {
            return None;
        }
        if self
            .conflicting_same_owner_session_ref(
                &source,
                &agent_label,
                &session_ref,
                session_start_source,
            )
            .is_some()
        {
            return None;
        }
        let replaced_hook_session = self.same_owner_full_lifecycle_hook_authority_session_ref(
            &source,
            &agent_label,
            &session_ref,
        );
        // A refused replacement preserves the confirmed live authority and
        // identity. A different ref alone can be delayed cross-talk; releasing
        // authority here would let an unrecognized start withdraw a live agent.
        if replaced_hook_session.is_some() && !session_replacement_allowed {
            return None;
        }

        if (!unsequenced_selection && !self.hook_report_order_allows(&source, seq, sample))
            || (seq.is_some()
                && (!self.hook_report_sequence_has_room(&source)
                    || !self.prepare_hook_source(&source)))
        {
            return None;
        }
        let selection = selection_can_reconcile.then(|| {
            self.current_session_identity_for_persistence()
                .is_some_and(|current| {
                    current.source.as_str() == source
                        && current.agent.label() == agent_label
                        && current.session_ref == session_ref
                })
        });
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_state = self.state;
        let previous_session = self.current_session_identity_for_persistence();
        let replacement_clears_authority = session_replacement_allowed
            && self.hook_authority.as_ref().is_some_and(|authority| {
                authority.source == source
                    && authority.agent_label == agent_label
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
        let current_session = self.current_session_identity_for_persistence();
        Some(TerminalStateMutation {
            effective_state_change: self
                .recompute_effective_state(previous_agent_label.as_deref(), previous_state),
            session_ref_changed: previous_session != current_session,
            agent_released: false,
        })
    }
}
