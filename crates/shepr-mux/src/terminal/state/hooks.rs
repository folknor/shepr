use super::*;

impl TerminalState {
    pub fn set_hook_authority_at(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        now: Instant,
    ) -> Option<TerminalStateMutation> {
        self.warn_unrecognized_hook_identity(&source, &agent_label);
        if shepr_agent::detect::session_identity_only_integration(&source, &agent_label) {
            return None;
        }
        if !shepr_agent::detect::full_lifecycle_hook_authority(&source, &agent_label)
            && self.recent_agent_process_exit.is_some_and(|exit| {
                shepr_agent::detect::parse_agent_label(&agent_label) == Some(exit.agent)
            })
        {
            return None;
        }
        let reanchor_sequence = match self.route_full_lifecycle_hook_report(
            &source,
            &agent_label,
            state,
            message.as_deref(),
            &session_ref,
            seq,
            now,
        ) {
            FullLifecycleHookReportRoute::Accept { reanchor_sequence } => reanchor_sequence,
            FullLifecycleHookReportRoute::Ignore => return None,
        };
        if self.known_agent_label_conflicts_with_detected_agent(&agent_label) {
            return None;
        }
        let owner_conflicts = self.current_session_owner_conflicts(&source, &agent_label);
        let foreground_takeover_allowed = owner_conflicts
            && self.foreground_agent_confirms_hook_authority_takeover(
                &source,
                &agent_label,
                &session_ref,
            );
        if owner_conflicts && !foreground_takeover_allowed {
            return None;
        }
        let session_ref = session_ref.map(|session_ref| {
            if self.lifecycle_hook_report_replaces_persisted_session(
                &source,
                &agent_label,
                &session_ref,
            ) {
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
        if reanchor_sequence {
            self.clear_hook_report_sequence(&source);
        }
        if !self.accept_hook_report_at(&source, seq, now) {
            return None;
        }

        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_session = self.current_session_identity_for_persistence();
        if foreground_takeover_allowed {
            self.suppress_current_full_lifecycle_hook_authority(
                FullLifecycleHookSuppressionReason::HookClear,
                now,
            );
        }
        if (session_ref.is_some() || reanchor_sequence)
            && let Some(suppressed) = self.suppressed_full_lifecycle_hook_reports.remove(&source)
            && let Some(suppressed_ref) = suppressed.session_ref
        {
            self.remember_stale_full_lifecycle_hook_session(
                source.clone(),
                suppressed.agent_label,
                suppressed_ref,
            );
        }
        self.persisted_agent_session = None;
        self.hook_authority = Some(HookAuthority {
            source,
            agent_label,
            state,
            message,
            reported_at: now,
            session_ref,
        });
        let current_session = self.current_session_identity_for_persistence();
        let effective_state_change = self.recompute_effective_state(
            previous_agent_label,
            previous_known_agent,
            previous_state,
        );
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
            shepr_agent::detect::parse_agent_label(&authority.agent_label)
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
        self.suppressed_full_lifecycle_hook_reports.insert(
            source,
            SuppressedFullLifecycleHookReport {
                agent_label,
                session_ref,
                observed_at,
                reason,
                replacement_session_ref: None,
                pending_replacement_report: None,
            },
        );
    }

    pub(super) fn route_full_lifecycle_hook_report(
        &mut self,
        source: &str,
        agent_label: &str,
        state: AgentState,
        message: Option<&str>,
        session_ref: &Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
        reported_at: Instant,
    ) -> FullLifecycleHookReportRoute {
        if !shepr_agent::detect::full_lifecycle_hook_authority(source, agent_label) {
            return FullLifecycleHookReportRoute::Accept {
                reanchor_sequence: false,
            };
        }
        if self.full_lifecycle_hook_report_matches_stale_session(source, agent_label, session_ref) {
            return FullLifecycleHookReportRoute::Ignore;
        }

        let known_agent = shepr_agent::detect::parse_agent_label(agent_label);
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
        let opencode_cross_talk = (source, agent_label) == ("shepr:opencode", "opencode")
            && process_present
            && anchored_session_ref
                .zip(session_ref.as_ref())
                .is_some_and(|(anchored, incoming)| anchored != incoming);
        if opencode_cross_talk {
            return FullLifecycleHookReportRoute::Ignore;
        }
        if let Some(suppressed) = self.suppressed_full_lifecycle_hook_reports.get(source) {
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

        if process_present
            && session_anchored
            && !self
                .suppressed_full_lifecycle_hook_reports
                .contains_key(source)
        {
            return FullLifecycleHookReportRoute::Accept {
                reanchor_sequence: self
                    .full_lifecycle_hook_report_has_fresh_session_after_stale_session(
                        source,
                        agent_label,
                        session_ref,
                    ),
            };
        }

        let Some(session_ref) = session_ref.clone() else {
            return FullLifecycleHookReportRoute::Ignore;
        };
        let Some(seq) = seq else {
            return FullLifecycleHookReportRoute::Ignore;
        };
        if self.hook_seq_superseded(source, seq, reported_at) {
            return FullLifecycleHookReportRoute::Ignore;
        }

        let previous_session_ref = self
            .persisted_agent_session
            .as_ref()
            .filter(|session| {
                session.source.as_str() == source && session.agent.label() == agent_label
            })
            .map(|session| session.session_ref.clone());
        let suppressed = self
            .suppressed_full_lifecycle_hook_reports
            .entry(source.to_string())
            .or_insert_with(|| SuppressedFullLifecycleHookReport {
                agent_label: agent_label.to_string(),
                session_ref: previous_session_ref,
                observed_at: reported_at,
                reason: FullLifecycleHookSuppressionReason::ProcessExit,
                replacement_session_ref: None,
                pending_replacement_report: None,
            });
        let replace_pending = suppressed
            .pending_replacement_report
            .as_ref()
            .is_none_or(|pending| seq > pending.seq);
        if replace_pending {
            suppressed.pending_replacement_report = Some(PendingFullLifecycleHookReport {
                authority: HookAuthority {
                    source: source.to_string(),
                    agent_label: agent_label.to_string(),
                    state,
                    message: message.map(str::to_string),
                    reported_at,
                    session_ref: Some(session_ref),
                },
                seq,
            });
        }
        FullLifecycleHookReportRoute::Ignore
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
        self.stale_full_lifecycle_hook_sessions
            .get(source)
            .is_some_and(|stale_sessions| {
                session_ref.as_ref().is_some_and(|incoming_ref| {
                    stale_sessions.iter().any(|stale| {
                        stale.agent_label == agent_label && incoming_ref == &stale.session_ref
                    })
                })
            })
    }

    pub(super) fn full_lifecycle_hook_report_has_fresh_session_after_stale_session(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &Option<shepr_agent::agent::resume::AgentSessionRef>,
    ) -> bool {
        if !shepr_agent::detect::full_lifecycle_hook_authority(source, agent_label) {
            return false;
        }
        self.stale_full_lifecycle_hook_sessions
            .get(source)
            .is_some_and(|stale_sessions| {
                stale_sessions
                    .iter()
                    .any(|stale| stale.agent_label == agent_label)
                    && session_ref.as_ref().is_some_and(|incoming_ref| {
                        stale_sessions.iter().all(|stale| {
                            stale.agent_label != agent_label || incoming_ref != &stale.session_ref
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
        now: Instant,
    ) {
        let Some(detected_agent) = detected_agent else {
            return;
        };
        if previous_detected_agent == Some(detected_agent) {
            return;
        }
        let detected_label = shepr_agent::detect::agent_label(detected_agent);
        let mut stale_sessions = Vec::new();
        let mut validated_replacement_sessions = Vec::new();
        self.suppressed_full_lifecycle_hook_reports
            .retain(|source, suppressed| {
                let should_clear = shepr_agent::detect::parse_agent_label(&suppressed.agent_label)
                    == Some(detected_agent);
                if !should_clear {
                    return true;
                }
                if suppressed.reason == FullLifecycleHookSuppressionReason::ProcessExit {
                    if let Some(session_ref) = suppressed.replacement_session_ref.take() {
                        if let Some(exited_session_ref) = suppressed
                            .session_ref
                            .as_ref()
                            .filter(|exited_session_ref| *exited_session_ref != &session_ref)
                            .cloned()
                        {
                            stale_sessions.push((
                                source.clone(),
                                StaleFullLifecycleHookSession {
                                    agent_label: suppressed.agent_label.clone(),
                                    session_ref: exited_session_ref,
                                },
                            ));
                        }
                        let session_start_seq = self.hook_report_sequences.get(source).copied();
                        let pending =
                            suppressed
                                .pending_replacement_report
                                .take()
                                .filter(|pending| {
                                    pending.authority.session_ref.as_ref() == Some(&session_ref)
                                        && session_start_seq.is_none_or(|seq| pending.seq > seq)
                                });
                        validated_replacement_sessions.push((
                            source.clone(),
                            suppressed.agent_label.clone(),
                            session_ref,
                            pending,
                        ));
                        return false;
                    }
                    return true;
                }
                if let Some(session_ref) = suppressed.session_ref.clone() {
                    stale_sessions.push((
                        source.clone(),
                        StaleFullLifecycleHookSession {
                            agent_label: suppressed.agent_label.clone(),
                            session_ref,
                        },
                    ));
                }
                false
            });
        for (source, stale_session) in stale_sessions {
            self.remember_stale_full_lifecycle_hook_session(
                source,
                stale_session.agent_label,
                stale_session.session_ref,
            );
        }
        self.hook_report_sequences.retain(|source, _| {
            validated_replacement_sessions
                .iter()
                .any(|(validated_source, _, _, _)| validated_source == source)
                || !shepr_agent::detect::full_lifecycle_hook_authority(source, detected_label)
        });
        let sequences = &self.hook_report_sequences;
        self.hook_report_accepted_at
            .retain(|source, _| sequences.contains_key(source));
        for (source, agent_label, session_ref, pending) in validated_replacement_sessions {
            self.forget_stale_full_lifecycle_hook_session(&source, &agent_label, &session_ref);
            let Some(persisted_session) =
                shepr_agent::agent::resume::PersistedAgentSession::from_report(
                    &source,
                    &agent_label,
                    session_ref,
                )
            else {
                continue;
            };
            self.persisted_agent_session = Some(persisted_session);
            if let Some(pending) = pending
                && self.record_hook_seq(source, pending.seq, now)
            {
                self.hook_authority = Some(pending.authority);
            }
        }
    }

    pub(super) fn remember_stale_full_lifecycle_hook_session(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: shepr_agent::agent::resume::AgentSessionRef,
    ) {
        let stale_session = StaleFullLifecycleHookSession {
            agent_label,
            session_ref,
        };
        let source_stale_sessions = self
            .stale_full_lifecycle_hook_sessions
            .entry(source)
            .or_default();
        if !source_stale_sessions
            .iter()
            .any(|existing| existing == &stale_session)
        {
            if source_stale_sessions.len() >= MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE {
                let stale_to_forget = source_stale_sessions.len()
                    - MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE
                    + 1;
                source_stale_sessions
                    .drain(..stale_to_forget)
                    .for_each(drop);
            }
            source_stale_sessions.push(stale_session);
        }
    }

    pub(super) fn forget_stale_full_lifecycle_hook_session(
        &mut self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
    ) {
        let remove_source = self
            .stale_full_lifecycle_hook_sessions
            .get_mut(source)
            .is_some_and(|stale_sessions| {
                stale_sessions.retain(|stale| {
                    stale.agent_label != agent_label || &stale.session_ref != session_ref
                });
                stale_sessions.is_empty()
            });
        if remove_source {
            self.stale_full_lifecycle_hook_sessions.remove(source);
        }
    }

    pub(super) fn detected_state_observed_before_release_suppression(
        &self,
        detected_agent: Option<Agent>,
        observed_at: Instant,
    ) -> bool {
        let Some(detected_agent) = detected_agent else {
            return false;
        };
        self.suppressed_full_lifecycle_hook_reports
            .values()
            .any(|suppressed| {
                shepr_agent::detect::parse_agent_label(&suppressed.agent_label)
                    == Some(detected_agent)
                    && observed_at <= suppressed.observed_at
            })
    }

    pub(super) fn current_session_identity_for_persistence(
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
        let source = shepr_agent::agent::AgentSource::parse(source);
        if source
            .agent()
            .is_some_and(|source_agent| source_agent != agent)
        {
            return true;
        }
        current.source != source || current.agent != agent
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

    pub(super) fn lifecycle_hook_report_replaces_persisted_session(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &shepr_agent::agent::resume::AgentSessionRef,
    ) -> bool {
        self.hook_authority.is_none()
            && (source, agent_label) == ("shepr:mastracode", "mastracode")
            && self
                .persisted_agent_session
                .as_ref()
                .is_some_and(|session| {
                    session.source.as_str() == source
                        && session.agent.label() == agent_label
                        && session.session_ref.kind()
                            == shepr_agent::agent::resume::AgentSessionRefKind::Id
                        && session_ref.kind() == shepr_agent::agent::resume::AgentSessionRefKind::Id
                        && &session.session_ref != session_ref
                })
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
        use AgentSessionStartSource as Start;
        matches!(
            (agent, session_start_source),
            (
                Agent::Claude,
                Some(Start::Clear | Start::Resume | Start::Compact)
            ) | (
                Agent::Codex,
                Some(Start::Startup | Start::Clear | Start::Resume | Start::Compact)
            ) | (Agent::Mastracode, Some(Start::Startup))
                | (Agent::OpenCode, Some(Start::Select))
                | (Agent::Pi, Some(Start::New | Start::Resume | Start::Fork))
                | (Agent::Grok, Some(Start::New | Start::Load))
                | (
                    Agent::Omp,
                    Some(Start::Startup | Start::New | Start::Resume | Start::Fork)
                )
                | (
                    Agent::Qwen,
                    Some(
                        Start::Startup
                            | Start::Clear
                            | Start::Resume
                            | Start::Compact
                            | Start::Branch
                    )
                )
                | (Agent::Antigravity, None)
        )
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
        (source, agent_label, session_start_source, seq)
            == (
                "shepr:opencode",
                "opencode",
                Some(AgentSessionStartSource::Select),
                None,
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
        message: Option<String>,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.set_hook_authority_at(
            source,
            agent_label,
            state,
            message,
            None,
            seq,
            Instant::now(),
        )
        .and_then(|mutation| mutation.effective_state_change)
    }

    pub fn set_hook_authority_with_session_ref(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        seq: Option<u64>,
    ) -> Option<TerminalStateMutation> {
        self.set_hook_authority_at(
            source,
            agent_label,
            state,
            message,
            session_ref,
            seq,
            Instant::now(),
        )
    }
}
