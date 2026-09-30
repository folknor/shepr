use super::{App, RenderDemand};
use shepr_agent::detect::{Agent, AgentState};
use shepr_core::layout::PaneId;
use shepr_mux::events::AppEvent;
use std::time::Instant;

/// Events the pure data reducer can apply. Runtime removal, Git completion and
/// clipboard delivery stay at the App boundary, outside this type.
#[derive(Debug)]
pub(crate) enum StateEvent {
    AgentProcessDetected {
        pane_id: PaneId,
        agent: Agent,
        observed_at: Instant,
    },
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        observed_at: Instant,
    },
    HookStateReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    },
    AgentSessionReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        session_start_source: Option<shepr_agent::agent::resume::AgentSessionStartSource>,
    },
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: shepr_mux::UsableCwd,
    },
}

impl App {
    fn live_workspace_identity_cwd(&self, workspace_id: &str) -> Option<std::path::PathBuf> {
        let workspace = self
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == *workspace_id)?;
        let root_pane_cwd = workspace.cwd_for_pane(
            workspace.root_pane(),
            &self.state.terminals,
            &self.terminal_runtimes,
        );
        Some(workspace.resolved_identity_cwd_from_root_pane(root_pane_cwd))
    }

    fn handle_git_status_refreshed(
        &mut self,
        results: Vec<shepr_mux::git::WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, shepr_mux::git::GitStatusCacheEntry)>,
    ) -> bool {
        self.git_refresh.finish(self.clock.now, cache_updates);
        let results = results
            .into_iter()
            .map(|result| {
                let resolved_identity_cwd = self.live_workspace_identity_cwd(&result.workspace_id);
                (result, resolved_identity_cwd)
            })
            .collect();
        let changed = self.state.apply_workspace_git_statuses(results);
        if changed {
            self.state.mark_shell_projection_dirty();
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        changed
    }

    pub(crate) fn handle_internal_event(&mut self, ev: AppEvent) {
        let _ = self.handle_internal_event_with_render_demand(ev);
    }

    pub(crate) fn handle_internal_event_with_render_demand(
        &mut self,
        ev: AppEvent,
    ) -> RenderDemand {
        self.handle_internal_event_inner(ev, false)
    }

    /// Publishes the process exit once, then asks the App's session policy
    /// whether the event must wait for a checkpoint before removal.
    pub(crate) fn prepare_pane_exit(
        &mut self,
        pane_id: shepr_core::layout::PaneId,
        exit_reason: shepr_platform::ChildExitReason,
    ) -> Option<u64> {
        self.publish_pane_process_exit(pane_id);
        if exit_reason.requires_session_checkpoint()
            && self.state.prepare_pane_removal_by_id(pane_id).is_some()
        {
            self.request_pane_exit_checkpoint()
        } else {
            None
        }
    }

    /// Applies an event whose pane-exit publication and checkpoint decision
    /// have already been made by the App.
    pub(crate) fn handle_prepared_pane_exit(&mut self, ev: AppEvent) -> RenderDemand {
        self.handle_internal_event_inner(ev, true)
    }

    fn handle_internal_event_inner(
        &mut self,
        ev: AppEvent,
        pane_exit_prepared: bool,
    ) -> RenderDemand {
        if matches!(&ev, AppEvent::ClipboardWrite { .. }) {
            return RenderDemand::None;
        }

        if let AppEvent::GitStatusRefreshed {
            results,
            cache_updates,
        } = ev
        {
            let changed = self.handle_git_status_refreshed(results, cache_updates);
            return Self::render_demand_if(changed);
        }

        let projection_before = self.state.shell_projection_revision;
        if let AppEvent::PaneDied { pane_id, .. } = &ev
            && !pane_exit_prepared
        {
            self.publish_pane_process_exit(*pane_id);
        }

        let mut removed = false;
        let mut state_changed = false;
        let mut touched_pane = None;
        let session_was_dirty = self.state.session_dirty;
        let pane_removal_plan = if let AppEvent::PaneDied { pane_id, .. } = &ev {
            self.state.prepare_pane_removal_by_id(*pane_id)
        } else {
            None
        };
        let checkpointed_pane_exit = matches!(
            &ev,
            AppEvent::PaneDied {
                exit_reason, ..
            } if exit_reason.requires_session_checkpoint() && pane_removal_plan.is_some()
        );
        // The headless loop prepares and holds checkpointed exits before
        // applying them, so this only reports a direct caller that skipped
        // that step; the pane is still removed.
        if checkpointed_pane_exit && !pane_exit_prepared && !self.pane_exit_checkpoint_settled() {
            tracing::warn!("pane exit reached removal before its session checkpoint settled");
        }

        let terminal_cwd_reported = matches!(ev, AppEvent::TerminalCwdReported { .. });
        let mut detached_terminal_ids = Vec::new();
        if let AppEvent::PaneDied { pane_id, .. } = &ev {
            if let Some(plan) = pane_removal_plan {
                match self.state.commit_pane_removal(&plan) {
                    crate::app::actions::PaneRemovalCommit::Removed(outcome) => {
                        removed = true;
                        detached_terminal_ids = outcome.detached_terminal_ids;
                    }
                    crate::app::actions::PaneRemovalCommit::Stale => {
                        // The plan was made above in this same call, so a
                        // stale one means something in between changed the
                        // workspaces. Nothing was removed.
                        tracing::warn!(
                            pane = pane_id.raw(),
                            workspace_index = plan.workspace_index,
                            "PaneDied removal went stale; the dead pane stays in the layout"
                        );
                    }
                }
            }
        } else if let Some(event) = StateEvent::from_app_event(ev) {
            touched_pane = Some(event.pane_id());
            state_changed =
                self.state.handle_state_event(event) != super::actions::StateUpdate::Unchanged;
        }
        if checkpointed_pane_exit {
            self.finish_checkpointed_pane_exit_after_event(session_was_dirty);
        }
        if let Some(pane_id) = touched_pane {
            self.sync_pane_lifecycle_authority_detection_pause(pane_id);
        }
        let changed =
            removed || state_changed || self.state.shell_projection_revision != projection_before;
        if terminal_cwd_reported && changed {
            self.request_git_identity_refresh(self.clock.now);
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }

        self.shutdown_detached_terminal_runtimes(&detached_terminal_ids);
        if removed {
            self.state.mark_shell_projection_dirty();
        }
        Self::render_demand_if(changed)
    }

    fn publish_pane_process_exit(&mut self, pane_id: shepr_core::layout::PaneId) {
        if self.state.publish_pane_process_exit_if_agent(pane_id) {
            self.sync_pane_lifecycle_authority_detection_pause(pane_id);
            self.state.mark_shell_projection_dirty();
        }
    }

    fn render_demand_if(changed: bool) -> RenderDemand {
        if changed {
            RenderDemand::Full
        } else {
            RenderDemand::None
        }
    }

    fn sync_pane_lifecycle_authority_detection_pause(&self, pane_id: PaneId) {
        let Some(terminal_id) = self
            .state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.terminal_id(pane_id))
        else {
            return;
        };
        if let (Some(terminal), Some(runtime)) = (
            self.state.terminals.get(terminal_id),
            self.terminal_runtimes.get(terminal_id),
        ) {
            runtime.set_full_lifecycle_authority_active(
                terminal.full_lifecycle_hook_authority_active(),
            );
        }
    }

    /// Tells one pane it gained or lost terminal focus. Which panes hold focus
    /// is decided per client on the server (`sync_pane_focus`); a pane with no
    /// live runtime is skipped.
    pub(crate) fn send_pane_focus_event(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
        event: shepr_vt::FocusEvent,
    ) {
        let Some(runtime) = self.state.workspaces.get(ws_idx).and_then(|_| {
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        }) else {
            return;
        };
        runtime.try_send_focus_event(event);
    }
}

#[cfg(test)]
impl App {
    pub(crate) fn handle_internal_event_with_render_impact(&mut self, ev: AppEvent) -> bool {
        self.handle_internal_event_with_render_demand(ev) != RenderDemand::None
    }
}
