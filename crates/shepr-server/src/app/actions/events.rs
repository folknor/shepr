use super::*;

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn apply_workspace_git_statuses(
        &mut self,
        terminal_runtimes: &shepr_mux::pane::PaneRuntimeRegistry,
        results: Vec<WorkspaceGitStatus>,
    ) -> bool {
        let mut changed = false;
        for result in results {
            let Some(ws_idx) = self
                .workspaces
                .iter()
                .position(|ws| ws.id == result.workspace_id)
            else {
                continue;
            };

            if self.workspaces[ws_idx]
                .resolved_identity_cwd_from(&self.terminals, terminal_runtimes)
                .as_ref()
                != Some(&result.resolved_identity_cwd)
            {
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
            if result.demand.branch && ws.cached_git_branch != result.branch {
                ws.cached_git_branch = result.branch;
                changed = true;
            }
            if result.demand.ahead_behind && ws.cached_git_ahead_behind != result.ahead_behind {
                ws.cached_git_ahead_behind = result.ahead_behind;
                changed = true;
            }
            if ws.cached_git_space != result.space {
                ws.cached_git_space = result.space;
                changed = true;
            }
        }
        changed
    }

    pub fn handle_app_event(&mut self, event: AppEvent) -> Vec<PaneStateUpdate> {
        let now = self.clock_now;
        match event {
            AppEvent::PaneDied { pane_id, .. } => {
                // `App::handle_internal_event` removes dead panes itself,
                // because only it can shut down the detached runtimes.
                tracing::warn!(
                    pane = pane_id.raw(),
                    "PaneDied reached AppState::handle_app_event; the pane is not removed here"
                );
                Vec::new()
            }
            AppEvent::AgentProcessDetected {
                pane_id,
                agent,
                observed_at,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    Some(terminal.set_detected_agent_process_at(agent, observed_at))
                })
                .into_iter()
                .collect(),
            AppEvent::StateChanged {
                pane_id,
                agent,
                state,
                visible_blocker,
                process_exited,
                observed_at,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    Some(terminal.set_detected_state_with_screen_signals_at(
                        agent,
                        state,
                        visible_blocker,
                        process_exited,
                        observed_at,
                    ))
                })
                .into_iter()
                .collect(),
            AppEvent::HookStateReported {
                pane_id,
                source,
                agent_label,
                state,
                message,
                seq,
                session_ref,
            } => {
                if shepr_agent::agent::resume::is_reserved_native_state_source(
                    &source,
                    &agent_label,
                ) {
                    self.update_terminal_state(pane_id, |terminal| {
                        terminal.set_agent_session_ref_at(
                            source,
                            agent_label,
                            session_ref,
                            seq,
                            now,
                        )
                    })
                    .into_iter()
                    .collect()
                } else {
                    self.update_terminal_state(pane_id, |terminal| {
                        terminal.set_hook_authority_at(
                            source,
                            agent_label,
                            state,
                            message,
                            session_ref,
                            seq,
                            now,
                        )
                    })
                    .into_iter()
                    .collect()
                }
            }
            AppEvent::AgentSessionReported {
                pane_id,
                source,
                agent_label,
                seq,
                session_ref,
                session_start_source,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    terminal.set_agent_session_ref_for_typed_start_source_at(
                        source,
                        agent_label,
                        session_ref,
                        seq,
                        session_start_source,
                        now,
                    )
                })
                .into_iter()
                .collect(),
            AppEvent::HookAuthorityCleared {
                pane_id,
                source,
                seq,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    terminal.clear_hook_authority_with_mutation_at(source.as_deref(), seq, now)
                })
                .into_iter()
                .collect(),
            // Handled before this state-only handler, which keeps them for
            // AppEvent exhaustiveness: a clipboard write is a host-local effect
            // the HeadlessServer forwards to the foreground client, and git and
            // tab-bar results are applied by the App's internal-event handler.
            AppEvent::ClipboardWrite { .. }
            | AppEvent::GitStatusRefreshed { .. }
            | AppEvent::TabBarCommandFinished { .. } => Vec::new(),
            AppEvent::TerminalCwdReported { pane_id, cwd } => {
                let Some(terminal_id) = self.workspaces.iter().find_map(|ws| {
                    ws.pane_state(pane_id)
                        .map(|pane| pane.attached_terminal_id.clone())
                }) else {
                    return Vec::new();
                };
                let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
                    return Vec::new();
                };
                if terminal.cwd() != cwd.as_path() {
                    terminal.set_cwd(cwd);
                    self.mark_session_dirty();
                }
                Vec::new()
            }
        }
    }

    pub(super) fn update_terminal_state<F>(
        &mut self,
        pane_id: PaneId,
        update: F,
    ) -> Option<PaneStateUpdate>
    where
        F: FnOnce(&mut shepr_mux::terminal::TerminalState) -> Option<TerminalStateMutation>,
    {
        let ws_idx = self
            .workspaces
            .iter()
            .position(|ws| ws.pane_state(pane_id).is_some())?;
        let workspace_id = self.workspaces[ws_idx].id.clone();
        let terminal_id = self.workspaces[ws_idx]
            .pane_state(pane_id)?
            .attached_terminal_id
            .clone();
        let (mutation, unchanged_change) = {
            let terminal = self.terminals.get_mut(&terminal_id)?;
            let mutation = update(terminal)?;
            let unchanged_change = mutation
                .agent_released
                .then(|| terminal.unchanged_effective_state_change());
            (mutation, unchanged_change)
        };
        if mutation.session_ref_changed {
            self.mark_session_dirty();
        }
        let agent_released = mutation.agent_released;
        let change = mutation.effective_state_change.or(unchanged_change)?;
        self.record_agent_state_change_seq(&terminal_id, &change);
        let update = PaneStateUpdate {
            pane_id,
            workspace_id,
            previous: PaneStateSnapshot {
                agent_label: change.previous_agent_label.clone(),
                known_agent: change.previous_known_agent,
                state: change.previous_state,
            },
            current: PaneStateSnapshot {
                agent_label: if agent_released {
                    change.previous_agent_label.clone()
                } else {
                    change.agent_label.clone()
                },
                known_agent: if agent_released {
                    change.previous_known_agent
                } else {
                    change.known_agent
                },
                state: change.state,
            },
            cause: if agent_released {
                PaneStateCause::Released
            } else {
                PaneStateCause::StateChanged
            },
        };
        Some(update)
    }

    /// The old background-completion predicate only populated the removed
    /// completion sequence; it had no notification caller. Pane status is now
    /// the current state directly, while state-change sequences remain so API
    /// waiters and endpoint agent sorting can observe transitions between
    /// snapshots.
    pub(super) fn record_agent_state_change_seq(
        &mut self,
        terminal_id: &shepr_protocol::TerminalId,
        change: &EffectiveStateChange,
    ) {
        if change.previous_state == change.state {
            return;
        }
        self.next_agent_state_change_seq += 1;
        if let Some(terminal) = self.terminals.get_mut(terminal_id) {
            terminal.last_agent_state_change_seq = Some(self.next_agent_state_change_seq);
        }
    }

    pub(crate) fn publish_pane_process_exit_if_agent(
        &mut self,
        pane_id: PaneId,
    ) -> Option<PaneStateUpdate> {
        let observed_at = self.clock_now;
        let update = self.update_terminal_state(pane_id, |terminal| {
            let agent = terminal.effective_known_agent().or(terminal.detected_agent);
            if agent.is_none() && !terminal.full_lifecycle_hook_authority_active() {
                return None;
            }
            Some(terminal.set_detected_state_with_screen_signals_at(
                agent,
                AgentState::Idle,
                false,
                true,
                observed_at,
            ))
        })?;
        update.cause.released().then_some(update)
    }

    /// Removes a dead pane by id and returns the terminals it detached, whose
    /// runtimes the caller must shut down. State-level tests use it in place
    /// of the App event path.
    #[cfg(test)]
    #[must_use = "the detached terminals' runtimes must be shut down"]
    pub(super) fn handle_pane_died(&mut self, pane_id: PaneId) -> Vec<shepr_protocol::TerminalId> {
        let ws_idx = self
            .workspaces
            .iter()
            .position(|ws| ws.find_tab_index_for_pane(pane_id).is_some());

        let Some(ws_idx) = ws_idx else {
            // Expected, not a fault: a pane already removed because its PTY
            // reader reported a broken terminal core gets a second PaneDied
            // from the child watcher once the child is reaped.
            tracing::debug!(pane = pane_id.raw(), "PaneDied for unknown pane");
            return Vec::new();
        };
        // The pane was just found in this workspace, so a stale plan means
        // the removal logic and the lookup above disagree.
        match self.remove_pane(ws_idx, pane_id) {
            PaneRemovalCommit::Removed(outcome) => outcome.detached_terminal_ids,
            PaneRemovalCommit::Stale => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    workspace_index = ws_idx,
                    "PaneDied removal went stale; the dead pane stays in the layout"
                );
                Vec::new()
            }
        }
    }
}
