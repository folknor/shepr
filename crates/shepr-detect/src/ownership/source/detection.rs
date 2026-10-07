use super::*;

impl AgentOwnership {
    /// A detector observation. An agent's exit releases at once: the pane
    /// shows no agent the moment its process is gone, and nothing has to
    /// guess who owns the pane while a release waits. When the release
    /// removes the pane's resume identity, that identity is kept aside as the
    /// checkpoint candidate (see `CheckpointCandidate`); a later genuine
    /// release replaces it. Newer accepted evidence of an agent process
    /// discards it, since that process is not the one that exited.
    /// Older detector observations, and an exit repeating the recorded one, are
    /// ignored before touching source generations, so replay cannot consume a
    /// subsequently parked start. One detector tick stamps its process and
    /// state observations with the same instant, so only a strictly older
    /// observation is stale; an exit at the instant of the last presence
    /// observation is a new exit. An applied observation also advances a
    /// start held for a relaunch (`ReplacementStart`), and admits it once the
    /// exit and the replacement's presence confirm the relaunch.
    pub(super) fn transition_detector_observation(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> AgentOwnershipMutation {
        if self.pane_ended
            || self
                .detector_observed_at
                .is_some_and(|observed_at| now < observed_at)
            || self.process_evidence.exit().is_some_and(|exit| {
                now < exit.observed_at || (process_exited && now == exit.observed_at)
            })
        {
            return AgentOwnershipMutation::default();
        }
        // Hooks invalidate screen evidence, not process evidence. An exit
        // sampled before activation still releases that process; only a newer
        // detector observation can supersede it. The mux publishes it once.
        self.detector_observed_at = Some(now);
        if !process_exited
            && self
                .fallback_observed_at
                .is_some_and(|observed_at| now < observed_at)
        {
            return AgentOwnershipMutation::default();
        }
        let previous_session = self.current_session_identity_for_persistence();
        // Only a held relaunch start needs the pane as it stood before this
        // observation; every other observation skips the reads.
        let before_held = self.replacement_start.is_some().then(|| {
            (
                self.effective_agent(),
                self.state,
                self.process_evidence.exit(),
            )
        });
        let mut mutation =
            self.transition_detection(agent, fallback_state, visible_blocker, process_exited, now);
        if process_exited {
            if let Some(identity) = previous_session.clone()
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
        if let Some((previous_agent, previous_state, exit_before)) = before_held
            && let Some(held) =
                self.advance_replacement_start(agent, process_exited, exit_before, now)
        {
            // Admitted as if it had arrived now that the old process is gone
            // and its replacement present: the same arbitration, ordering and
            // diagnostic record as any start, at the start's own clock sample.
            let outcome = self.admit_session_start(
                &held.origin,
                Some(held.session.session_ref().clone()),
                held.seq,
                held.session_start_source,
                held.received,
            );
            if matches!(outcome, HookOutcome::Applied(_)) {
                mutation.effective_state_change =
                    self.recompute_effective_state(previous_agent, previous_state);
                mutation.session_ref_changed =
                    previous_session != self.current_session_identity_for_persistence();
            }
        }
        mutation
    }

    /// Advances a held relaunch start (`ReplacementStart`) by the observation
    /// just applied, returning it when the observation confirms the relaunch.
    /// `exit_before` is the recorded exit before the observation. An
    /// observation naming no agent neither confirms nor refutes it.
    fn advance_replacement_start(
        &mut self,
        agent: Option<Agent>,
        process_exited: bool,
        exit_before: Option<RecentAgentProcessExit>,
        now: Instant,
    ) -> Option<ReplacementStart> {
        let held = self.replacement_start.take()?;
        let held_agent = held.origin.agent();
        if now > held.deadline() || agent.is_some_and(|observed| observed != held_agent) {
            return None;
        }
        if agent.is_none() {
            self.replacement_start = Some(held);
            return None;
        }
        if process_exited {
            let recorded = self
                .process_evidence
                .exit()
                .is_some_and(|exit| exit.agent == held_agent && exit.observed_at == now);
            self.replacement_start = match (recorded, held.exit_observed_at) {
                (false, _) => Some(held),
                (true, None) => Some(ReplacementStart {
                    exit_observed_at: Some(now),
                    ..held
                }),
                // A second exit: the process the start may belong to has gone
                // too.
                (true, Some(_)) => None,
            };
            return None;
        }
        let replacement_present = held.exit_observed_at.is_some()
            && exit_before.is_some_and(|exit| exit.agent == held_agent)
            && self.process_evidence.exit().is_none()
            && self.detected_agent == Some(held_agent);
        if replacement_present {
            return Some(held);
        }
        // Continued presence of the process the start was refused against, or
        // presence the release refused as older than the exit: keep waiting.
        self.replacement_start = Some(held);
        None
    }

    pub(super) fn transition_detection(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> AgentOwnershipMutation {
        let previous_agent = self.effective_agent();
        let previous_state = self.state;
        let previous_detected_agent = self.detected_agent;
        let previous_session = self.current_session_identity_for_persistence();
        let agent_released = process_exited && previous_agent.is_some();
        if self.should_ignore_detected_state_under_full_lifecycle_hook(agent, process_exited) {
            // The hook governs only while `detected_agent` is already its
            // agent, so this rewrites the same value: an overridden
            // observation never moves the detected agent (see
            // `clear_full_lifecycle_hook_suppression_for_detected_agent`).
            if self
                .hook_authority
                .as_ref()
                .map(|authority| authority.origin.agent())
                == agent
            {
                self.detected_agent = agent;
            }
            return AgentOwnershipMutation {
                effective_state_change: self
                    .recompute_effective_state(previous_agent, previous_state),
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
            return AgentOwnershipMutation {
                effective_state_change: self
                    .recompute_effective_state(previous_agent, previous_state),
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
                now,
            );
        }
        self.fallback_state = fallback_state;
        self.fallback_visible_blocker = visible_blocker && fallback_state == AgentState::Blocked;
        // An exit may be older than the activation watermark (see
        // `transition_detector_observation`); it must not move that watermark
        // back and readmit screen verdicts from before the activation.
        self.fallback_observed_at = self.fallback_observed_at.max(Some(now));
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
            if let Some(target) = agent.and_then(Agent::integration_target)
                && let Some(record) = self.hook_sources.get_mut(&AgentSource::new(target))
            {
                record.transition(HookSourceEvent::ProcessExited(now));
            }

            let official_session = self
                .hook_authority
                .as_ref()
                .filter(|authority| Some(authority.origin.agent()) == agent)
                .map(|authority| (authority.origin, authority.session_ref.clone()))
                .or_else(|| {
                    self.persisted_agent_session.as_ref().and_then(|session| {
                        if Some(session.agent()) != agent {
                            return None;
                        }
                        Some((
                            ReportOrigin::official(session.agent())?,
                            Some(session.session_ref().clone()),
                        ))
                    })
                })
                // Only full-lifecycle integrations need a process-exit
                // suppression for their later state reports.
                .filter(|(origin, _)| origin.is_full_lifecycle());
            if let Some((origin, session_ref)) = official_session {
                self.clear_hook_source_sequence(origin.source());
                self.suppress_full_lifecycle_hook_report_with_session_ref(
                    &origin,
                    session_ref,
                    FullLifecycleHookSuppressionReason::AwaitingProcess,
                    now,
                );
            }
            let cleared_hook_source = self.hook_authority.as_ref().and_then(|authority| {
                (Some(authority.origin.agent()) == agent).then_some(*authority.origin.source())
            });
            if let Some(source) = cleared_hook_source {
                self.clear_hook_source_sequence(&source);
                self.apply_source_effect(HookSourceEffects::Commit {
                    authority: AuthorityEffect::Clear,
                    persisted: self.persisted_agent_session.clone(),
                });
            }
            if self
                .persisted_agent_session
                .as_ref()
                .is_some_and(|session| Some(session.agent()) == agent)
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
                        Some(authority.origin.agent()) == previous_detected_agent
                    })))
        {
            // The authority withdrawn here never belongs to the agent the
            // detector now reports (the guard above), so a persisted
            // identity owned by that agent (a promoted parked start, a
            // restored seed) outranks it. Otherwise the effective identity is
            // kept: the authority's own session, else what was persisted. A
            // sessionless authority never owned the resume identity and must
            // not clear it. Promotion requires process evidence within the
            // parked start's lifetime; neither input supplies a process handle
            // that could prove attribution beyond that temporal bound.
            let durable_session = match &self.persisted_agent_session {
                Some(persisted) if agent == Some(persisted.agent()) => Some(persisted.clone()),
                persisted => {
                    self.hook_authority
                        .as_ref()
                        .and_then(|authority| {
                            authority.session_ref.as_ref().and_then(|session_ref| {
                                authority.origin.session(session_ref.clone())
                            })
                        })
                        .or_else(|| persisted.clone())
                }
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
        let effective_state_change = self.recompute_effective_state(previous_agent, previous_state);
        AgentOwnershipMutation {
            effective_state_change,
            session_ref_changed: previous_session
                != self.current_session_identity_for_persistence(),
            agent_released,
        }
    }

    pub(super) fn transition_pane_exit(
        &mut self,
        needs_checkpoint: bool,
        now: Instant,
    ) -> AgentOwnershipMutation {
        if self.pane_ended {
            return AgentOwnershipMutation::default();
        }
        let previous_session = self.current_session_identity_for_persistence();
        let agent = self.effective_agent().or(self.detected_agent);
        // A pane's own death is never a candidate: it is resolved here.
        let candidate = self.checkpoint_candidate.take();
        // No replacement presence can follow the pane's own ending.
        self.replacement_start = None;
        self.pane_ended = true;
        let mut mutation = self.transition_detection(agent, AgentState::Idle, false, true, now);
        if needs_checkpoint {
            // What the pane held when it died; failing that, an identity a
            // detector release removed just before, as a group kill that took
            // the agent first leaves it. The pane is gone once its checkpoint
            // settles, so writing it back into the slot only feeds that
            // checkpoint, and it changes the saved identity, which marks the
            // session dirty so an older checkpoint cannot settle this exit.
            let identity = previous_session.clone().or_else(|| {
                candidate
                    .filter(|candidate| {
                        candidate.qualifies(CheckpointContext::PaneEnding { ended_at: now })
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
            CheckpointContext::PaneEnding { ended_at } => {
                self.observed_at <= ended_at && ended_at.duration_since(self.observed_at) <= grace
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
