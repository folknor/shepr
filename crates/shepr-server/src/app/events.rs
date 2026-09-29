use super::{App, RenderDemand};
use shepr_mux::events::AppEvent;

impl App {
    fn handle_git_status_refreshed(
        &mut self,
        results: Vec<shepr_mux::git::WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, shepr_mux::git::GitStatusCacheEntry)>,
    ) -> bool {
        self.git_refresh.finish(self.clock.now, cache_updates);
        let changed = self
            .state
            .apply_workspace_git_statuses(&self.terminal_runtimes, results);
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

        if let AppEvent::TabBarCommandFinished {
            segment_index,
            result,
        } = ev
        {
            let changed = self.handle_tab_bar_command_finished(segment_index, result);
            if changed {
                self.state.mark_shell_projection_dirty();
            }
            return Self::render_demand_if(changed);
        }

        if let AppEvent::PaneDied { pane_id, .. } = &ev
            && self.state.publish_pane_process_exit_if_agent(*pane_id)
        {
            self.sync_full_lifecycle_authority_detection_pauses();
        }

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
        // The headless loop holds a checkpointed exit until its save settles
        // (`checkpoint_session_before_pane_exit`), so this only reports a
        // caller that skipped that step; the pane is still removed.
        if checkpointed_pane_exit && !self.pane_exit_checkpoint_settled() {
            tracing::warn!("pane exit reached removal before its session checkpoint settled");
        }

        let terminal_cwd_reported = matches!(ev, AppEvent::TerminalCwdReported { .. });
        let mut detached_terminal_ids = Vec::new();
        if let AppEvent::PaneDied { pane_id, .. } = &ev {
            if let Some(plan) = pane_removal_plan {
                match self.state.commit_pane_removal(&plan) {
                    crate::app::actions::PaneRemovalCommit::Removed(outcome) => {
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
        } else {
            self.state.handle_app_event(ev);
        }
        if checkpointed_pane_exit {
            self.finish_checkpointed_pane_exit();
        }
        self.sync_full_lifecycle_authority_detection_pauses();
        if terminal_cwd_reported {
            self.request_git_identity_refresh(self.clock.now);
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }

        self.shutdown_detached_terminal_runtimes(&detached_terminal_ids);
        self.state.mark_shell_projection_dirty();
        RenderDemand::Full
    }

    fn render_demand_if(changed: bool) -> RenderDemand {
        if changed {
            RenderDemand::Full
        } else {
            RenderDemand::None
        }
    }

    pub(crate) fn sync_full_lifecycle_authority_detection_pauses(&self) {
        for workspace in &self.state.workspaces {
            for tab in workspace.tabs() {
                for pane in tab.panes().values() {
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    let Some(runtime) = self.terminal_runtimes.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    runtime.set_full_lifecycle_authority_active(
                        terminal.full_lifecycle_hook_authority_active(),
                    );
                }
            }
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
