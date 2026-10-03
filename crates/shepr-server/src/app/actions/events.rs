use super::*;
use crate::app::events::StateEvent;

impl StateEvent {
    /// The state part of `event`; `None` for the events the App applies itself
    /// (a pane's death, a clipboard write, a Git refresh).
    pub(crate) fn from_app_event(event: AppEvent) -> Option<Self> {
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
                detection,
                process_exited,
                observed_at,
            } => Some(Self::StateChanged {
                pane_id,
                agent,
                detection,
                process_exited,
                observed_at,
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
}

#[derive(Debug, Clone, Copy)]
enum HookReportKind {
    State,
    SessionStart,
}

/// Turns a hook admission outcome into the mutation the reducer applies. A
/// parked report and a rejected one both leave the pane as it was, but they
/// are different answers: a parked one waits for process evidence, a
/// rejected one is dropped for the logged reason. The API still answers a
/// hook with success either way, since hooks are fire-and-forget and an
/// out-of-order or superseded report is routine, not a caller error.
fn admit_hook_outcome(
    pane_id: shepr_core::layout::PaneId,
    kind: HookReportKind,
    source: &shepr_agent::AgentSource,
    outcome: shepr_detect::ownership::HookOutcome,
) -> Option<AgentOwnershipMutation> {
    match &outcome {
        shepr_detect::ownership::HookOutcome::Applied(_) => {}
        shepr_detect::ownership::HookOutcome::Parked => tracing::debug!(
            pane = pane_id.raw(),
            ?kind,
            %source,
            "hook report parked until process evidence"
        ),
        shepr_detect::ownership::HookOutcome::Rejected(reason) => tracing::debug!(
            pane = pane_id.raw(),
            ?kind,
            %source,
            ?reason,
            "hook report rejected"
        ),
    }
    outcome.into_mutation()
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

            changed |= self.workspaces[ws_idx]
                .admit_git_status(result, resolved_identity_cwd.as_deref())
                .is_changed();
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
                Some(
                    terminal
                        .ownership_mut()
                        .set_detected_agent_process_at(agent, observed_at),
                )
            }),
            StateEvent::StateChanged {
                pane_id,
                agent,
                detection,
                process_exited,
                observed_at,
            } => self.update_terminal_state(pane_id, |terminal| {
                Some(
                    terminal
                        .ownership_mut()
                        .set_detected_state_with_screen_signals_at(
                            agent,
                            detection.state(),
                            detection.visible_blocker(),
                            process_exited,
                            observed_at,
                        ),
                )
            }),
            StateEvent::HookStateReported {
                pane_id,
                sample,
                origin,
                state,
                seq,
                session_ref,
            } => self.update_terminal_state(pane_id, |terminal| {
                let source = *origin.source();
                let outcome =
                    terminal.report_hook_outcome_at(origin, state, session_ref, seq, sample);
                admit_hook_outcome(pane_id, HookReportKind::State, &source, outcome)
            }),
            StateEvent::AgentSessionReported {
                pane_id,
                sample,
                origin,
                seq,
                session_ref,
                session_start_source,
            } => self.update_terminal_state(pane_id, |terminal| {
                let source = *origin.source();
                let outcome = terminal.report_session_start_outcome_at(
                    &origin,
                    session_ref,
                    seq,
                    session_start_source,
                    sample,
                );
                admit_hook_outcome(pane_id, HookReportKind::SessionStart, &source, outcome)
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
    pub(in crate::app) fn update_terminal_state<F>(
        &mut self,
        pane_id: PaneId,
        update: F,
    ) -> StateUpdate
    where
        F: FnOnce(&mut shepr_mux::terminal::TerminalState) -> Option<AgentOwnershipMutation>,
    {
        let Some(terminal_id) = self.terminal_of(pane_id).cloned() else {
            return StateUpdate::Unchanged;
        };
        let (mutation, unchanged_change) = {
            let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
                return StateUpdate::Unchanged;
            };
            let mutation = update(terminal);
            let unchanged_change = mutation
                .as_ref()
                .is_some_and(|mutation| mutation.agent_released)
                .then(|| terminal.ownership().unchanged_effective_state_change());
            (mutation, unchanged_change)
        };
        // Every update that ran re-syncs the runtime's detector pause, whatever
        // it returned. Relying on the mutation to report authority changes
        // would lose one made by an update that returns `None` or reports no
        // state change; the drain reads the live authority instead.
        self.lifecycle_authority_dirty.insert(terminal_id.clone());
        let Some(mutation) = mutation else {
            return StateUpdate::Unchanged;
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
            terminal
                .ownership_mut()
                .record_agent_state_change_seq(self.next_agent_state_change_seq);
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
            Some(
                terminal
                    .ownership_mut()
                    .set_pane_process_exit_at(exit_reason, ended_at),
            )
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
            adopted |= terminal
                .ownership_mut()
                .adopt_checkpoint_candidate_for_shutdown(signaled_at);
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
        self.handle_state_event(StateEvent::from_app_event(event).expect("state event"))
    }
}
