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

    /// Retain a saved name until the resumed agent is observed or the resume
    /// expires. The owner carries no session: the first session the resumed
    /// agent reports is adopted rather than compared against the saved one,
    /// so an agent that resumes under a fresh session id keeps its name.
    pub fn hold_agent_name_for_resume(&mut self, name: String, kind: Agent) {
        self.set_agent_name(name);
        self.agent_name_owner = Some(AgentNameOwner {
            agent_label: shepr_agent::detect::agent_label(kind).to_string(),
            session_ref: None,
        });
        self.resume_name_hold = Some(ResumeNameHold {
            kind,
            deadline: None,
        });
    }

    pub fn begin_agent_resume_name_hold(&mut self, now: Instant, timeout: Duration) {
        if let Some(hold) = self.resume_name_hold.as_mut() {
            hold.deadline = Some(now.checked_add(timeout).unwrap_or(now));
        }
    }

    pub fn agent_resume_name_deadline(&self) -> Option<Instant> {
        self.resume_name_hold.and_then(|hold| hold.deadline)
    }

    /// Settles a held name once its resume command has been typed. Until then
    /// (no deadline) no process exists, so nothing observed about the pane can
    /// confirm or refute the agent and the hold is left alone. The seeded
    /// restore detection is deliberately not evidence: only a hook report
    /// from the agent, or its process (`confirm_agent_resume_process`),
    /// confirms it. Returns whether anything changed.
    pub fn reconcile_agent_resume_name(&mut self, now: Instant) -> bool {
        let Some(hold) = self.resume_name_hold else {
            return false;
        };
        let Some(deadline) = hold.deadline else {
            return false;
        };
        let hook_confirms = self
            .hook_authority
            .as_ref()
            .filter(|authority| self.hook_authority_is_effective(authority))
            .and_then(|authority| shepr_agent::detect::parse_agent_label(&authority.agent_label))
            == Some(hold.kind);
        if hook_confirms {
            self.resume_name_hold = None;
            return true;
        }
        let other_agent = self
            .effective_known_agent()
            .is_some_and(|agent| agent != hold.kind);
        if other_agent || now >= deadline {
            self.clear_agent_name();
            return true;
        }
        false
    }

    pub(super) fn confirm_agent_resume_process(&mut self, agent: Agent) {
        if self
            .resume_name_hold
            .is_some_and(|hold| hold.kind == agent && hold.deadline.is_some())
        {
            self.resume_name_hold = None;
        }
    }

    pub fn clear_agent_name(&mut self) {
        self.agent_name = None;
        self.agent_name_owner = None;
        self.resume_name_hold = None;
    }

    pub fn is_agent_terminal(&self) -> bool {
        self.agent_name.is_some() || self.effective_agent_label().is_some()
    }

    pub(super) fn reconcile_agent_name_owner(
        &mut self,
        agent_label: &str,
        session_ref: Option<&shepr_agent::agent::resume::AgentSessionRef>,
    ) {
        if self.agent_name.is_none() {
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
                self.resume_name_hold = None;
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
