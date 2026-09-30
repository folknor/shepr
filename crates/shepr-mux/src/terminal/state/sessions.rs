use super::*;

impl TerminalState {
    pub fn set_persisted_agent_session(
        &mut self,
        session: shepr_agent::agent::resume::PersistedAgentSession,
    ) {
        self.persisted_agent_session = Some(session);
    }

    pub fn set_agent_session_ref_at(
        &mut self,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.set_agent_session_ref_for_typed_start_source_at(
            source,
            agent_label,
            session_ref,
            seq,
            None,
            sample,
        )
    }

    pub fn set_agent_session_ref_for_typed_start_source_at(
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
            && self.recent_agent_process_exit.is_none();
        let full_lifecycle_source =
            shepr_agent::detect::full_lifecycle_hook_authority(&source, &agent_label);
        let generation_gated = self
            .suppressed_hook_source(&source)
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
        if full_lifecycle_source && unsequenced_selection && !selection_can_reconcile {
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
            self.hook_sources.entry(source).or_default().park_start(
                SuppressedFullLifecycleHookReport {
                    agent_label,
                    session_ref: previous_session_ref,
                    observed_at: now,
                    reason: FullLifecycleHookSuppressionReason::AwaitingProcess,
                    pending_start: None,
                    pending_replacement_report: None,
                },
                persisted_session,
            );
            return Some(TerminalStateMutation::default());
        }
        if full_lifecycle_source
            && !selection_can_reconcile
            && (!process_present || generation_gated || !session_anchored)
        {
            if !Self::session_start_source_is_recognized(session_start_source) {
                return None;
            }
            let seq = seq?;
            if self.hook_seq_superseded(&source, seq, sample) {
                return None;
            }
            if !self.hook_report_sequence_has_room(&source) {
                return None;
            }

            let previous_agent_label = self.effective_agent_label().map(str::to_string);
            let previous_state = self.state;
            let previous_session = self.current_session_identity_for_persistence();
            // Capacity and ordering were validated above. Commit the sequence
            // before the pending start; no fallible step follows either write.
            if !self.record_hook_seq(source.clone(), seq, sample) {
                return None;
            }
            self.hook_sources
                .entry(source.clone())
                .or_default()
                .park_start(
                    SuppressedFullLifecycleHookReport {
                        agent_label: agent_label.clone(),
                        session_ref: None,
                        observed_at: now,
                        reason: FullLifecycleHookSuppressionReason::AwaitingProcess,
                        pending_start: None,
                        pending_replacement_report: None,
                    },
                    persisted_session,
                );

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

        if !unsequenced_selection && !self.accept_hook_report_at(&source, seq, sample) {
            return None;
        }
        if selection_can_reconcile {
            let new_generation = self.suppressed_hook_source(&source).is_some()
                || self
                    .current_session_identity_for_persistence()
                    .is_none_or(|current| {
                        current.source.as_str() != source
                            || current.agent.label() != agent_label
                            || current.session_ref != session_ref
                    });
            self.activate_hook_source(&source);
            // A trusted unsequenced selection starts one generation. Selecting
            // the current session again must not revive its older state reports.
            if new_generation {
                self.clear_hook_report_sequence(&source);
            }
        }
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_state = self.state;
        let previous_session = self.current_session_identity_for_persistence();
        // A recognized replacement start must not stay shadowed by the old
        // session's state authority, which otherwise keeps supplying its id
        // to persistence and rewrites subsequent reports back to that id.
        if session_replacement_allowed
            && self.hook_authority.as_ref().is_some_and(|authority| {
                authority.source == source
                    && authority.agent_label == agent_label
                    && authority
                        .session_ref
                        .as_ref()
                        .is_some_and(|current| current != &session_ref)
            })
        {
            self.hook_authority = None;
        }
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
                now,
            );
            self.hook_authority = None;
        }
        self.persisted_agent_session = Some(persisted_session);
        let current_session = self.current_session_identity_for_persistence();
        Some(TerminalStateMutation {
            effective_state_change: self
                .recompute_effective_state(previous_agent_label.as_deref(), previous_state),
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
        Agent::parse_canonical_label(agent_label)
            .is_some_and(|hook_agent| hook_agent != detected_agent)
    }

    pub(super) fn foreground_agent_confirms_different_owner_takeover(
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
        Agent::parse_canonical_label(agent_label) == Some(detected_agent)
            && shepr_agent::agent::resume::PersistedAgentSession::from_report(
                source,
                agent_label,
                session_ref.clone(),
            )
            .and_then(|session| shepr_agent::agent::resume::plan(&session))
            .is_some()
    }

    pub(super) fn hook_report_order_allows(
        &self,
        source: &str,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> bool {
        let sample = sample.into();
        seq.map_or_else(
            || {
                self.hook_sources
                    .get(source)
                    .is_none_or(|record| record.sequence_value().is_none())
            },
            |seq| !self.hook_seq_superseded(source, seq, sample),
        )
    }

    pub(super) fn accept_hook_report_at(
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

    /// Sequence numbers are wall-clock stamps, not monotonic observation times.
    /// Within one generation they must increase unless a backward step in the
    /// server's wall clock is corroborated by its monotonic clock. The caller supplies
    /// the same clock pair to validation and acceptance.
    /// Generation transitions also reset ordering. Mere silence never does.
    pub(super) fn hook_seq_superseded(
        &self,
        source: &str,
        seq: u64,
        sample: HookClockSample,
    ) -> bool {
        self.hook_sources
            .get(source)
            .is_some_and(|record| record.seq_superseded(seq, sample.monotonic, sample.wall))
    }

    pub(super) fn record_hook_seq(
        &mut self,
        source: String,
        seq: u64,
        sample: HookClockSample,
    ) -> bool {
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
            .record_sequence(seq, sample);
        true
    }

    /// Capacity validation never mutates. Unprotected records are evicted only
    /// when the validated report commits, so rejection preserves all ordering.
    pub(super) fn hook_report_sequence_has_room(&self, source: &str) -> bool {
        self.hook_sources.contains_key(source)
            || self.hook_sources.len() < MAX_HOOK_REPORT_SOURCES
            || self
                .hook_sources
                .iter()
                .any(|(source, record)| !self.hook_source_protected(source, record))
    }

    pub(super) fn hook_source_protected(&self, source: &str, record: &HookSourceState) -> bool {
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

    pub(super) fn prepare_hook_source(&mut self, source: &str) -> bool {
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

    pub(super) fn clear_hook_report_sequence(&mut self, source: &str) {
        if let Some(record) = self.hook_sources.get_mut(source) {
            record.clear_sequence();
        }
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
