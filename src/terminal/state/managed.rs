use super::*;

impl TerminalState {
    pub fn set_manual_label(&mut self, mut label: String) {
        let trimmed = label.trim();
        if trimmed.len() != label.len() {
            label = trimmed.to_string();
        }
        self.manual_label = (!label.is_empty()).then_some(label);
    }

    pub fn clear_manual_label(&mut self) {
        self.manual_label = None;
    }

    pub fn set_agent_name(&mut self, name: String) {
        self.agent_name = (!name.is_empty()).then_some(name);
        self.agent_name_owner = self.agent_name.as_ref().and_then(|_| {
            self.hook_authority
                .as_ref()
                .map(|authority| AgentNameOwner {
                    agent_label: authority.agent_label.clone(),
                    session_ref: authority.session_ref.clone(),
                })
                .or_else(|| {
                    self.persisted_agent_session
                        .as_ref()
                        .map(|session| AgentNameOwner {
                            agent_label: session.agent.label().to_owned(),
                            session_ref: Some(session.session_ref.clone()),
                        })
                })
                .or_else(|| {
                    self.effective_agent_label()
                        .map(|agent_label| AgentNameOwner {
                            agent_label: agent_label.to_string(),
                            session_ref: None,
                        })
                })
        });
    }

    pub fn begin_managed_agent(
        &mut self,
        name: String,
        kind: Agent,
        now: Instant,
        settle_delay: Duration,
        timeout: Duration,
    ) {
        self.prompt_ready_agent = None;
        self.set_agent_name(name);
        self.agent_name_owner = Some(AgentNameOwner {
            agent_label: crate::detect::agent_label(kind).to_string(),
            session_ref: None,
        });
        self.managed_agent = Some(ManagedAgent {
            kind,
            phase: ManagedAgentPhase::Pending {
                ready_after: Some(now.checked_add(settle_delay).unwrap_or(now)),
                deadline: now.checked_add(timeout).unwrap_or(now),
                observed_expected: false,
            },
        });
    }

    pub fn managed_agent_launch_pending(&self) -> bool {
        self.managed_agent.is_some_and(|managed| {
            matches!(
                managed.phase,
                ManagedAgentPhase::Pending { .. } | ManagedAgentPhase::Blocked
            )
        })
    }

    pub fn managed_agent_interactive_ready(&self) -> bool {
        self.managed_agent
            .is_some_and(|managed| matches!(managed.phase, ManagedAgentPhase::Active))
    }

    pub fn observe_agent_prompt_ready(
        &mut self,
        agent: Agent,
        ready: bool,
    ) -> Option<TerminalStateMutation> {
        if !agent.prompt_observation()
            || self.detected_agent != Some(agent)
            || self.recent_agent_process_exit.is_some()
            || !self.managed_agent.is_some_and(|managed| {
                managed.kind == agent && managed.phase != ManagedAgentPhase::Active
            })
        {
            return None;
        }
        let next = if ready {
            Some(agent)
        } else if self.prompt_ready_agent == Some(agent) {
            None
        } else {
            return None;
        };
        if self.prompt_ready_agent == next {
            return None;
        }
        self.prompt_ready_agent = next;
        Some(TerminalStateMutation::default())
    }

    pub fn managed_agent_kind(&self) -> Option<Agent> {
        self.managed_agent.map(|managed| managed.kind)
    }

    pub fn next_managed_agent_deadline(&self) -> Option<Instant> {
        match self.managed_agent?.phase {
            ManagedAgentPhase::Pending {
                ready_after,
                deadline,
                ..
            } => Some(ready_after.unwrap_or(deadline).min(deadline)),
            ManagedAgentPhase::Resuming { deadline } => Some(deadline),
            ManagedAgentPhase::Blocked
            | ManagedAgentPhase::Active
            | ManagedAgentPhase::AwaitingResume => None,
        }
    }

    pub fn reconcile_managed_agent_at(&mut self, now: Instant, process_exited: bool) -> bool {
        let Some(managed) = self.managed_agent else {
            return false;
        };
        match managed.phase {
            ManagedAgentPhase::AwaitingResume => return false,
            ManagedAgentPhase::Resuming { deadline } => {
                return self.reconcile_managed_agent_resume(
                    managed.kind,
                    deadline,
                    now,
                    process_exited,
                );
            }
            ManagedAgentPhase::Pending { .. }
            | ManagedAgentPhase::Blocked
            | ManagedAgentPhase::Active => {}
        }
        let known_agent = self.effective_known_agent();
        let observed_expected = match managed.phase {
            ManagedAgentPhase::Pending {
                observed_expected, ..
            } => observed_expected || known_agent == Some(managed.kind),
            ManagedAgentPhase::Blocked
            | ManagedAgentPhase::Active
            | ManagedAgentPhase::AwaitingResume
            | ManagedAgentPhase::Resuming { .. } => false,
        };
        let clear = process_exited
            || known_agent.is_some_and(|agent| agent != managed.kind)
            || matches!(managed.phase, ManagedAgentPhase::Pending { .. })
                && observed_expected
                && known_agent.is_none();
        if clear {
            self.clear_agent_name();
            return true;
        }
        if managed.phase == ManagedAgentPhase::Blocked {
            if known_agent == Some(managed.kind)
                && managed_agent_state_is_ready(
                    managed.kind,
                    self.state,
                    self.prompt_ready_agent == Some(managed.kind),
                    crate::detect::manifest::has_screen_manifest,
                )
            {
                self.managed_agent = Some(ManagedAgent {
                    kind: managed.kind,
                    phase: ManagedAgentPhase::Active,
                });
                self.managed_agent_launch_session = None;
                return true;
            }
            return false;
        }
        if let ManagedAgentPhase::Pending {
            ready_after,
            deadline,
            observed_expected: previous_observed_expected,
        } = managed.phase
        {
            if known_agent == Some(managed.kind) && self.state == AgentState::Blocked {
                self.managed_agent = Some(ManagedAgent {
                    kind: managed.kind,
                    phase: ManagedAgentPhase::Blocked,
                });
                return true;
            }
            if now >= deadline {
                self.clear_agent_name();
                return true;
            }
            if ready_after.is_none_or(|ready_after| now >= ready_after) {
                if known_agent == Some(managed.kind)
                    && managed_agent_state_is_ready(
                        managed.kind,
                        self.state,
                        self.prompt_ready_agent == Some(managed.kind),
                        crate::detect::manifest::has_screen_manifest,
                    )
                {
                    self.managed_agent = Some(ManagedAgent {
                        kind: managed.kind,
                        phase: ManagedAgentPhase::Active,
                    });
                    self.managed_agent_launch_session = None;
                    return true;
                }
                if ready_after.is_some() {
                    self.managed_agent = Some(ManagedAgent {
                        kind: managed.kind,
                        phase: ManagedAgentPhase::Pending {
                            ready_after: None,
                            deadline,
                            observed_expected,
                        },
                    });
                    return true;
                }
            }
            if observed_expected != previous_observed_expected {
                self.managed_agent = Some(ManagedAgent {
                    kind: managed.kind,
                    phase: ManagedAgentPhase::Pending {
                        ready_after,
                        deadline,
                        observed_expected,
                    },
                });
                return true;
            }
        }
        false
    }

    pub fn restore_managed_agent(&mut self, name: String, kind: Agent) {
        self.restore_managed_agent_in(name, kind, ManagedAgentPhase::Active);
    }

    /// Restores a managed agent whose resume has not been launched yet. See
    /// [`ManagedAgentPhase::AwaitingResume`]; [`Self::begin_managed_agent_resume`]
    /// moves it on once the resume command is typed.
    pub fn restore_managed_agent_for_resume(&mut self, name: String, kind: Agent) {
        self.restore_managed_agent_in(name, kind, ManagedAgentPhase::AwaitingResume);
    }

    pub(super) fn restore_managed_agent_in(
        &mut self,
        name: String,
        kind: Agent,
        phase: ManagedAgentPhase,
    ) {
        self.set_agent_name(name);
        self.agent_name_owner = Some(AgentNameOwner {
            agent_label: crate::detect::agent_label(kind).to_string(),
            session_ref: None,
        });
        self.managed_agent = Some(ManagedAgent { kind, phase });
    }

    /// The restored shell has been handed the resume command: from now on the
    /// managed name holds only until `now + timeout` unless the agent shows
    /// up. A no-op for anything but a restored agent awaiting its resume.
    /// Returns whether the phase changed.
    pub fn begin_managed_agent_resume(&mut self, now: Instant, timeout: Duration) -> bool {
        let Some(managed) = self.managed_agent else {
            return false;
        };
        if managed.phase != ManagedAgentPhase::AwaitingResume {
            return false;
        }
        self.managed_agent = Some(ManagedAgent {
            kind: managed.kind,
            phase: ManagedAgentPhase::Resuming {
                deadline: now.checked_add(timeout).unwrap_or(now),
            },
        });
        true
    }

    /// A process of `agent` was detected in the pane: if a resume of that
    /// agent is waiting for proof, this is it.
    pub(super) fn confirm_managed_agent_resume(&mut self, agent: Agent) {
        if let Some(managed) = self.managed_agent
            && managed.kind == agent
            && matches!(managed.phase, ManagedAgentPhase::Resuming { .. })
        {
            self.managed_agent = Some(ManagedAgent {
                kind: managed.kind,
                phase: ManagedAgentPhase::Active,
            });
        }
    }

    /// Reconciles [`ManagedAgentPhase::Resuming`]. A hook report from the
    /// agent is evidence as good as its process; the seeded restore
    /// detection is not, so `effective_known_agent` alone decides nothing
    /// except a different agent taking the pane.
    pub(super) fn reconcile_managed_agent_resume(
        &mut self,
        kind: Agent,
        deadline: Instant,
        now: Instant,
        process_exited: bool,
    ) -> bool {
        let hook_confirms = self
            .hook_authority
            .as_ref()
            .filter(|authority| self.hook_authority_is_effective(authority))
            .and_then(|authority| crate::detect::parse_agent_label(&authority.agent_label))
            == Some(kind);
        if hook_confirms {
            self.managed_agent = Some(ManagedAgent {
                kind,
                phase: ManagedAgentPhase::Active,
            });
            return true;
        }
        let other_agent = self
            .effective_known_agent()
            .is_some_and(|agent| agent != kind);
        if process_exited || other_agent || now >= deadline {
            self.clear_agent_name();
            return true;
        }
        false
    }

    pub fn clear_agent_name(&mut self) {
        self.prompt_ready_agent = None;
        if self
            .managed_agent_launch_session
            .take()
            .as_ref()
            .is_some_and(|session| self.persisted_agent_session.as_ref() == Some(session))
        {
            self.persisted_agent_session = None;
        }
        self.agent_name = None;
        self.agent_name_owner = None;
        self.managed_agent = None;
    }

    pub fn is_agent_terminal(&self) -> bool {
        self.agent_name.is_some() || self.effective_agent_label().is_some()
    }

    pub(super) fn reconcile_agent_name_owner(
        &mut self,
        agent_label: &str,
        session_ref: Option<&crate::agent::resume::AgentSessionRef>,
    ) {
        if self.agent_name.is_none() {
            return;
        }
        if self.managed_agent.is_some_and(|managed| {
            crate::detect::parse_agent_label(agent_label) == Some(managed.kind)
        }) {
            return;
        }
        match self.agent_name_owner.as_mut() {
            Some(owner)
                if owner.agent_label != agent_label
                    || owner
                        .session_ref
                        .as_ref()
                        .zip(session_ref)
                        .is_some_and(|(current, incoming)| current != incoming) =>
            {
                self.agent_name = None;
                self.agent_name_owner = None;
            }
            Some(owner) if owner.session_ref.is_none() && session_ref.is_some() => {
                owner.session_ref = session_ref.cloned();
            }
            None => {
                self.agent_name_owner = Some(AgentNameOwner {
                    agent_label: agent_label.to_string(),
                    session_ref: session_ref.cloned(),
                });
            }
            _ => {}
        }
    }
}
