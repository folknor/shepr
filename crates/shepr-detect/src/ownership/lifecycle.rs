use super::*;

pub(super) struct EffectiveRow {
    pub(super) agent: Option<Agent>,
    pub(super) state: AgentState,
    pub(super) source: EffectiveStateSource,
}

impl AgentOwnership {
    fn fallback_not_older_than_hook(&self) -> bool {
        self.hook_authority.as_ref().is_none_or(|authority| {
            self.fallback_observed_at
                .is_some_and(|observed_at| authority.reported_at <= observed_at)
        })
    }

    /// The arbitration table has three rows:
    /// - a live full-lifecycle source supplies identity and state;
    /// - other effective hooks supply identity and state, except that a newer
    ///   visible blocker for the same agent supplies Blocked;
    /// - without an effective hook, detection supplies identity and state.
    ///
    /// Process exits withdraw detector identity; suspension does not.
    pub(super) fn effective_row(&self) -> EffectiveRow {
        let hook = self.hook_authority.as_ref().and_then(|authority| {
            let agent = authority.origin.agent();
            let full = authority.origin.is_full_lifecycle();
            (!full
                || (Some(agent) == self.detected_agent && self.process_evidence.exit().is_none()))
            .then_some((authority, agent, full))
        });
        match hook {
            Some((authority, agent, full_lifecycle_hook)) => {
                let visible_blocker = !full_lifecycle_hook
                    && self.fallback_visible_blocker
                    && self.fallback_not_older_than_hook()
                    && Some(agent) == self.detected_agent
                    && authority.state != AgentState::Blocked;
                EffectiveRow {
                    agent: Some(agent),
                    state: if visible_blocker {
                        AgentState::Blocked
                    } else {
                        authority.state
                    },
                    source: if full_lifecycle_hook {
                        EffectiveStateSource::FullLifecycleHook
                    } else if visible_blocker {
                        EffectiveStateSource::Screen
                    } else {
                        EffectiveStateSource::Hook
                    },
                }
            }
            None => EffectiveRow {
                agent: self
                    .process_evidence
                    .exit()
                    .is_none()
                    .then_some(self.detected_agent)
                    .flatten(),
                state: self.fallback_state,
                source: if self.pane_ended || self.process_evidence.exit().is_some() {
                    EffectiveStateSource::ProcessExit
                } else {
                    EffectiveStateSource::Screen
                },
            },
        }
    }

    /// The agent the pane presents: the effective hook's, else the detected
    /// one while its process is present.
    pub fn effective_agent(&self) -> Option<Agent> {
        self.effective_row().agent
    }

    pub fn unchanged_effective_state_change(&self) -> EffectiveStateChange {
        EffectiveStateChange {
            previous_state: self.state,
            state: self.state,
        }
    }

    /// Whether a live full-lifecycle hook owns the state, which pauses screen
    /// detection. Derived from the same row as `state_owner()`, so the pause
    /// and detect explain never disagree.
    pub fn full_lifecycle_hook_authority_active(&self) -> bool {
        self.state_owner() == EffectiveStateSource::FullLifecycleHook
    }
}
