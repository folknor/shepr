use super::*;

impl TerminalState {
    pub fn set_persisted_agent_session(
        &mut self,
        session: shepr_agent::agent::resume::PersistedAgentSession,
    ) {
        self.persisted_agent_session = Some(session);
    }

    pub fn set_managed_agent_launch_session(
        &mut self,
        session: shepr_agent::agent::resume::PersistedAgentSession,
    ) {
        self.persisted_agent_session = Some(session.clone());
        self.managed_agent_launch_session = Some(session);
    }

    pub fn set_agent_session_ref(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
    ) -> Option<TerminalStateMutation> {
        self.set_agent_session_ref_for_session_start(source, agent_label, session_ref, seq, None)
    }

    pub fn set_agent_session_ref_for_session_start(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<&str>,
    ) -> Option<TerminalStateMutation> {
        self.set_agent_session_ref_for_typed_start_source(
            source,
            agent_label,
            session_ref,
            seq,
            shepr_agent::agent::resume::normalize_session_start_source(session_start_source),
        )
    }

    pub fn set_agent_session_ref_for_typed_start_source(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<shepr_agent::agent::resume::AgentSessionStartSource>,
    ) -> Option<TerminalStateMutation> {
        self.warn_unrecognized_hook_identity(&source, &agent_label);
        let session_ref = session_ref?;
        let known_agent = shepr_agent::detect::parse_agent_label(&agent_label);
        let process_present = known_agent.is_some()
            && self.detected_agent == known_agent
            && self.recent_agent_process_exit.is_none();
        let full_lifecycle_source =
            shepr_agent::detect::full_lifecycle_hook_authority(&source, &agent_label);
        let generation_gated = self
            .suppressed_full_lifecycle_hook_reports
            .get(&source)
            .is_some_and(|suppressed| {
                suppressed.agent_label == agent_label
                    && suppressed.reason != FullLifecycleHookSuppressionReason::HookClear
            });
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
        if selection_can_reconcile {
            self.suppressed_full_lifecycle_hook_reports.remove(&source);
        } else if full_lifecycle_source && unsequenced_selection {
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
            let suppressed = self
                .suppressed_full_lifecycle_hook_reports
                .entry(source)
                .or_insert_with(|| SuppressedFullLifecycleHookReport {
                    agent_label,
                    session_ref: previous_session_ref,
                    observed_at: Instant::now(),
                    reason: FullLifecycleHookSuppressionReason::ProcessExit,
                    replacement_session_ref: None,
                    pending_replacement_report: None,
                });
            suppressed.replacement_session_ref = Some(session_ref);
            suppressed.pending_replacement_report = None;
            return None;
        }
        if full_lifecycle_source
            && !selection_can_reconcile
            && (!process_present || generation_gated || !session_anchored)
        {
            if !Self::session_start_source_is_recognized(session_start_source) {
                return None;
            }
            let seq = seq?;
            let now = Instant::now();
            if self.hook_seq_superseded(&source, seq, now) {
                return None;
            }
            if !self.hook_report_sequence_has_room(&source) {
                return None;
            }

            let previous_agent_label = self.effective_agent_label().map(str::to_string);
            let previous_known_agent = self.effective_known_agent();
            let previous_state = self.state;
            let previous_presentation = self.effective_presentation_at(now);
            let previous_session = self.current_session_identity_for_persistence();
            let suppressed = self
                .suppressed_full_lifecycle_hook_reports
                .entry(source.clone())
                .or_insert_with(|| SuppressedFullLifecycleHookReport {
                    agent_label: agent_label.clone(),
                    session_ref: None,
                    observed_at: now,
                    reason: FullLifecycleHookSuppressionReason::ProcessExit,
                    replacement_session_ref: None,
                    pending_replacement_report: None,
                });
            if suppressed.replacement_session_ref.as_ref() != Some(&session_ref) {
                if suppressed
                    .pending_replacement_report
                    .as_ref()
                    .is_some_and(|pending| {
                        pending.authority.session_ref.as_ref() != Some(&session_ref)
                    })
                {
                    suppressed.pending_replacement_report = None;
                }
                suppressed.replacement_session_ref = Some(session_ref);
            }
            if !self.record_hook_seq(source.clone(), seq, now) {
                return None;
            }

            if process_present {
                self.clear_full_lifecycle_hook_suppression_for_detected_agent(None, known_agent);
                let current_session = self.current_session_identity_for_persistence();
                return Some(TerminalStateMutation {
                    effective_state_change: self.recompute_effective_state(
                        previous_agent_label,
                        previous_known_agent,
                        previous_state,
                        previous_presentation,
                        now,
                    ),
                    session_ref_changed: previous_session != current_session,
                    agent_released: false,
                });
            }
            return None;
        }
        if !unsequenced_selection && !self.accept_hook_report(&source, seq) {
            return None;
        }
        if self.known_agent_label_conflicts_with_detected_agent(&agent_label) {
            return None;
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
        if replaced_hook_session.is_some() && !session_replacement_allowed {
            return None;
        }

        let now = Instant::now();
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_presentation = self.effective_presentation_at(now);
        let previous_session = self.current_session_identity_for_persistence();
        if session_replacement_allowed || foreground_takeover_allowed {
            self.forget_stale_full_lifecycle_hook_session(&source, &agent_label, &session_ref);
        }
        if let Some(replaced_hook_session) = replaced_hook_session {
            self.remember_stale_full_lifecycle_hook_session(
                source.clone(),
                agent_label.clone(),
                replaced_hook_session,
            );
            self.hook_authority = None;
        } else if foreground_takeover_allowed {
            self.suppress_current_full_lifecycle_hook_authority(
                FullLifecycleHookSuppressionReason::HookClear,
            );
            self.hook_authority = None;
        }
        self.reconcile_agent_name_owner(&agent_label, Some(&session_ref));
        let persisted_session = shepr_agent::agent::resume::PersistedAgentSession::from_report(
            &source,
            &agent_label,
            session_ref,
        )?;
        if self.managed_agent_launch_session.as_ref() == Some(&persisted_session) {
            self.managed_agent_launch_session = None;
        }
        self.persisted_agent_session = Some(persisted_session);
        let current_session = self.current_session_identity_for_persistence();
        Some(TerminalStateMutation {
            effective_state_change: self.recompute_effective_state(
                previous_agent_label,
                previous_known_agent,
                previous_state,
                previous_presentation,
                now,
            ),
            session_ref_changed: previous_session != current_session,
            agent_released: false,
        })
    }

    pub(super) fn known_agent_label_conflicts_with_detected_agent(
        &self,
        agent_label: &str,
    ) -> bool {
        let Some(detected_agent) = self.detected_agent else {
            return false;
        };
        shepr_agent::detect::parse_agent_label(agent_label)
            .is_some_and(|hook_agent| hook_agent != detected_agent)
    }

    pub(super) fn foreground_agent_confirms_different_owner_takeover(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
        session_start_source: Option<AgentSessionStartSource>,
    ) -> bool {
        (source, agent_label) != ("shepr:grok", "grok")
            && Self::session_start_source_is_recognized(session_start_source)
            && self.foreground_agent_confirms_session_owner(source, agent_label, session_ref)
    }

    pub(super) fn foreground_agent_confirms_hook_authority_takeover(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &Option<shepr_agent::agent::resume::AgentSessionRef>,
    ) -> bool {
        session_ref.as_ref().is_some_and(|session_ref| {
            self.foreground_agent_confirms_session_owner(source, agent_label, session_ref)
        })
    }

    pub(super) fn foreground_agent_confirms_session_owner(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
    ) -> bool {
        let Some(detected_agent) = self.detected_agent else {
            return false;
        };
        shepr_agent::detect::parse_agent_label(agent_label) == Some(detected_agent)
            && shepr_agent::agent::resume::PersistedAgentSession::from_report(
                source,
                agent_label,
                session_ref.clone(),
            )
            .and_then(|session| shepr_agent::agent::resume::plan(&session))
            .is_some()
    }

    pub(super) fn accept_hook_report(&mut self, source: &str, seq: Option<u64>) -> bool {
        self.accept_hook_report_at(source, seq, Instant::now())
    }

    pub(super) fn accept_hook_report_at(
        &mut self,
        source: &str,
        seq: Option<u64>,
        now: Instant,
    ) -> bool {
        let Some(seq) = seq else {
            return !self.hook_report_sequences.contains_key(source);
        };
        if self.hook_seq_superseded(source, seq, now) {
            return false;
        }
        self.record_hook_seq(source.to_string(), seq, now)
    }

    /// Whether `seq` from `source` is older than what was already accepted.
    /// See [`HOOK_SEQUENCE_REANCHOR_AFTER`] for why a non-increasing `seq`
    /// long after the last acceptance is not.
    pub(super) fn hook_seq_superseded(&self, source: &str, seq: u64, now: Instant) -> bool {
        let Some(last_seq) = self.hook_report_sequences.get(source) else {
            return false;
        };
        report_seq_superseded(
            *last_seq,
            self.hook_report_accepted_at.get(source).copied(),
            seq,
            now,
        )
    }

    pub(super) fn record_hook_seq(&mut self, source: String, seq: u64, now: Instant) -> bool {
        if !self.hook_report_sequence_has_room(&source) {
            tracing::debug!(
                source = %source,
                limit = MAX_HOOK_REPORT_SOURCES,
                "ignoring hook report from a new source: too many sources"
            );
            return false;
        }
        self.hook_report_accepted_at.insert(source.clone(), now);
        self.hook_report_sequences.insert(source, seq);
        true
    }

    /// Drop ordering marks that no longer protect a current hook, session,
    /// suppression, or stale-session record before refusing a new source.
    pub(super) fn hook_report_sequence_has_room(&mut self, source: &str) -> bool {
        if self.hook_report_sequences.contains_key(source)
            || self.hook_report_sequences.len() < MAX_HOOK_REPORT_SOURCES
        {
            return true;
        }

        let mut protected_sources = std::collections::HashSet::new();
        if let Some(authority) = &self.hook_authority {
            protected_sources.insert(authority.source.clone());
        }
        if let Some(session) = &self.persisted_agent_session {
            protected_sources.insert(session.source.as_str().to_owned());
        }
        protected_sources.extend(self.suppressed_full_lifecycle_hook_reports.keys().cloned());
        protected_sources.extend(self.stale_full_lifecycle_hook_sessions.keys().cloned());

        self.hook_report_sequences
            .retain(|known_source, _| protected_sources.contains(known_source));
        let sequences = &self.hook_report_sequences;
        self.hook_report_accepted_at
            .retain(|known_source, _| sequences.contains_key(known_source));
        self.hook_report_sequences.len() < MAX_HOOK_REPORT_SOURCES
    }

    pub(super) fn clear_hook_report_sequence(&mut self, source: &str) {
        self.hook_report_sequences.remove(source);
        self.hook_report_accepted_at.remove(source);
    }
}
