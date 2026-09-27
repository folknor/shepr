use std::time::Instant;

use super::{App, api_helpers::pane_agent_status};
use crate::events::AppEvent;

impl App {
    pub(crate) fn handle_internal_event_with_render_demand(
        &mut self,
        ev: AppEvent,
    ) -> crate::api::RenderDemand {
        self.handle_internal_event_with_updates_and_render(ev).1
    }

    #[cfg(test)]
    pub(crate) fn handle_internal_event_with_render_impact(&mut self, ev: AppEvent) -> bool {
        self.handle_internal_event_with_render_demand(ev) != crate::api::RenderDemand::None
    }

    fn handle_git_status_refreshed(
        &mut self,
        results: Vec<crate::workspace::WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, crate::workspace::GitStatusCacheEntry)>,
    ) -> bool {
        self.git_refresh.finish(Instant::now(), cache_updates);
        let changed = self
            .state
            .apply_workspace_git_statuses(&self.terminal_runtimes, results);
        if changed {
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        changed
    }

    pub(crate) fn handle_internal_event(&mut self, ev: AppEvent) {
        let _ = self.handle_internal_event_with_updates_and_render(ev);
    }

    pub(crate) fn handle_internal_event_with_pane_updates(
        &mut self,
        ev: AppEvent,
    ) -> Vec<crate::app::actions::PaneStateUpdate> {
        self.handle_internal_event_with_updates_and_render(ev).0
    }

    fn handle_internal_event_with_updates_and_render(
        &mut self,
        ev: AppEvent,
    ) -> (
        Vec<crate::app::actions::PaneStateUpdate>,
        crate::api::RenderDemand,
    ) {
        if matches!(&ev, AppEvent::ClipboardWrite { .. }) {
            return (Vec::new(), crate::api::RenderDemand::None);
        }

        if let AppEvent::GitStatusRefreshed {
            results,
            cache_updates,
        } = ev
        {
            let changed = self.handle_git_status_refreshed(results, cache_updates);
            return (Vec::new(), Self::render_demand_if(changed));
        }

        if let AppEvent::TabBarCommandFinished {
            segment_index,
            result,
        } = ev
        {
            let changed = self.handle_tab_bar_command_finished(segment_index, result);
            return (Vec::new(), Self::render_demand_if(changed));
        }

        if let AppEvent::PaneDied { pane_id, .. } = &ev
            && let Some(update) = self.state.publish_pane_process_exit_if_agent(*pane_id)
        {
            self.sync_full_lifecycle_authority_detection_pauses();
            self.emit_pane_state_update(&update);
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
        if checkpointed_pane_exit {
            self.checkpoint_session_before_pane_exit();
        }

        if let AppEvent::PaneDied { pane_id, .. } = &ev
            && let Some(plan) = &pane_removal_plan
            && let Some(public_pane_id) = self.public_pane_id(plan.workspace_index, *pane_id)
        {
            self.emit_event(crate::api::schema::EventEnvelope {
                data: crate::api::schema::EventData::PaneExited {
                    pane_id: public_pane_id,
                    workspace_id: self.public_workspace_id(plan.workspace_index),
                },
            });
        }
        let pane_exit_layout_target = if let Some(plan) = &pane_removal_plan {
            (plan.scope == crate::workspace::PaneRemovalScope::Pane)
                .then_some((plan.workspace_index, plan.tab_index))
        } else {
            None
        };
        let pane_exit_container_events = if let Some(plan) = &pane_removal_plan {
            let events = match plan.scope {
                crate::workspace::PaneRemovalScope::Pane => Vec::new(),
                crate::workspace::PaneRemovalScope::Tab => {
                    self.tab_close_events(plan.workspace_index, plan.tab_index)
                }
                crate::workspace::PaneRemovalScope::Workspace => {
                    self.workspace_close_events(plan.workspace_index)
                }
            };
            events
                .into_iter()
                .filter(|event| event.data.kind() != crate::api::schema::EventKind::PaneClosed)
                .collect()
        } else {
            Vec::new()
        };

        let released_agent = if let AppEvent::HookAgentReleased {
            pane_id,
            known_agent,
            ..
        } = &ev
        {
            known_agent.map(|agent| (*pane_id, agent))
        } else {
            None
        };

        let terminal_cwd_reported = matches!(ev, AppEvent::TerminalCwdReported { .. });
        let pane_updates = if matches!(ev, AppEvent::PaneDied { .. }) {
            if let Some(plan) = pane_removal_plan {
                let _ = self.state.commit_pane_removal(&plan);
            }
            Vec::new()
        } else {
            self.state.handle_app_event(ev)
        };
        if checkpointed_pane_exit {
            self.finish_checkpointed_pane_exit();
        }
        if let Some((pane_id, agent)) = released_agent
            && pane_updates.iter().any(|update| update.pane_id == pane_id)
            && let Some((ws_idx, _)) = self.find_pane(pane_id)
            && let Some(runtime) =
                self.state
                    .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        {
            runtime.begin_graceful_release(agent);
        }
        self.sync_full_lifecycle_authority_detection_pauses();
        if terminal_cwd_reported {
            self.request_git_identity_refresh(Instant::now());
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        for update in &pane_updates {
            self.emit_pane_state_update(update);
        }
        self.sync_agent_metadata_deadline();
        if let Some((ws_idx, tab_idx)) = pane_exit_layout_target {
            self.emit_layout_updated_event(ws_idx, tab_idx);
        }
        self.emit_events(pane_exit_container_events);

        self.shutdown_detached_terminal_runtimes();
        (pane_updates, crate::api::RenderDemand::Full)
    }

    fn render_demand_if(changed: bool) -> crate::api::RenderDemand {
        if changed {
            crate::api::RenderDemand::Full
        } else {
            crate::api::RenderDemand::None
        }
    }

    pub(super) fn reset_all_agent_detection_runtimes(&self) {
        for runtime in self.terminal_runtimes.values() {
            runtime.reset_agent_detection();
        }
    }

    fn sync_full_lifecycle_authority_detection_pauses(&self) {
        for workspace in &self.state.workspaces {
            for tab in &workspace.tabs {
                for pane in tab.panes.values() {
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

    pub(crate) fn emit_pane_state_update(&mut self, update: &crate::app::actions::PaneStateUpdate) {
        // Workspace positions can change between state mutation and event emission. Resolve the
        // stable workspace identity carried by the update before building public IDs.
        let Some(ws_idx) = self.parse_workspace_id(&update.workspace_id) else {
            return;
        };
        let Some(pane_id) = self.public_pane_id(ws_idx, update.pane_id) else {
            return;
        };
        let workspace_id = update.workspace_id.clone();

        if update.cause.name_changed() {
            self.emit_pane_updated(ws_idx, update.pane_id);
        }

        if update.previous.agent_label != update.current.agent_label || update.cause.released() {
            self.emit_event(crate::api::schema::EventEnvelope {
                data: crate::api::schema::EventData::PaneAgentDetected {
                    pane_id: pane_id.clone(),
                    workspace_id: workspace_id.clone(),
                    agent: update.current.agent_label.clone(),
                    released: update.cause.released(),
                    final_status: update
                        .cause
                        .released()
                        .then(|| pane_agent_status(update.current.state)),
                },
            });
        }

        let previous_agent_status = pane_agent_status(update.previous.state);
        let agent_status = pane_agent_status(update.current.state);

        if previous_agent_status != agent_status
            || update.previous.presentation != update.current.presentation
        {
            let presentation = update.current.presentation.clone();
            self.emit_event(crate::api::schema::EventEnvelope {
                data: crate::api::schema::EventData::PaneAgentStatusChanged {
                    pane_id,
                    workspace_id,
                    agent_status,
                    agent: update.current.agent_label.clone(),
                    title: presentation.title,
                    display_agent: presentation.display_agent,
                    state_labels: presentation.state_labels,
                },
            });
        }
    }

    pub(super) fn emit_event(&mut self, event: crate::api::schema::EventEnvelope) {
        self.event_hub.push(event);
    }

    pub(super) fn emit_events(&mut self, events: Vec<crate::api::schema::EventEnvelope>) {
        for event in events {
            self.emit_event(event);
        }
    }

    /// Close events for a tab and every pane in it, panes first. Removing a
    /// container has to announce each child it takes along: `agent.wait` and
    /// `agent.prompt --wait` end on their pane's `pane.closed`, and subscribers
    /// rebuild the model from these events. Public ids stop resolving once
    /// the tab is gone, so build these before removing it and emit them after.
    ///
    /// The app prepares these while ids still resolve, then emits them after
    /// the state command removes the tab or workspace.
    pub(super) fn tab_close_events(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Vec<crate::api::schema::EventEnvelope> {
        use crate::api::schema::{EventData, EventEnvelope};

        let Some(tab) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs.get(tab_idx))
        else {
            return Vec::new();
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        let mut events: Vec<_> = tab
            .layout
            .pane_ids()
            .into_iter()
            .filter_map(|pane_id| self.public_pane_id(ws_idx, pane_id))
            .map(|pane_id| EventEnvelope {
                data: EventData::PaneClosed {
                    pane_id,
                    workspace_id: workspace_id.clone(),
                },
            })
            .collect();
        if let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) {
            events.push(EventEnvelope {
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id,
                },
            });
        }
        events
    }

    /// Close events for a workspace and everything in it, children first; see
    /// `tab_close_events`.
    pub(super) fn workspace_close_events(
        &self,
        ws_idx: usize,
    ) -> Vec<crate::api::schema::EventEnvelope> {
        use crate::api::schema::{EventData, EventEnvelope};

        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        let mut events: Vec<_> = (0..ws.tabs.len())
            .flat_map(|tab_idx| self.tab_close_events(ws_idx, tab_idx))
            .collect();
        events.push(EventEnvelope {
            data: EventData::WorkspaceClosed {
                workspace_id: self.public_workspace_id(ws_idx),
                workspace: self.workspace_info(ws_idx),
            },
        });
        events
    }

    pub(crate) fn emit_pane_updated(&mut self, ws_idx: usize, pane_id: crate::layout::PaneId) {
        if let Some(pane) = self.pane_info(ws_idx, pane_id) {
            self.emit_event(crate::api::schema::EventEnvelope {
                data: crate::api::schema::EventData::PaneUpdated { pane },
            });
        }
    }

    pub(crate) fn emit_workspace_token_updated(&mut self, ws_idx: usize) {
        let Some(workspace) = self.workspace_info(ws_idx) else {
            return;
        };
        self.event_hub.push(crate::api::schema::EventEnvelope {
            data: crate::api::schema::EventData::WorkspaceMetadataUpdated { workspace },
        });
    }

    pub(crate) fn sync_focus_events(&mut self) {
        self.sync_focus_events_with_outer_event(None);
    }

    pub(crate) fn accept_current_focus_without_events(&mut self) {
        self.last_focus = self.state.active_index().and_then(|idx| {
            self.state
                .workspaces
                .get(idx)
                .and_then(|workspace| workspace.focused_pane_id().map(|pane_id| (idx, pane_id)))
        });
    }

    pub(crate) fn accept_current_focus_with_api_events(&mut self) {
        let current_focus = self.state.active_index().and_then(|idx| {
            self.state
                .workspaces
                .get(idx)
                .and_then(|workspace| workspace.focused_pane_id().map(|pane_id| (idx, pane_id)))
        });
        if current_focus == self.last_focus {
            return;
        }
        self.last_focus = current_focus;
        if let Some((ws_idx, pane_id)) = current_focus {
            self.emit_focus_api_events(ws_idx, pane_id);
        }
    }

    pub(crate) fn emit_focus_api_events(&mut self, ws_idx: usize, pane_id: crate::layout::PaneId) {
        self.emit_event(crate::api::schema::EventEnvelope {
            data: crate::api::schema::EventData::WorkspaceFocused {
                workspace_id: self.public_workspace_id(ws_idx),
            },
        });
        if let Some(tab_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| self.public_tab_id(ws_idx, ws.active_tab))
        {
            self.emit_event(crate::api::schema::EventEnvelope {
                data: crate::api::schema::EventData::TabFocused {
                    tab_id,
                    workspace_id: self.public_workspace_id(ws_idx),
                },
            });
        }
        if let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) {
            self.emit_event(crate::api::schema::EventEnvelope {
                data: crate::api::schema::EventData::PaneFocused {
                    pane_id: public_pane_id,
                    workspace_id: self.public_workspace_id(ws_idx),
                },
            });
        }
    }

    fn sync_focus_events_with_outer_event(&mut self, outer_event: Option<crate::vt::FocusEvent>) {
        let current_focus = self.state.active_index().and_then(|idx| {
            self.state
                .workspaces
                .get(idx)
                .and_then(|ws| ws.focused_pane_id().map(|pane_id| (idx, pane_id)))
        });
        if current_focus == self.last_focus {
            if let (Some((ws_idx, pane_id)), Some(event)) = (current_focus, outer_event) {
                self.send_pane_focus_event(ws_idx, pane_id, event);
            }
            return;
        }

        if let Some((ws_idx, pane_id)) = self.last_focus {
            self.send_pane_focus_event(ws_idx, pane_id, crate::vt::FocusEvent::Lost);
        }
        if let Some((ws_idx, pane_id)) = current_focus {
            let event = outer_event.unwrap_or_else(|| {
                if self.state.outer_terminal_focus == Some(false) {
                    crate::vt::FocusEvent::Lost
                } else {
                    crate::vt::FocusEvent::Gained
                }
            });
            self.send_pane_focus_event(ws_idx, pane_id, event);
            self.emit_focus_api_events(ws_idx, pane_id);
        }

        self.last_focus = current_focus;
    }

    pub(crate) fn send_pane_focus_event(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        event: crate::vt::FocusEvent,
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
