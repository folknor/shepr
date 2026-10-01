use super::*;

impl TerminalState {
    pub(super) fn transition_provisional_detection(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> TerminalStateMutation {
        if let Some(mut pending) = self.provisional_process_exit {
            // Old queued observations neither confirm nor cancel a newer exit.
            if now <= pending.observed_at {
                return TerminalStateMutation::default();
            }
            if !process_exited && agent.is_some() {
                self.provisional_process_exit = None;
            } else {
                if !process_exited {
                    pending.deferred = Some(DeferredDetection {
                        fallback_state,
                        visible_blocker,
                        observed_at: now,
                    });
                }
                if pending.cancelled
                    || now.saturating_duration_since(pending.observed_at)
                        < crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE
                {
                    self.provisional_process_exit = Some(pending);
                    return TerminalStateMutation::default();
                }
                self.provisional_process_exit = None;
                // Confirmation proves shell survival, but must not promote an
                // old detector observation above a newer custom hook report.
                let release = self.transition_detection(
                    pending.agent,
                    AgentState::Idle,
                    false,
                    true,
                    pending.observed_at,
                );
                let Some(deferred) = pending.deferred else {
                    return release;
                };
                let withdrawal = self.transition_detection(
                    None,
                    deferred.fallback_state,
                    deferred.visible_blocker,
                    false,
                    deferred.observed_at,
                );
                return TerminalStateMutation {
                    effective_state_change: match (
                        release.effective_state_change,
                        withdrawal.effective_state_change,
                    ) {
                        (Some(first), Some(last)) => Some(EffectiveStateChange {
                            previous_state: first.previous_state,
                            state: last.state,
                        }),
                        (first, last) => last.or(first),
                    },
                    session_ref_changed: release.session_ref_changed
                        || withdrawal.session_ref_changed,
                    agent_released: release.agent_released || withdrawal.agent_released,
                };
            }
        }
        if process_exited {
            self.provisional_process_exit = Some(ProvisionalProcessExit {
                agent,
                observed_at: now,
                cancelled: false,
                deferred: None,
            });
            return TerminalStateMutation::default();
        }
        self.transition_detection(agent, fallback_state, visible_blocker, false, now)
    }

    pub(super) fn transition_detection(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> TerminalStateMutation {
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_state = self.state;
        let previous_detected_agent = self.detected_agent;
        let previous_session = self.current_session_identity_for_persistence();
        let newer_custom_authority = process_exited
            && self.hook_authority.as_ref().is_some_and(|authority| {
                Agent::parse_canonical_label(&authority.agent_label) == agent
                    && !shepr_agent::agent::resume::is_official_agent_source(
                        &authority.source,
                        &authority.agent_label,
                    )
                    && authority.reported_at > now
            });
        let agent_released =
            process_exited && !newer_custom_authority && previous_agent_label.is_some();
        if self.should_ignore_detected_state_under_full_lifecycle_hook(agent, process_exited) {
            if self
                .hook_authority
                .as_ref()
                .and_then(|authority| Agent::parse_canonical_label(&authority.agent_label))
                == agent
            {
                self.detected_agent = agent;
            }
            return TerminalStateMutation {
                effective_state_change: self
                    .recompute_effective_state(previous_agent_label.as_deref(), previous_state),
                session_ref_changed: previous_session
                    != self.current_session_identity_for_persistence(),
                agent_released: false,
            };
        }
        let replacement_process_detected = !process_exited
            && agent.is_some()
            && self
                .process_evidence
                .exit()
                .is_some_and(|exit| Some(exit.agent) == agent && exit.observed_at < now);
        if !process_exited && self.detected_state_observed_before_release_suppression(agent, now) {
            return TerminalStateMutation {
                effective_state_change: self
                    .recompute_effective_state(previous_agent_label.as_deref(), previous_state),
                session_ref_changed: previous_session
                    != self.current_session_identity_for_persistence(),
                agent_released: false,
            };
        }
        self.detected_agent = agent;
        if !process_exited {
            self.clear_full_lifecycle_hook_suppression_for_detected_agent(
                if replacement_process_detected {
                    None
                } else {
                    previous_detected_agent
                },
                agent,
            );
        }
        self.fallback_state = fallback_state;
        self.fallback_visible_blocker = visible_blocker && fallback_state == AgentState::Blocked;
        self.fallback_observed_at = Some(now);
        if process_exited {
            if let Some(agent) = agent {
                self.process_evidence = AgentProcessEvidence::Exited(RecentAgentProcessExit {
                    agent,
                    observed_at: now,
                });
            }
        } else if agent.is_some() {
            self.process_evidence = AgentProcessEvidence::Available;
        }
        if process_exited {
            if let Some(source) = agent.and_then(Agent::integration_source)
                && let Some(record) = self.hook_sources.get_mut(source)
            {
                record.transition(HookSourceEvent::ProcessExited(now));
            }

            let official_session = self
                .hook_authority
                .as_ref()
                .filter(|authority| {
                    shepr_agent::agent::resume::is_official_agent_source(
                        &authority.source,
                        &authority.agent_label,
                    ) && Agent::parse_canonical_label(&authority.agent_label) == agent
                })
                .map(|authority| {
                    (
                        authority.source.clone(),
                        authority.agent_label.clone(),
                        authority.session_ref.clone(),
                    )
                })
                .or_else(|| {
                    self.persisted_agent_session.as_ref().and_then(|session| {
                        agent
                            .is_some_and(|agent| {
                                session.source == shepr_agent::agent::AgentSource::Official(agent)
                                    && session.agent == agent
                            })
                            .then(|| {
                                (
                                    session.source.to_source_string(),
                                    session.agent.label().to_owned(),
                                    Some(session.session_ref.clone()),
                                )
                            })
                    })
                })
                // Only full-lifecycle integrations need a process-exit
                // suppression for their later state reports.
                .filter(|(source, agent_label, _)| {
                    shepr_agent::detect::full_lifecycle_hook_authority(source, agent_label)
                });
            if let Some((source, agent_label, session_ref)) = official_session {
                self.clear_hook_report_sequence(&source);
                self.suppress_full_lifecycle_hook_report_with_session_ref(
                    source,
                    agent_label,
                    session_ref,
                    FullLifecycleHookSuppressionReason::AwaitingProcess,
                    now,
                );
            }
            let cleared_hook_source = self.hook_authority.as_ref().and_then(|authority| {
                (Agent::parse_canonical_label(&authority.agent_label) == agent
                    && !newer_custom_authority)
                    .then(|| authority.source.clone())
            });
            if let Some(source) = cleared_hook_source {
                self.clear_hook_report_sequence(&source);
                self.apply_source_effect(HookSourceEffects::Commit {
                    authority: AuthorityEffect::Clear,
                    persisted: self.persisted_agent_session.clone(),
                });
            }
            if !newer_custom_authority
                && self
                    .persisted_agent_session
                    .as_ref()
                    .is_some_and(|session| Some(session.agent) == agent)
            {
                // A confirmed live-shell release clears the completed agent.
                // Pane death retains the pre-release identity when interrupted.
                self.apply_source_effect(HookSourceEffects::Commit {
                    authority: AuthorityEffect::Keep,
                    persisted: None,
                });
            }
        }
        if self.hook_authority_not_newer_than(now)
            && (self.hook_authority_conflicts_with_detected_agent(agent)
                || (previous_detected_agent.is_some()
                    && agent != previous_detected_agent
                    && self.hook_authority.as_ref().is_some_and(|authority| {
                        Agent::parse_canonical_label(&authority.agent_label)
                            == previous_detected_agent
                    })))
        {
            let durable_session = self.hook_authority.as_ref().and_then(|authority| {
                authority.session_ref.as_ref().and_then(|session_ref| {
                    shepr_agent::agent::resume::PersistedAgentSession::from_report(
                        &authority.source,
                        &authority.agent_label,
                        session_ref.clone(),
                    )
                })
            });
            self.suppress_current_full_lifecycle_hook_authority(
                FullLifecycleHookSuppressionReason::HookClear,
                now,
            );
            self.apply_source_effect(HookSourceEffects::Commit {
                authority: AuthorityEffect::Clear,
                persisted: durable_session,
            });
        }
        let effective_state_change =
            self.recompute_effective_state(previous_agent_label.as_deref(), previous_state);
        TerminalStateMutation {
            effective_state_change,
            session_ref_changed: previous_session
                != self.current_session_identity_for_persistence(),
            agent_released,
        }
    }

    pub(super) fn transition_pane_exit(
        &mut self,
        exit_reason: shepr_platform::ChildExitReason,
        now: Instant,
    ) -> TerminalStateMutation {
        let previous_session = self.current_session_identity_for_persistence();
        let agent = self.effective_known_agent().or(self.detected_agent);
        // Pane death wins over a provisional detector release. Apply the final
        // release directly, retaining the pre-release identity for a checkpoint.
        self.provisional_process_exit = None;
        let mut mutation = self.transition_detection(agent, AgentState::Idle, false, true, now);
        if exit_reason.requires_session_checkpoint() {
            self.apply_source_effect(HookSourceEffects::Commit {
                authority: AuthorityEffect::Keep,
                persisted: previous_session.clone(),
            });
        }
        mutation.session_ref_changed =
            previous_session != self.current_session_identity_for_persistence();
        mutation
    }
}
