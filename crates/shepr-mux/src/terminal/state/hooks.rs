use super::*;

impl TerminalState {
    pub fn set_hook_report_at(
        &mut self,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: HookClockSample,
    ) -> Option<TerminalStateMutation> {
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
        if typed_source
            .agent()
            .is_some_and(|agent| agent.descriptor().session_identity_only_integration)
        {
            return None;
        }
        if let Some(session_ref) = session_ref.as_ref()
            && typed_source.agent().is_some_and(|agent| {
                shepr_agent::agent::resume::PersistedAgentSession::new(
                    typed_source.clone(),
                    agent,
                    session_ref.clone(),
                )
                .is_none()
            })
        {
            return None;
        }
        // Codex turn reports carry the id of the session they belong to. One
        // for another session than the current one (a late Stop from a session
        // that /new or /resume replaced) must not overwrite this session's state.
        if typed_source.agent().is_some_and(|agent| {
            agent
                .descriptor()
                .hook_session_policy
                .state_requires_current_session
        }) && let Some(incoming) = session_ref.as_ref()
            && self
                .current_session_identity_for_persistence()
                .is_some_and(|current| {
                    current.source.as_str() == source
                        && current.agent.label() == agent_label
                        && &current.session_ref != incoming
                })
        {
            return None;
        }
        if !shepr_agent::detect::full_lifecycle_hook_authority(&source, &agent_label)
            && self
                .recent_agent_process_exit
                .is_some_and(|exit| Agent::parse_canonical_label(&agent_label) == Some(exit.agent))
        {
            return None;
        }
        if self.known_agent_label_conflicts_with_detected_agent(&agent_label) {
            return None;
        }
        let custom_state_report = session_ref.is_none() && typed_source.agent().is_none();
        // A sessionless custom report updates state but cannot claim the
        // resume identity already stored for this pane.
        let owner_conflicts =
            !custom_state_report && self.current_session_owner_conflicts(&source, &agent_label);
        let foreground_takeover_allowed = owner_conflicts
            && self.foreground_agent_confirms_hook_authority_takeover(
                &source,
                &agent_label,
                &session_ref,
            );
        if owner_conflicts && !foreground_takeover_allowed {
            return None;
        }
        // Absence of a session ref means "state for the current generation",
        // never "forget the session". Only a same-owner anchor may be inherited.
        let session_ref = session_ref.or_else(|| {
            if custom_state_report {
                return None;
            }
            self.current_session_identity_for_persistence()
                .filter(|session| {
                    session.source.as_str() == source && session.agent.label() == agent_label
                })
                .map(|session| session.session_ref)
        });
        let session_ref = session_ref.map(|session_ref| {
            if shepr_agent::detect::full_lifecycle_hook_authority(&source, &agent_label) {
                session_ref
            } else {
                self.conflicting_same_owner_session_ref(&source, &agent_label, &session_ref, None)
                    .unwrap_or(session_ref)
            }
        });
        if self.live_full_lifecycle_hook_authority_conflicts_with_session(
            &source,
            &agent_label,
            &session_ref,
        ) {
            return None;
        }
        let reanchor_sequence = match self.route_full_lifecycle_hook_report(
            &source,
            &agent_label,
            state,
            &session_ref,
            seq,
            sample,
        ) {
            FullLifecycleHookReportRoute::Accept { reanchor_sequence } => reanchor_sequence,
            FullLifecycleHookReportRoute::Ignore => return None,
            FullLifecycleHookReportRoute::Pending => return Some(TerminalStateMutation::default()),
        };
        if !self.hook_report_sequence_has_room(&source)
            || (!reanchor_sequence && !self.hook_report_order_allows(&source, seq, sample))
        {
            return None;
        }
        if reanchor_sequence {
            self.clear_hook_report_sequence(&source);
        }
        if !self.accept_hook_report_at(&source, seq, sample) {
            return None;
        }

        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_state = self.state;
        let previous_session = self.current_session_identity_for_persistence();
        if foreground_takeover_allowed {
            self.suppress_current_full_lifecycle_hook_authority(
                FullLifecycleHookSuppressionReason::HookClear,
                now,
            );
        }
        if (session_ref.is_some() || reanchor_sequence)
            && let Some(suppressed) = self.activate_hook_source(&source)
            && let Some(suppressed_ref) = suppressed.session_ref
        {
            self.remember_stale_full_lifecycle_hook_session(
                source.clone(),
                suppressed.agent_label,
                suppressed_ref,
            );
        }
        if custom_state_report {
            // A custom state report cannot replace the session identity used
            // for resume, even when the previous authority held that identity.
            self.persisted_agent_session = previous_session.clone();
        } else {
            self.persisted_agent_session = None;
        }
        self.hook_authority = Some(HookAuthority {
            source,
            agent_label,
            state,
            reported_at: now,
            session_ref,
        });
        let current_session = self.current_session_identity_for_persistence();
        let effective_state_change =
            self.recompute_effective_state(previous_agent_label.as_deref(), previous_state);
        Some(TerminalStateMutation {
            effective_state_change,
            session_ref_changed: previous_session != current_session,
            agent_released: false,
        })
    }

    pub(super) fn warn_unrecognized_hook_identity(&self, source: &str, agent_label: &str) {
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

    pub(super) fn hook_authority_not_newer_than(&self, observed_at: Instant) -> bool {
        self.hook_authority
            .as_ref()
            .is_none_or(|authority| authority.reported_at <= observed_at)
    }

    pub(super) fn fallback_not_older_than_hook(&self) -> bool {
        self.hook_authority.as_ref().is_none_or(|authority| {
            self.fallback_observed_at
                .is_some_and(|observed_at| authority.reported_at <= observed_at)
        })
    }

    pub(super) fn hook_authority_conflicts_with_detected_agent(
        &self,
        detected_agent: Option<Agent>,
    ) -> bool {
        let Some(detected_agent) = detected_agent else {
            return false;
        };
        self.hook_authority.as_ref().is_some_and(|authority| {
            Agent::parse_canonical_label(&authority.agent_label)
                .is_some_and(|hook_agent| hook_agent != detected_agent)
        })
    }

    pub(super) fn should_ignore_detected_state_under_full_lifecycle_hook(
        &self,
        detected_agent: Option<Agent>,
        process_exited: bool,
    ) -> bool {
        self.live_full_lifecycle_hook_authority()
            && !process_exited
            && !self.hook_authority_conflicts_with_detected_agent(detected_agent)
    }

    pub(super) fn persisted_agent_session_matches(&self, source: &str, agent: &str) -> bool {
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

    pub(super) fn suppress_current_full_lifecycle_hook_authority(
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

    pub(super) fn suppress_full_lifecycle_hook_report_with_session_ref(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        reason: FullLifecycleHookSuppressionReason,
        observed_at: Instant,
    ) {
        self.hook_sources
            .entry(source)
            .or_default()
            .release(SuppressedFullLifecycleHookReport {
                agent_label,
                session_ref,
                observed_at,
                reason,
                pending_start: None,
                pending_replacement_report: None,
            });
    }

    pub(super) fn route_full_lifecycle_hook_report(
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
        if self.full_lifecycle_hook_report_matches_stale_session(source, agent_label, session_ref) {
            return FullLifecycleHookReportRoute::Ignore;
        }

        let known_agent = Agent::parse_canonical_label(agent_label);
        let process_present = known_agent.is_some()
            && self.detected_agent == known_agent
            && self.recent_agent_process_exit.is_none();
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
        let session_anchored = anchored_session_ref.is_some_and(|anchored| {
            session_ref
                .as_ref()
                .is_none_or(|incoming| incoming == anchored)
        });
        // A live state report cannot switch the session generation. Session
        // starts reconcile replacements through the session-report path.
        let live_session_cross_talk = process_present
            && anchored_session_ref
                .zip(session_ref.as_ref())
                .is_some_and(|(anchored, incoming)| anchored != incoming);
        if live_session_cross_talk {
            return FullLifecycleHookReportRoute::Ignore;
        }
        if let Some(suppressed) = self.suppressed_hook_source(source) {
            if suppressed.agent_label != agent_label {
                return FullLifecycleHookReportRoute::Ignore;
            }
            if suppressed.reason == FullLifecycleHookSuppressionReason::HookClear {
                let reanchor_sequence = matches!(
                    (&suppressed.session_ref, session_ref),
                    (Some(previous), Some(incoming)) if previous != incoming
                );
                return if reanchor_sequence {
                    FullLifecycleHookReportRoute::Accept {
                        reanchor_sequence: true,
                    }
                } else {
                    FullLifecycleHookReportRoute::Ignore
                };
            }
        }

        if process_present && session_anchored && !self.suppressed_hook_source(source).is_some() {
            return FullLifecycleHookReportRoute::Accept {
                reanchor_sequence: false,
            };
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
        if self.hook_seq_superseded(source, seq, sample) {
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
            .park_report(
                SuppressedFullLifecycleHookReport {
                    agent_label: agent_label.to_string(),
                    session_ref: previous_session_ref,
                    observed_at: reported_at,
                    reason: FullLifecycleHookSuppressionReason::AwaitingProcess,
                    pending_start: None,
                    pending_replacement_report: None,
                },
                pending,
            );
        if parked {
            FullLifecycleHookReportRoute::Pending
        } else {
            FullLifecycleHookReportRoute::Ignore
        }
    }

    pub(super) fn full_lifecycle_hook_report_matches_stale_session(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &Option<shepr_agent::agent::resume::AgentSessionRef>,
    ) -> bool {
        if !shepr_agent::detect::full_lifecycle_hook_authority(source, agent_label) {
            return false;
        }
        self.hook_sources
            .get(source)
            .map(HookSourceState::stale_sessions)
            .is_some_and(|stale_sessions| {
                session_ref.as_ref().is_some_and(|incoming_ref| {
                    stale_sessions.iter().any(|stale| {
                        stale.agent_label == agent_label && incoming_ref == &stale.session_ref
                    })
                })
            })
    }

    pub(super) fn live_full_lifecycle_hook_authority_conflicts_with_session(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &Option<shepr_agent::agent::resume::AgentSessionRef>,
    ) -> bool {
        let Some(authority) = self.hook_authority.as_ref() else {
            return false;
        };
        if !shepr_agent::detect::full_lifecycle_hook_authority(
            &authority.source,
            &authority.agent_label,
        ) {
            return false;
        }
        if authority.source != source || authority.agent_label != agent_label {
            return false;
        }
        authority
            .session_ref
            .as_ref()
            .zip(session_ref.as_ref())
            .is_some_and(|(current, incoming)| current != incoming)
    }

    pub(super) fn same_owner_full_lifecycle_hook_authority_session_ref(
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

    pub(super) fn clear_full_lifecycle_hook_suppression_for_detected_agent(
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
        let activation = self
            .hook_sources
            .get_mut(source)
            .and_then(HookSourceState::observe_process);
        let Some((persisted_session, pending)) = activation else {
            return;
        };
        self.persisted_agent_session = Some(persisted_session);
        if let Some(pending) = pending
            && self.record_hook_seq(source.to_owned(), pending.seq, pending.sample)
        {
            self.hook_authority = Some(pending.authority);
        }
    }

    pub(super) fn remember_stale_full_lifecycle_hook_session(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: shepr_agent::agent::resume::AgentSessionRef,
    ) {
        self.hook_sources
            .entry(source)
            .or_default()
            .retire(StaleFullLifecycleHookSession {
                agent_label,
                session_ref,
            });
    }

    pub(super) fn forget_stale_full_lifecycle_hook_session(
        &mut self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
    ) {
        if let Some(record) = self.hook_sources.get_mut(source) {
            record.forget(agent_label, session_ref);
        }
    }

    pub(super) fn suppressed_hook_source(
        &self,
        source: &str,
    ) -> Option<&SuppressedFullLifecycleHookReport> {
        self.hook_sources.get(source)?.suppressed()
    }

    pub(super) fn activate_hook_source(
        &mut self,
        source: &str,
    ) -> Option<SuppressedFullLifecycleHookReport> {
        self.hook_sources.get_mut(source)?.activate()
    }

    pub(super) fn detected_state_observed_before_release_suppression(
        &self,
        detected_agent: Option<Agent>,
        observed_at: Instant,
    ) -> bool {
        let Some(source) = detected_agent.and_then(Agent::integration_source) else {
            return false;
        };
        self.suppressed_hook_source(source)
            .is_some_and(|suppressed| observed_at <= suppressed.observed_at)
    }

    pub fn current_session_identity_for_persistence(
        &self,
    ) -> Option<shepr_agent::agent::resume::PersistedAgentSession> {
        if let Some(authority) = self.hook_authority.as_ref()
            && let Some(session_ref) = authority.session_ref.as_ref()
            && let Some(session) = shepr_agent::agent::resume::PersistedAgentSession::from_report(
                &authority.source,
                &authority.agent_label,
                session_ref.clone(),
            )
        {
            return Some(session);
        }
        self.persisted_agent_session.clone()
    }

    pub(super) fn current_session_owner_conflicts(&self, source: &str, agent_label: &str) -> bool {
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

    pub(super) fn conflicting_same_owner_session_ref(
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

    pub(super) fn session_report_allows_session_replacement(
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

    pub(super) fn session_start_source_is_recognized(
        session_start_source: Option<AgentSessionStartSource>,
    ) -> bool {
        session_start_source.is_some()
    }

    pub(super) fn is_unsequenced_opencode_selection(
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
    /// Convenience seam for fixtures, taking the source as a string. The event
    /// reducer uses the typed report entry point.
    pub fn set_hook_authority_at(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        sample: impl Into<HookClockSample>,
    ) -> Option<TerminalStateMutation> {
        self.set_hook_report_at(
            source.into(),
            agent_label,
            state,
            session_ref,
            seq,
            sample.into(),
        )
    }
}

#[cfg(test)]
impl TerminalState {
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
