use super::*;
use crate::app::events::StateEvent;

impl StateEvent {
    /// The state part of `event`; `None` for the events the App applies itself
    /// (a pane's death, a clipboard write, a Git refresh).
    pub(crate) fn from_app_event(
        event: AppEvent,
        sample: shepr_mux::terminal::state::HookClockSample,
    ) -> Option<Self> {
        match event {
            AppEvent::AgentProcessDetected {
                pane_id,
                agent,
                observed_at,
            } => Some(Self::AgentProcessDetected {
                pane_id,
                agent,
                observed_at,
            }),
            AppEvent::StateChanged {
                pane_id,
                agent,
                state,
                visible_blocker,
                process_exited,
                observed_at,
            } => Some(Self::StateChanged {
                pane_id,
                agent,
                state,
                visible_blocker,
                process_exited,
                observed_at,
            }),
            AppEvent::HookStateReported {
                pane_id,
                origin,
                state,
                seq,
                session_ref,
            } => Some(Self::HookStateReported {
                pane_id,
                sample,
                origin,
                state,
                seq,
                session_ref,
            }),
            AppEvent::AgentSessionReported {
                pane_id,
                origin,
                seq,
                session_ref,
                session_start_source,
            } => Some(Self::AgentSessionReported {
                pane_id,
                sample,
                origin,
                seq,
                session_ref,
                session_start_source,
            }),
            AppEvent::TerminalCwdReported { pane_id, cwd } => {
                Some(Self::TerminalCwdReported { pane_id, cwd })
            }
            AppEvent::Runtime { .. }
            | AppEvent::PaneLaunchSettled { .. }
            | AppEvent::PaneDied { .. }
            | AppEvent::ClipboardWrite { .. }
            | AppEvent::GitStatusRefreshed { .. } => None,
        }
    }

    pub(crate) fn pane_id(&self) -> PaneId {
        match self {
            Self::AgentProcessDetected { pane_id, .. }
            | Self::StateChanged { pane_id, .. }
            | Self::HookStateReported { pane_id, .. }
            | Self::AgentSessionReported { pane_id, .. }
            | Self::TerminalCwdReported { pane_id, .. } => *pane_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn apply_workspace_git_statuses(
        &mut self,
        results: Vec<(WorkspaceGitStatus, Option<std::path::PathBuf>)>,
    ) -> bool {
        let mut changed = false;
        for (result, resolved_identity_cwd) in results {
            let Some(ws_idx) = self
                .workspaces
                .iter()
                .position(|ws| ws.id == result.workspace_id)
            else {
                continue;
            };

            // App resolves the live cwd before entering this pure state reducer
            // so a result is admitted only for the identity the worker saw.
            if resolved_identity_cwd.as_ref() != Some(&result.resolved_identity_cwd) {
                continue;
            }

            let ws = &mut self.workspaces[ws_idx];
            if ws.cached_identity_cwd != result.resolved_identity_cwd {
                ws.cached_identity_cwd = result.resolved_identity_cwd;
            }
            if ws.cached_auto_label != result.auto_label {
                ws.cached_auto_label = result.auto_label;
                changed |= ws.custom_name.is_none();
            }
            if ws.cached_git_status_key != result.status_cache_key {
                ws.cached_git_status_key = result.status_cache_key;
            }
            if ws.cached_git_branch != result.branch {
                ws.cached_git_branch = result.branch;
                changed = true;
            }
            if ws.cached_git_ahead_behind != result.ahead_behind {
                ws.cached_git_ahead_behind = result.ahead_behind;
                changed = true;
            }
        }
        changed
    }

    /// Applies one state-level event and reports what it did to the terminal's
    /// effective agent state.
    pub(crate) fn handle_state_event(&mut self, event: StateEvent) -> StateUpdate {
        match event {
            StateEvent::AgentProcessDetected {
                pane_id,
                agent,
                observed_at,
            } => self.update_terminal_state(pane_id, |terminal| {
                Some(terminal.set_detected_agent_process_at(agent, observed_at))
            }),
            StateEvent::StateChanged {
                pane_id,
                agent,
                state,
                visible_blocker,
                process_exited,
                observed_at,
            } => self.update_terminal_state(pane_id, |terminal| {
                Some(terminal.set_detected_state_with_screen_signals_at(
                    agent,
                    state,
                    visible_blocker,
                    process_exited,
                    observed_at,
                ))
            }),
            StateEvent::HookStateReported {
                pane_id,
                sample,
                origin,
                state,
                seq,
                session_ref,
            } => self.update_terminal_state(pane_id, |terminal| {
                terminal.set_hook_report_at(origin, state, session_ref, seq, sample)
            }),
            StateEvent::AgentSessionReported {
                pane_id,
                sample,
                origin,
                seq,
                session_ref,
                session_start_source,
            } => self.update_terminal_state(pane_id, |terminal| {
                terminal.set_agent_session_ref_for_typed_start_source_at(
                    origin,
                    session_ref,
                    seq,
                    session_start_source,
                    sample,
                )
            }),
            StateEvent::TerminalCwdReported { pane_id, cwd } => {
                let Some(terminal_id) = self.terminal_of(pane_id).cloned() else {
                    return StateUpdate::Unchanged;
                };
                let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
                    return StateUpdate::Unchanged;
                };
                if terminal.cwd() != cwd.as_path() {
                    terminal.set_cwd(cwd);
                    self.mark_session_dirty();
                    self.mark_shell_projection_dirty();
                }
                StateUpdate::Unchanged
            }
        }
    }

    /// Applies `update` to the pane's terminal and reports what it did to the
    /// terminal's effective agent state.
    pub(super) fn update_terminal_state<F>(&mut self, pane_id: PaneId, update: F) -> StateUpdate
    where
        F: FnOnce(&mut shepr_mux::terminal::TerminalState) -> Option<TerminalStateMutation>,
    {
        let Some(terminal_id) = self.terminal_of(pane_id).cloned() else {
            return StateUpdate::Unchanged;
        };
        let (mutation, unchanged_change) = {
            let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
                return StateUpdate::Unchanged;
            };
            let Some(mutation) = update(terminal) else {
                return StateUpdate::Unchanged;
            };
            let unchanged_change = mutation
                .agent_released
                .then(|| terminal.unchanged_effective_state_change());
            (mutation, unchanged_change)
        };
        if mutation.session_ref_changed {
            self.mark_session_dirty();
            self.mark_shell_projection_dirty();
        }
        let agent_released = mutation.agent_released;
        let Some(change) = mutation.effective_state_change.or(unchanged_change) else {
            return StateUpdate::Unchanged;
        };
        self.record_agent_state_change_seq(&terminal_id, &change);
        self.mark_shell_projection_dirty();
        if agent_released {
            StateUpdate::Released
        } else {
            StateUpdate::Changed
        }
    }

    /// Pane status is the current state directly; state-change sequences
    /// observe changes in the presented state so endpoint sorting does not
    /// react to Unknown/Idle transitions that both display as Idle.
    pub(super) fn record_agent_state_change_seq(
        &mut self,
        terminal_id: &shepr_protocol::TerminalId,
        change: &EffectiveStateChange,
    ) {
        if change.previous_state.presentation_state() == change.state.presentation_state() {
            return;
        }
        self.next_agent_state_change_seq += 1;
        if let Some(terminal) = self.terminals.get_mut(terminal_id) {
            terminal.last_agent_state_change_seq = Some(self.next_agent_state_change_seq);
        }
    }

    /// Applies the pane child's exit, observed at `ended_at`, to its terminal:
    /// any agent is released, and a checkpointed exit resolves the identity
    /// the checkpoint saves. It runs for every pane, agent or not: a detector
    /// release just before can have left a pane presenting no agent whose
    /// checkpoint still owes the identity that release removed. Returns
    /// whether that released an agent.
    pub(crate) fn publish_pane_process_exit(
        &mut self,
        pane_id: PaneId,
        exit_reason: shepr_platform::ChildExitReason,
        ended_at: std::time::Instant,
    ) -> bool {
        let update = self.update_terminal_state(pane_id, |terminal| {
            Some(terminal.set_pane_process_exit_at(exit_reason, ended_at))
        });
        update == StateUpdate::Released
    }

    /// The final save after a termination signal: every pane whose agent a
    /// detector release took within the grace of `signaled_at` gets that
    /// identity back for the save, since pane deaths are no longer processed.
    pub(crate) fn adopt_checkpoint_candidates_for_shutdown(
        &mut self,
        signaled_at: std::time::Instant,
    ) {
        let mut adopted = false;
        for terminal in self.terminals.values_mut() {
            adopted |= terminal.adopt_checkpoint_candidate_for_shutdown(signaled_at);
        }
        if adopted {
            self.mark_session_dirty();
        }
    }

    /// State-level tests use this to exercise the pure reducer from an event.
    /// It omits App event admission and runtime effects, so event-path behavior
    /// must be tested through `App::handle_internal_event`.
    #[cfg(test)]
    pub(crate) fn handle_app_event(&mut self, event: AppEvent) -> StateUpdate {
        self.handle_state_event(
            StateEvent::from_app_event(
                event,
                shepr_mux::terminal::state::HookClockSample {
                    monotonic: self.clock_now,
                    wall: std::time::SystemTime::now(),
                },
            )
            .expect("state event"),
        )
    }
}
