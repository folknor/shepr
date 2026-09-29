use super::*;

impl TerminalState {
    pub fn set_detected_agent_process_at(
        &mut self,
        agent: Agent,
        now: Instant,
    ) -> TerminalStateMutation {
        self.set_detected_state_with_screen_signals_at(
            Some(agent),
            AgentState::Unknown,
            false,
            false,
            now,
        )
    }

    pub fn terminal_title_stripped(&self) -> Option<String> {
        self.terminal_title
            .as_deref()
            .and_then(crate::terminal::stripped_terminal_title)
    }

    pub fn set_terminal_title(&mut self, title: Option<String>) -> TerminalTitleChange {
        if self.terminal_title == title {
            return TerminalTitleChange::default();
        }
        let previous_stripped = self.terminal_title_stripped();
        self.terminal_title = title;
        TerminalTitleChange {
            raw_changed: true,
            stripped_changed: previous_stripped != self.terminal_title_stripped(),
        }
    }

    pub fn with_pending_agent_resume_plan(
        mut self,
        plan: shepr_agent::agent::resume::AgentResumePlan,
    ) -> Self {
        self.pending_agent_resume_plan = Some(plan);
        self
    }

    /// The deferred resume can never run (its directory is gone, its shell
    /// will not start), so the pane stays without any process. Drops the
    /// plan, records why, and withdraws the detection `restored_terminal`
    /// seeded for the resumed agent: no runtime ever existed here, so that
    /// detection is only the seed, and with no detector to ever report the
    /// pane empty it would show an idle agent on a dead pane indefinitely.
    /// The saved session stays, as for any unavailable restored pane, so a
    /// later save writes it back.
    pub fn abandon_agent_resume(&mut self, error: super::RestoreFailure, now: Instant) {
        self.pending_agent_resume_plan = None;
        self.restore_error = Some(error);
        if self.detected_agent.is_some() {
            let _ = self.set_detected_state_with_screen_signals_at(
                None,
                AgentState::Unknown,
                false,
                false,
                now,
            );
        }
    }

    pub fn set_detected_state_with_screen_signals_at(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        now: Instant,
    ) -> TerminalStateMutation {
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_detected_agent = self.detected_agent;
        let previous_session = self.current_session_identity_for_persistence();
        let newer_custom_authority = process_exited
            && self.hook_authority.as_ref().is_some_and(|authority| {
                shepr_agent::detect::parse_agent_label(&authority.agent_label) == agent
                    && !shepr_agent::agent::resume::is_official_agent_source(
                        &authority.source,
                        &authority.agent_label,
                    )
                    && authority.reported_at > now
            });
        let agent_released =
            process_exited && !newer_custom_authority && previous_agent_label.is_some();
        if self.should_ignore_detected_state_under_full_lifecycle_hook(agent, process_exited) {
            if self.hook_authority.as_ref().and_then(|authority| {
                shepr_agent::detect::parse_agent_label(&authority.agent_label)
            }) == agent
            {
                self.detected_agent = agent;
            }
            return TerminalStateMutation {
                effective_state_change: self.recompute_effective_state(
                    previous_agent_label,
                    previous_known_agent,
                    previous_state,
                ),
                session_ref_changed: previous_session
                    != self.current_session_identity_for_persistence(),
                agent_released: false,
            };
        }
        let replacement_process_detected = !process_exited
            && agent.is_some()
            && self
                .recent_agent_process_exit
                .is_some_and(|exit| Some(exit.agent) == agent && exit.observed_at < now);
        if !process_exited && self.detected_state_observed_before_release_suppression(agent, now) {
            return TerminalStateMutation {
                effective_state_change: self.recompute_effective_state(
                    previous_agent_label,
                    previous_known_agent,
                    previous_state,
                ),
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
        self.fallback_observed_at = Some(now);
        if process_exited {
            if let Some(agent) = agent {
                self.recent_agent_process_exit = Some(RecentAgentProcessExit {
                    agent,
                    observed_at: now,
                });
            }
        } else if agent.is_some() {
            self.recent_agent_process_exit = None;
        }
        if process_exited {
            let mut reset_sources = Vec::new();
            let mut stale_sessions = Vec::new();
            for (source, suppressed) in &mut self.suppressed_full_lifecycle_hook_reports {
                if shepr_agent::detect::parse_agent_label(&suppressed.agent_label) != agent
                    || suppressed.reason == FullLifecycleHookSuppressionReason::HookClear
                {
                    continue;
                }
                let exited_session_ref = suppressed
                    .replacement_session_ref
                    .take()
                    .or_else(|| {
                        suppressed
                            .pending_replacement_report
                            .as_ref()
                            .and_then(|pending| pending.authority.session_ref.clone())
                    })
                    .or_else(|| suppressed.session_ref.clone());
                if let (Some(previous), Some(exited)) =
                    (suppressed.session_ref.as_ref(), exited_session_ref.as_ref())
                    && previous != exited
                {
                    stale_sessions.push((
                        source.clone(),
                        suppressed.agent_label.clone(),
                        previous.clone(),
                    ));
                }
                suppressed.session_ref = exited_session_ref;
                suppressed.pending_replacement_report = None;
                suppressed.observed_at = now;
                reset_sources.push(source.clone());
            }
            for (source, agent_label, session_ref) in stale_sessions {
                self.remember_stale_full_lifecycle_hook_session(source, agent_label, session_ref);
            }
            for source in reset_sources {
                self.clear_hook_report_sequence(&source);
            }

            let official_session = self
                .hook_authority
                .as_ref()
                .filter(|authority| {
                    shepr_agent::agent::resume::is_official_agent_source(
                        &authority.source,
                        &authority.agent_label,
                    ) && shepr_agent::detect::parse_agent_label(&authority.agent_label) == agent
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
                });
            if let Some((source, agent_label, session_ref)) = official_session {
                self.clear_hook_report_sequence(&source);
                self.suppress_full_lifecycle_hook_report_with_session_ref(
                    source,
                    agent_label,
                    session_ref,
                    FullLifecycleHookSuppressionReason::ProcessExit,
                    now,
                );
            }
            let cleared_hook_source = self.hook_authority.as_ref().and_then(|authority| {
                (shepr_agent::detect::parse_agent_label(&authority.agent_label) == agent
                    && !newer_custom_authority)
                    .then(|| authority.source.clone())
            });
            if let Some(source) = cleared_hook_source {
                self.clear_hook_report_sequence(&source);
                self.hook_authority = None;
            }
            if !newer_custom_authority
                && self
                    .persisted_agent_session
                    .as_ref()
                    .is_some_and(|session| Some(session.agent) == agent)
            {
                self.persisted_agent_session = None;
            }
        }
        if self.hook_authority_not_newer_than(now)
            && (self.hook_authority_conflicts_with_detected_agent(agent)
                || (previous_detected_agent.is_some()
                    && agent != previous_detected_agent
                    && self.hook_authority.as_ref().is_some_and(|authority| {
                        shepr_agent::detect::parse_agent_label(&authority.agent_label)
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
            self.hook_authority = None;
            self.persisted_agent_session = durable_session;
        }
        let effective_state_change = self.recompute_effective_state(
            previous_agent_label,
            previous_known_agent,
            previous_state,
        );
        TerminalStateMutation {
            effective_state_change,
            session_ref_changed: previous_session
                != self.current_session_identity_for_persistence(),
            agent_released,
        }
    }
}

#[cfg(test)]
impl TerminalState {
    pub fn set_detected_state(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        self.set_detected_state_with_visible_blocker(agent, fallback_state, false, false, false)
    }

    pub fn set_detected_state_with_mutation(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> TerminalStateMutation {
        self.set_detected_state_with_screen_signals_at(
            agent,
            fallback_state,
            false,
            false,
            Instant::now(),
        )
    }

    pub fn set_detected_state_with_visible_blocker(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        _ignored_screen_idle: bool,
        process_exited: bool,
    ) -> Option<EffectiveStateChange> {
        self.set_detected_state_with_screen_signals_at(
            agent,
            fallback_state,
            visible_blocker,
            process_exited,
            Instant::now(),
        )
        .effective_state_change
    }
}
