use super::*;
use crate::app::events::StateEvent;

impl StateEvent {
    /// The state part of a runtime payload from `pane_id`; `None` for the
    /// payloads the App applies itself (a pane's death and launch settlement,
    /// a clipboard write).
    pub(crate) fn from_runtime(pane_id: PaneId, event: RuntimeEvent) -> Option<Self> {
        match event {
            RuntimeEvent::AgentProcessDetected { agent, observed_at } => {
                Some(Self::AgentProcessDetected {
                    pane_id,
                    agent,
                    observed_at,
                })
            }
            RuntimeEvent::StateChanged {
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
            RuntimeEvent::TerminalCwdReported { cwd } => {
                Some(Self::TerminalCwdReported { pane_id, cwd })
            }
            RuntimeEvent::PaneLaunchSettled { .. }
            | RuntimeEvent::PaneDied { .. }
            | RuntimeEvent::ClipboardWrite { .. } => None,
        }
    }
}

/// Turns a hook admission outcome into the mutation the reducer applies. A
/// parked report and a rejected one both leave the pane as it was, but they
/// are different answers: a parked one is held by its source until what it
/// awaits arrives (a session start of its agent, or process evidence for its
/// agent before the parked start expires), a rejected one is dropped for the
/// logged reason. The API still answers a hook with success either way, since
/// hooks are fire-and-forget and an out-of-order or superseded report is
/// routine, not a caller error. The terminal's ownership keeps the last such
/// outcome itself, so `detect explain` shows it without this log; the log
/// reads what a parked report awaits from that same record, as of `now`.
fn admit_hook_outcome(
    pane_id: shepr_core::layout::PaneId,
    kind: shepr_detect::ownership::HookReportKind,
    source: &shepr_agent::AgentSource,
    outcome: shepr_detect::ownership::HookOutcome,
    ownership: &shepr_detect::ownership::AgentOwnership,
    now: std::time::Instant,
) -> Option<AgentOwnershipMutation> {
    use shepr_detect::ownership::{ParkedHookAwaiting, UnappliedHookDisposition};
    let report = ownership
        .last_unapplied_hook_report(now)
        .filter(|report| report.origin.source() == source);
    let seq = report.as_ref().and_then(|report| report.seq);
    let session_ref = report
        .as_ref()
        .and_then(|report| report.session_ref.as_ref());
    match &outcome {
        shepr_detect::ownership::HookOutcome::Applied(_) => {}
        shepr_detect::ownership::HookOutcome::Parked => {
            let awaiting = report.as_ref().and_then(|report| match report.disposition {
                UnappliedHookDisposition::Parked(awaiting) => Some(awaiting),
                UnappliedHookDisposition::Rejected(_) => None,
            });
            match awaiting {
                Some(ParkedHookAwaiting::SessionStart) => tracing::debug!(
                    pane = %pane_id,
                    ?kind,
                    %source,
                    seq = ?seq,
                    session_ref = ?session_ref,
                    "hook report parked until a session start of its agent"
                ),
                Some(ParkedHookAwaiting::Process { expires_at }) => tracing::debug!(
                    pane = %pane_id,
                    ?kind,
                    %source,
                    seq = ?seq,
                    session_ref = ?session_ref,
                    expires_in = ?expires_at.saturating_duration_since(now),
                    "hook report parked until process evidence for its agent"
                ),
                // A state report riding a start that cannot carry it: one past
                // its lifetime, or one newer than the report.
                None => tracing::debug!(
                    pane = %pane_id,
                    ?kind,
                    %source,
                    seq = ?seq,
                    session_ref = ?session_ref,
                    "hook report parked behind a start that cannot carry it"
                ),
            }
        }
        shepr_detect::ownership::HookOutcome::Rejected(reason) => {
            if reason.is_integration_fault() {
                shepr_platform::structured_log!(
                    WARN, event = agent.report, outcome = Refused,
                    pane = %pane_id,
                    ?kind,
                    %source,
                    error = %reason,
                    seq = ?seq,
                    session_ref = ?session_ref,
                    "bundled agent integration report violated its contract"
                );
            } else {
                tracing::debug!(
                    pane = %pane_id,
                    ?kind,
                    %source,
                    rejection = %reason,
                    seq = ?seq,
                    session_ref = ?session_ref,
                    "hook report rejected"
                );
            }
        }
    }
    outcome.into_mutation()
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

impl AppState {
    /// Applies each refreshed status to the workspace it was asked for. A
    /// status whose workspace is gone is dropped.
    pub(crate) fn apply_workspace_git_statuses(
        &mut self,
        results: Vec<(WorkspaceGitStatus, Option<std::path::PathBuf>)>,
    ) -> bool {
        let mut changed = false;
        for (result, resolved_identity_cwd) in results {
            let Some(workspace) = self.workspaces.get_mut(&result.owner) else {
                continue;
            };

            changed |= workspace
                .apply_git_status(result.status, resolved_identity_cwd.as_deref())
                .is_changed();
        }
        if changed {
            self.mark_shell_projection_dirty();
        }
        changed
    }

    /// Stores the deferred command with the resume plan. Launching is part of
    /// the shell projection even before the child confirms its cwd.
    pub(crate) fn begin_agent_resume_launch(&mut self, pane_id: PaneId, command: bytes::Bytes) {
        let Some(record) = self.workspaces.pane_mut(pane_id) else {
            return;
        };
        record.terminal_mut().begin_agent_resume_launch(command);
        // The saved plan is untouched until the launch settles, so the
        // persisted session does not change here.
        self.mark_shell_projection_dirty();
    }

    /// Claims only the deferred command; the projected plan stays until the
    /// send succeeds or the launch is abandoned.
    pub(crate) fn take_agent_resume_command(&mut self, pane_id: PaneId) -> Option<bytes::Bytes> {
        self.workspaces
            .pane_mut(pane_id)?
            .terminal_mut()
            .take_agent_resume_command()
    }

    pub(crate) fn finish_agent_resume_launch(&mut self, pane_id: PaneId) {
        let Some(record) = self.workspaces.pane_mut(pane_id) else {
            return;
        };
        record.terminal_mut().clear_agent_resume();
        self.mark_session_dirty();
        self.mark_shell_projection_dirty();
    }

    pub(crate) fn record_pane_start_failure(
        &mut self,
        pane_id: PaneId,
        failure: shepr_mux::terminal::PaneStartFailure,
    ) {
        let Some(record) = self.workspaces.pane_mut(pane_id) else {
            return;
        };
        record.terminal_mut().record_start_failure(failure);
        self.mark_session_dirty();
        self.mark_shell_projection_dirty();
    }

    /// Abandonment changes the resume placeholder even when its agent's
    /// effective state stays idle. Ownership bookkeeping alone cannot see it.
    pub(crate) fn abandon_pane_agent_resume(
        &mut self,
        pane_id: PaneId,
        failure: shepr_mux::terminal::PaneStartFailure,
        now: std::time::Instant,
    ) {
        if self.terminal(pane_id).is_none() {
            return;
        }
        self.update_terminal_state(pane_id, |terminal| {
            Some(terminal.abandon_agent_resume(failure, now))
        });
        self.mark_session_dirty();
        self.mark_shell_projection_dirty();
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
                let withdrawn_authority = terminal
                    .ownership()
                    .hook_authority()
                    .filter(|authority| authority.origin.agent() != agent)
                    .map(|authority| (authority.origin, authority.session_ref.clone()));
                let mutation = terminal
                    .ownership_mut()
                    .set_detected_agent_process_at(agent, observed_at);
                if let Some((origin, session_ref)) = withdrawn_authority
                    && terminal
                        .ownership()
                        .hook_authority()
                        .is_none_or(|current| current.origin != origin)
                {
                    shepr_platform::structured_log!(
                        INFO, event = agent.authority, outcome = Changed,
                        pane = %pane_id,
                        previous_agent = %origin.agent(),
                        detected_agent = %agent,
                        source = %origin.source(),
                        session_ref = ?session_ref,
                        "screen detection withdrew hook authority after identifying another agent"
                    );
                }
                Some(mutation)
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
                let outcome = terminal.ownership_mut().report_hook_outcome_at(
                    origin,
                    state,
                    session_ref,
                    seq,
                    sample,
                );
                admit_hook_outcome(
                    pane_id,
                    shepr_detect::ownership::HookReportKind::State(state),
                    &source,
                    outcome,
                    terminal.ownership(),
                    sample.monotonic,
                )
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
                let outcome = terminal.ownership_mut().report_session_start_outcome_at(
                    &origin,
                    session_ref,
                    seq,
                    session_start_source,
                    sample,
                );
                admit_hook_outcome(
                    pane_id,
                    shepr_detect::ownership::HookReportKind::SessionStart(session_start_source),
                    &source,
                    outcome,
                    terminal.ownership(),
                    sample.monotonic,
                )
            }),
            StateEvent::TerminalCwdReported { pane_id, cwd } => {
                let Some(record) = self.workspaces.pane_mut(pane_id) else {
                    return StateUpdate::Unchanged;
                };
                let terminal = record.terminal_mut();
                if terminal.cwd() != cwd.as_absolute() {
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
        let (mutation, unchanged_change) = {
            let Some(record) = self.workspaces.pane_mut(pane_id) else {
                return StateUpdate::Unchanged;
            };
            let terminal = record.terminal_mut();
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
        self.lifecycle_authority_dirty.insert(pane_id);
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
        self.record_agent_state_change_seq(pane_id, &change);
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
        pane_id: PaneId,
        change: &EffectiveStateChange,
    ) {
        if change.previous_state.presentation_state() == change.state.presentation_state() {
            return;
        }
        self.next_agent_state_change_seq.advance();
        let seq = self.next_agent_state_change_seq;
        if let Some(record) = self.workspaces.pane_mut(pane_id) {
            record
                .terminal_mut()
                .ownership_mut()
                .record_agent_state_change_seq(seq);
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
        ending: shepr_mux::pane::PaneEnding,
        ended_at: std::time::Instant,
    ) -> bool {
        let update = self.update_terminal_state(pane_id, |terminal| {
            Some(
                terminal
                    .ownership_mut()
                    .set_pane_process_exit_at(ending.needs_checkpoint(), ended_at),
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
        for (_, record) in self.workspaces.records_mut() {
            adopted |= record
                .terminal_mut()
                .ownership_mut()
                .adopt_checkpoint_candidate_for_shutdown(signaled_at);
        }
        if adopted {
            self.mark_shell_projection_dirty();
            self.mark_session_dirty();
        }
    }
}
