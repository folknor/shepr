use super::*;

impl TerminalState {
    /// A detector observation. An agent's exit releases at once: the pane
    /// shows no agent the moment its process is gone, and nothing has to
    /// guess who owns the pane while a release waits. When the release
    /// removes the pane's resume identity, that identity is kept aside as the
    /// checkpoint candidate (see `CheckpointCandidate`); a later genuine
    /// release replaces it. Newer accepted evidence of an agent process
    /// discards it, since that process is not the one that exited.
    ///
    /// The detector reports an exit once (its exit bookkeeping survives
    /// resets), but an old exit replayed to a pane whose sources moved on can
    /// still act on them: `ProcessExited` consumes a start parked after the
    /// genuine release. That is not guarded here.
    pub(super) fn transition_detector_observation(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> TerminalStateMutation {
        if self.pane_ended {
            return TerminalStateMutation::default();
        }
        let previous_session = self.current_session_identity_for_persistence();
        let mutation =
            self.transition_detection(agent, fallback_state, visible_blocker, process_exited, now);
        if process_exited {
            if let Some(identity) = previous_session
                && self.current_session_identity_for_persistence().is_none()
            {
                self.checkpoint_candidate = Some(CheckpointCandidate {
                    identity,
                    observed_at: now,
                });
            }
        } else if agent.is_some()
            && self.detected_agent == agent
            && self
                .checkpoint_candidate
                .as_ref()
                .is_some_and(|candidate| candidate.observed_at < now)
        {
            self.checkpoint_candidate = None;
        }
        mutation
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
                // An exit under a live shell clears the completed agent. Pane
                // death retains the pre-release identity when interrupted.
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
            // The authority withdrawn here never belongs to the agent the
            // detector now reports (the guard above), so a persisted
            // identity owned by that agent (a promoted parked start, a
            // restored seed) outranks it. Otherwise the effective identity is
            // kept: the authority's own session, else what was persisted. A
            // sessionless authority never owned the resume identity and must
            // not clear it. This only decides between the two slots: whether a
            // promoted parked start really belongs to the process now detected
            // is a separate attribution gap (a parked start has no expiry and
            // carries no process identity).
            let durable_session = match &self.persisted_agent_session {
                Some(persisted) if agent == Some(persisted.agent) => Some(persisted.clone()),
                persisted => self
                    .hook_authority
                    .as_ref()
                    .and_then(|authority| {
                        authority.session_ref.as_ref().and_then(|session_ref| {
                            shepr_agent::agent::resume::PersistedAgentSession::from_report(
                                &authority.source,
                                &authority.agent_label,
                                session_ref.clone(),
                            )
                        })
                    })
                    .or_else(|| persisted.clone()),
            };
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
        // A pane's own death is never a candidate: it is resolved here.
        let candidate = self.checkpoint_candidate.take();
        self.pane_ended = true;
        let mut mutation = self.transition_detection(agent, AgentState::Idle, false, true, now);
        if exit_reason.requires_session_checkpoint() {
            // What the pane held when it died; failing that, an identity a
            // detector release removed just before, as a group kill that took
            // the agent first leaves it. The pane is gone once its checkpoint
            // settles, so writing it back into the slot only feeds that
            // checkpoint, and it changes the saved identity, which marks the
            // session dirty so an older checkpoint cannot settle this exit.
            let identity = previous_session.clone().or_else(|| {
                candidate
                    .filter(|candidate| {
                        candidate.qualifies(CheckpointContext::PaneEnding {
                            reason: exit_reason,
                            ended_at: now,
                        })
                    })
                    .map(|candidate| candidate.identity)
            });
            self.apply_source_effect(HookSourceEffects::Commit {
                authority: AuthorityEffect::Keep,
                persisted: identity,
            });
        }
        mutation.session_ref_changed =
            previous_session != self.current_session_identity_for_persistence();
        mutation
    }

    /// The final save after a termination signal: pane deaths are no longer
    /// processed, so a pane whose agent a detector release took within the
    /// grace of the signal, on either side of it, has its removed identity
    /// written back for that save. Returns whether the saved identity changed.
    pub fn adopt_checkpoint_candidate_for_shutdown(&mut self, signaled_at: Instant) -> bool {
        if self.current_session_identity_for_persistence().is_some() {
            return false;
        }
        let Some(candidate) = self.checkpoint_candidate.take().filter(|candidate| {
            candidate.qualifies(CheckpointContext::SignalShutdown { signaled_at })
        }) else {
            return false;
        };
        self.apply_source_effect(HookSourceEffects::Commit {
            authority: AuthorityEffect::Keep,
            persisted: Some(candidate.identity),
        });
        true
    }
}

impl CheckpointCandidate {
    /// Whether `context` may turn this candidate back into the saved identity.
    /// Lifetime is judged against the recorded ending, never the time it is
    /// handled, so a qualifying ending delivered late still counts.
    fn qualifies(&self, context: CheckpointContext) -> bool {
        let grace = crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE;
        match context {
            CheckpointContext::PaneEnding { reason, ended_at } => {
                reason.requires_session_checkpoint()
                    && self.observed_at <= ended_at
                    && ended_at.duration_since(self.observed_at) <= grace
            }
            // The release can land on either side of the signal: the agent
            // may die from the same kill a moment before or after the server
            // hears of it.
            CheckpointContext::SignalShutdown { signaled_at } => {
                self.observed_at.saturating_duration_since(signaled_at) <= grace
                    && signaled_at.saturating_duration_since(self.observed_at) <= grace
            }
        }
    }
}
