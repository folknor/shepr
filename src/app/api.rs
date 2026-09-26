use std::time::Instant;

mod agents;
mod env;
mod layouts;
mod panes;
pub(super) mod responses;
mod session;
mod tabs;
mod workspaces;

use super::{App, api_helpers::pane_agent_status};
use crate::events::AppEvent;

impl App {
    pub(crate) fn handle_internal_event_with_render_impact(&mut self, ev: AppEvent) -> bool {
        match ev {
            AppEvent::GitStatusRefreshed {
                results,
                cache_updates,
            } => self.handle_git_status_refreshed(results, cache_updates),
            AppEvent::TabBarCommandFinished {
                segment_index,
                result,
            } => self.handle_tab_bar_command_finished(segment_index, result),
            ev => {
                self.handle_internal_event(ev);
                true
            }
        }
    }

    fn handle_git_status_refreshed(
        &mut self,
        results: Vec<crate::workspace::WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, crate::workspace::GitStatusCacheEntry)>,
    ) -> bool {
        self.git_refresh_in_flight = false;
        for (key, entry) in cache_updates {
            self.git_status_cache.insert(key, entry);
        }
        if self.git_refresh_due_after_in_flight {
            self.mark_git_status_refresh_due(Instant::now());
            self.git_refresh_due_after_in_flight = false;
        } else {
            self.last_git_remote_status_refresh = Instant::now();
        }
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
        let _ = self.handle_internal_event_with_pane_updates(ev);
    }

    pub(crate) fn handle_internal_event_with_pane_updates(
        &mut self,
        ev: AppEvent,
    ) -> Vec<crate::app::actions::PaneStateUpdate> {
        if matches!(&ev, AppEvent::ClipboardWrite { .. }) {
            return Vec::new();
        }

        if let AppEvent::GitStatusRefreshed {
            results,
            cache_updates,
        } = ev
        {
            self.handle_git_status_refreshed(results, cache_updates);
            return Vec::new();
        }

        if let AppEvent::TabBarCommandFinished {
            segment_index,
            result,
        } = ev
        {
            let _ = self.handle_tab_bar_command_finished(segment_index, result);
            return Vec::new();
        }

        if let AppEvent::PaneDied { pane_id, .. } = &ev
            && let Some(update) = self
                .state
                .publish_pane_process_exit_if_agent(*pane_id, false)
        {
            self.sync_full_lifecycle_authority_detection_pauses();
            self.emit_pane_state_update(&update);
        }

        let checkpointed_pane_exit = matches!(
            &ev,
            AppEvent::PaneDied {
                pane_id,
                exit_reason,
            } if exit_reason.requires_session_checkpoint() && self.find_pane(*pane_id).is_some()
        );
        if checkpointed_pane_exit {
            self.checkpoint_session_before_pane_exit();
        }

        if let AppEvent::PaneDied { pane_id, .. } = &ev
            && let Some((ws_idx, _)) = self.find_pane(*pane_id)
            && let Some(public_pane_id) = self.public_pane_id(ws_idx, *pane_id)
        {
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneExited,
                data: crate::api::schema::EventData::PaneExited {
                    pane_id: public_pane_id,
                    workspace_id: self.public_workspace_id(ws_idx),
                },
            });
        }
        let pane_exit_layout_target = if let AppEvent::PaneDied { pane_id, .. } = &ev {
            self.find_pane(*pane_id).and_then(|(ws_idx, _)| {
                self.layout_update_target_after_pane_removal(ws_idx, *pane_id)
            })
        } else {
            None
        };
        let pane_exit_container_events = if let AppEvent::PaneDied { pane_id, .. } = &ev {
            self.pane_exit_container_events(*pane_id)
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
        let pane_updates = self.state.handle_app_event(ev);
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
        pane_updates
    }

    /// Close events for the tab, and the workspace, that disappear when the
    /// exited `pane_id` was their last pane (see `AppState::handle_pane_died`).
    /// The pane itself is announced by `pane.exited`.
    fn pane_exit_container_events(
        &self,
        pane_id: crate::layout::PaneId,
    ) -> Vec<crate::api::schema::EventEnvelope> {
        let Some((ws_idx, _)) = self.find_pane(pane_id) else {
            return Vec::new();
        };
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        let Some(tab_idx) = ws.find_tab_index_for_pane(pane_id) else {
            return Vec::new();
        };
        if ws
            .tabs
            .get(tab_idx)
            .is_none_or(|tab| tab.layout.pane_count() > 1)
        {
            return Vec::new();
        }
        let events = if ws.tabs.len() <= 1 {
            self.workspace_close_events(ws_idx)
        } else {
            self.tab_close_events(ws_idx, tab_idx)
        };
        events
            .into_iter()
            .filter(|event| event.event != crate::api::schema::EventKind::PaneClosed)
            .collect()
    }

    fn reset_all_agent_detection_runtimes(&self) {
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
        let Some(pane_id) = self.public_pane_id(update.ws_idx, update.pane_id) else {
            return;
        };
        let workspace_id = self.public_workspace_id(update.ws_idx);

        if update.agent_name_changed {
            self.emit_pane_updated(update.ws_idx, update.pane_id);
        }

        if update.previous_agent_label != update.agent_label || update.agent_released {
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneAgentDetected,
                data: crate::api::schema::EventData::PaneAgentDetected {
                    pane_id: pane_id.clone(),
                    workspace_id: workspace_id.clone(),
                    agent: update.agent_label.clone(),
                    released: update.agent_released,
                    final_status: update.agent_release_status,
                },
            });
        }

        let previous_agent_status = pane_agent_status(update.previous_state, update.previous_seen);
        let agent_status = self
            .state
            .workspaces
            .get(update.ws_idx)
            .and_then(|ws| ws.pane_state(update.pane_id))
            .map(|pane| pane_agent_status(update.state, pane.seen))
            .unwrap_or_else(|| pane_agent_status(update.state, update.seen));

        if previous_agent_status != agent_status
            || update.previous_presentation != update.presentation
        {
            let presentation = update.presentation.clone();
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneAgentStatusChanged,
                data: crate::api::schema::EventData::PaneAgentStatusChanged {
                    pane_id,
                    workspace_id,
                    agent_status,
                    agent: update.agent_label.clone(),
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
    /// Every production close reaches one of the emitting paths: keybinding,
    /// context-menu and confirm-dialog closes in the client shell are sent as
    /// `tab.close` / `pane.close` / `workspace.close` over the client-shell
    /// endpoint lane and land in the same API handlers as socket requests,
    /// and a pane whose process exits is covered by
    /// `pane_exit_container_events`. `AppState::close_tab` and
    /// `AppState::close_pane` are test-only; a new direct caller of
    /// `AppState::close_workspace_at` or `Workspace::close_tab` must emit
    /// these events itself.
    pub(super) fn tab_close_events(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Vec<crate::api::schema::EventEnvelope> {
        use crate::api::schema::{EventData, EventEnvelope, EventKind};

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
                event: EventKind::PaneClosed,
                data: EventData::PaneClosed {
                    pane_id,
                    workspace_id: workspace_id.clone(),
                },
            })
            .collect();
        if let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) {
            events.push(EventEnvelope {
                event: EventKind::TabClosed,
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
        use crate::api::schema::{EventData, EventEnvelope, EventKind};

        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        let mut events: Vec<_> = (0..ws.tabs.len())
            .flat_map(|tab_idx| self.tab_close_events(ws_idx, tab_idx))
            .collect();
        events.push(EventEnvelope {
            event: EventKind::WorkspaceClosed,
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
                event: crate::api::schema::EventKind::PaneUpdated,
                data: crate::api::schema::EventData::PaneUpdated { pane },
            });
        }
    }

    pub(crate) fn emit_workspace_token_updated(&mut self, ws_idx: usize) {
        let Some(workspace) = self.workspace_info(ws_idx) else {
            return;
        };
        self.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkspaceMetadataUpdated,
            data: crate::api::schema::EventData::WorkspaceMetadataUpdated { workspace },
        });
    }

    pub(crate) fn sync_focus_events(&mut self) {
        self.sync_focus_events_with_outer_event(None);
    }

    pub(crate) fn accept_current_focus_without_events(&mut self) {
        self.last_focus = self.state.active.and_then(|idx| {
            self.state
                .workspaces
                .get(idx)
                .and_then(|workspace| workspace.focused_pane_id().map(|pane_id| (idx, pane_id)))
        });
    }

    pub(crate) fn accept_current_focus_with_api_events(&mut self) {
        let current_focus = self.state.active.and_then(|idx| {
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
            event: crate::api::schema::EventKind::WorkspaceFocused,
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
                event: crate::api::schema::EventKind::TabFocused,
                data: crate::api::schema::EventData::TabFocused {
                    tab_id,
                    workspace_id: self.public_workspace_id(ws_idx),
                },
            });
        }
        if let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) {
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneFocused,
                data: crate::api::schema::EventData::PaneFocused {
                    pane_id: public_pane_id,
                    workspace_id: self.public_workspace_id(ws_idx),
                },
            });
        }
    }

    fn sync_focus_events_with_outer_event(
        &mut self,
        outer_event: Option<crate::ghostty::FocusEvent>,
    ) {
        let current_focus = self.state.active.and_then(|idx| {
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
            self.send_pane_focus_event(ws_idx, pane_id, crate::ghostty::FocusEvent::Lost);
        }
        if let Some((ws_idx, pane_id)) = current_focus {
            let event = outer_event.unwrap_or_else(|| {
                if self.state.outer_terminal_focus == Some(false) {
                    crate::ghostty::FocusEvent::Lost
                } else {
                    crate::ghostty::FocusEvent::Gained
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
        event: crate::ghostty::FocusEvent,
    ) {
        let Some(runtime) = self.state.workspaces.get(ws_idx).and_then(|_| {
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        }) else {
            return;
        };
        runtime.try_send_focus_event(event);
    }

    #[cfg(test)]
    pub(crate) fn handle_api_request(&mut self, request: crate::api::schema::Request) -> String {
        self.drain_all_internal_events();
        self.handle_api_request_after_internal_events_drained(request)
    }

    pub(crate) fn handle_api_request_after_internal_events_drained(
        &mut self,
        request: crate::api::schema::Request,
    ) -> String {
        self.sync_pending_terminal_titles();
        use crate::api::schema::{Method, ResponseResult, SuccessResponse};

        let method_name = crate::api::api_method_name(&request.method);
        let response = match request.method {
            // Every one of these is answered before a request reaches the app:
            // the API server handles ping, SSH agent leases, subscriptions and
            // waits (including `agent.wait`) on the connection thread and
            // rejects `client_shell.surface.set`; the headless server
            // intercepts window titles and `agent.prompt` before calling this
            // function. Reaching here is a routing bug, reported as such.
            Method::Ping(_)
            | Method::ServerStop(_)
            | Method::ServerSshAgentRegister(_)
            | Method::ClientWindowTitleSet(_)
            | Method::ClientWindowTitleClear(_)
            | Method::ClientShellSurfaceSet(_)
            | Method::AgentPrompt(_)
            | Method::AgentWait(_)
            | Method::EventsSubscribe(_)
            | Method::EventsWait(_)
            | Method::PaneWaitForOutput(_) => {
                tracing::warn!(
                    method = method_name,
                    "api request routed to the app by mistake"
                );
                return responses::encode_error(
                    request.id,
                    "internal_error",
                    format!("{method_name} is not handled by the app"),
                );
            }
            Method::ServerAgentManifests(_) => {
                self.state.refresh_agent_manifest_summaries();
                SuccessResponse {
                    id: request.id,
                    result: ResponseResult::AgentManifestStatus {
                        manifests: self
                            .state
                            .agent_manifest_summaries
                            .clone()
                            .into_iter()
                            .map(agent_manifest_info)
                            .collect(),
                    },
                }
            }
            Method::ServerReloadAgentManifests(_) => {
                let summaries = crate::detect::manifest::reload_manifests();
                self.state.agent_manifest_summaries = summaries.clone();
                self.reset_all_agent_detection_runtimes();
                SuccessResponse {
                    id: request.id,
                    result: ResponseResult::AgentManifestReload {
                        manifests: summaries.into_iter().map(agent_manifest_info).collect(),
                    },
                }
            }
            Method::SessionSnapshot(_) => return self.handle_session_snapshot(request.id),
            Method::WorkspaceList(_) => return self.handle_workspace_list(request.id),
            Method::WorkspaceGet(target) => return self.handle_workspace_get(request.id, &target),
            Method::WorkspaceCreate(params) => {
                return self.handle_workspace_create(request.id, params);
            }
            Method::WorkspaceFocus(target) => {
                return self.handle_workspace_focus(request.id, &target);
            }
            Method::WorkspaceRename(params) => {
                return self.handle_workspace_rename(request.id, params);
            }
            Method::WorkspaceMove(params) => {
                return self.handle_workspace_move(request.id, &params);
            }
            Method::WorkspaceMoveBlock(params) => {
                return self.handle_workspace_move_block(request.id, params);
            }
            Method::WorkspaceReportMetadata(params) => {
                return self.handle_workspace_report_metadata(request.id, params);
            }
            Method::WorkspaceClose(target) => {
                return self.handle_workspace_close(request.id, &target);
            }
            Method::TabList(params) => return self.handle_tab_list(request.id, params),
            Method::TabGet(target) => return self.handle_tab_get(request.id, &target),
            Method::TabCreate(params) => return self.handle_tab_create(request.id, params),
            Method::TabFocus(target) => return self.handle_tab_focus(request.id, &target),
            Method::TabRename(params) => return self.handle_tab_rename(request.id, params),
            Method::TabMove(params) => return self.handle_tab_move(request.id, &params),
            Method::TabClose(target) => return self.handle_tab_close(request.id, &target),
            Method::AgentList(_) => return self.handle_agent_list(request.id),
            Method::AgentGet(target) => return self.handle_agent_get(request.id, &target),
            Method::AgentFocus(target) => return self.handle_agent_focus(request.id, &target),
            Method::AgentRename(params) => return self.handle_agent_rename(request.id, params),
            Method::AgentStart(params) => return self.handle_agent_start(request.id, params),
            Method::AgentRead(params) => return self.handle_agent_read(request.id, &params),
            Method::AgentExplain(target) => return self.handle_agent_explain(request.id, &target),
            Method::AgentSendKeys(params) => {
                return self.handle_agent_send_keys(request.id, &params);
            }
            Method::PaneSplit(params) => return self.handle_pane_split(request.id, params),
            Method::PaneSwap(params) => return self.handle_pane_swap(request.id, params),
            Method::PaneMove(params) => return self.handle_pane_move(request.id, params),
            Method::PaneZoom(params) => return self.handle_pane_zoom(request.id, &params),
            Method::PaneLayout(params) => return self.handle_pane_layout(request.id, &params),
            Method::PaneProcessInfo(params) => {
                return self.handle_pane_process_info(request.id, &params);
            }
            Method::LayoutExport(params) => {
                return self.handle_layout_export(request.id, &params);
            }
            Method::LayoutApply(params) => return self.handle_layout_apply(request.id, &params),
            Method::LayoutSetSplitRatio(params) => {
                return self.handle_layout_set_split_ratio(request.id, params);
            }
            Method::PaneNeighbor(params) => return self.handle_pane_neighbor(request.id, &params),
            Method::PaneEdges(params) => return self.handle_pane_edges(request.id, &params),
            Method::PaneFocusDirection(params) => {
                return self.handle_pane_focus_direction(request.id, &params);
            }
            Method::PaneResize(params) => return self.handle_pane_resize(request.id, &params),
            Method::PaneScroll(params) => return self.handle_pane_scroll(request.id, &params),
            Method::PaneClear(target) => return self.handle_pane_clear(request.id, &target),
            Method::PaneSelectionRead(params) => {
                return self.handle_pane_selection_read(request.id, params);
            }
            Method::PaneCopyMotion(params) => {
                return self.handle_pane_copy_motion(request.id, params);
            }
            Method::PaneCopySearch(params) => {
                return self.handle_pane_copy_search(request.id, params);
            }
            Method::PaneList(params) => return self.handle_pane_list(request.id, &params),
            Method::PaneCurrent(params) => return self.handle_pane_current(request.id, &params),
            Method::PaneGet(target) => return self.handle_pane_get(request.id, &target),
            Method::PaneFocus(target) => return self.handle_pane_focus(request.id, &target),
            Method::PaneInputSet(params) => return self.handle_pane_input_set(request.id, &params),
            Method::PaneRename(params) => return self.handle_pane_rename(request.id, params),
            Method::PaneRead(params) => return self.handle_pane_read(request.id, &params),
            Method::PaneReportAgent(params) => {
                return self.handle_pane_report_agent(request.id, params);
            }
            Method::PaneReportAgentSession(params) => {
                return self.handle_pane_report_agent_session(request.id, params);
            }
            Method::PaneReportMetadata(params) => {
                return self.handle_pane_report_metadata(request.id, params);
            }
            Method::PaneClearAgentAuthority(params) => {
                return self.handle_pane_clear_agent_authority(request.id, params);
            }
            Method::PaneReleaseAgent(params) => {
                return self.handle_pane_release_agent(request.id, params);
            }
            Method::PaneSendText(params) => return self.handle_pane_send_text(request.id, params),
            Method::PaneSendInput(params) => {
                return self.handle_pane_send_input(request.id, &params);
            }
            Method::PaneClose(target) => return self.handle_pane_close(request.id, &target),
            Method::PaneSendKeys(params) => return self.handle_pane_send_keys(request.id, &params),
        };

        responses::encode_success(response.id, response.result)
    }
}

fn agent_manifest_info(
    summary: crate::detect::manifest::AgentManifestSummary,
) -> crate::api::schema::AgentManifestInfo {
    crate::api::schema::AgentManifestInfo {
        agent: crate::detect::agent_label(summary.agent).to_string(),
        source: summary.active_source.label(),
        source_kind: summary.active_source.kind().to_string(),
        warning: summary.warning,
    }
}

#[cfg(test)]
pub(super) mod test_support {
    pub(crate) fn exiting_test_command() -> &'static str {
        "/usr/bin/true"
    }

    pub(crate) fn shutdown_test_runtimes(app: &mut crate::app::App) {
        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            runtime.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Agent, AgentState};

    #[tokio::test]
    async fn server_reload_agent_manifests_resets_detection_runtimes() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("manifest-reload")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let reset_notify = runtime.agent_detection_reset_notify_for_test();
        app.terminal_runtimes.insert(terminal_id, runtime);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "reload_manifests".into(),
            method: crate::api::schema::Method::ServerReloadAgentManifests(
                crate::api::schema::EmptyParams::default(),
            ),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response["result"]["type"], "agent_manifest_reload");
        assert!(
            !response["result"]["manifests"]
                .as_array()
                .expect("test precondition")
                .is_empty()
        );

        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reset_notify.notified(),
        )
        .await
        .expect("manual manifest reload should reset detection runtimes");
    }

    #[tokio::test]
    async fn server_agent_manifests_reports_status_without_resetting_runtimes() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("manifest-status")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let reset_notify = runtime.agent_detection_reset_notify_for_test();
        app.terminal_runtimes.insert(terminal_id, runtime);

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "manifest_status".into(),
            method: crate::api::schema::Method::ServerAgentManifests(
                crate::api::schema::EmptyParams::default(),
            ),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response["result"]["type"], "agent_manifest_status");
        assert!(
            !response["result"]["manifests"]
                .as_array()
                .expect("test precondition")
                .is_empty()
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                reset_notify.notified(),
            )
            .await
            .is_err(),
            "status request should not reset detection runtimes"
        );
    }

    #[tokio::test]
    async fn agent_explain_evaluates_with_server_manifest_cache() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("agent-explain")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .detected_agent = Some(Agent::Codex);
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(
            80,
            24,
            b"press enter to confirm or esc to cancel",
        );
        app.terminal_runtimes.insert(terminal_id, runtime);
        let target = app.public_pane_id(0, pane_id).expect("test precondition");

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "agent_explain".into(),
            method: crate::api::schema::Method::AgentExplain(crate::api::schema::AgentTarget {
                target,
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "agent_explain");
        assert_eq!(response["result"]["explain"]["state"], "blocked");
        assert_eq!(
            response["result"]["explain"]["matched_rule"]["id"],
            "live_strong_blocker"
        );
    }

    #[tokio::test]
    async fn agent_explain_rejects_hook_only_full_lifecycle_authority() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("agent-explain-omp")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_hook_authority(
                "shepr:omp".to_string(),
                "omp".to_string(),
                AgentState::Working,
                None,
                Some(1),
            );
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        app.terminal_runtimes.insert(terminal_id, runtime);
        let target = app.public_pane_id(0, pane_id).expect("test precondition");

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "agent_explain_omp".into(),
            method: crate::api::schema::Method::AgentExplain(crate::api::schema::AgentTarget {
                target,
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["error"]["code"], "agent_not_found");
    }

    #[tokio::test]
    async fn pane_process_info_returns_response_for_existing_pane() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("process-info")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id, runtime);
        let target = app.public_pane_id(0, pane_id).expect("test precondition");

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "process_info".into(),
            method: crate::api::schema::Method::PaneProcessInfo(
                crate::api::schema::PaneProcessInfoParams {
                    pane_id: Some(target.clone()),
                },
            ),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "pane_process_info");
        assert_eq!(response["result"]["process_info"]["pane_id"], target);
    }

    #[test]
    fn methods_answered_before_the_app_are_reported_as_misrouted() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        );

        for method in [
            crate::api::schema::Method::ClientWindowTitleClear(
                crate::api::schema::EmptyParams::default(),
            ),
            crate::api::schema::Method::AgentWait(crate::api::schema::AgentWaitParams {
                target: "reviewer".into(),
                until: Vec::new(),
                timeout_ms: None,
            }),
            crate::api::schema::Method::Ping(crate::api::schema::PingParams::default()),
        ] {
            let name = crate::api::api_method_name(&method);
            let response = app.handle_api_request(crate::api::schema::Request {
                id: "misrouted".into(),
                method,
            });
            let response: serde_json::Value =
                serde_json::from_str(&response).expect("test precondition");
            assert_eq!(response["id"], "misrouted", "{name}");
            assert_eq!(response["error"]["code"], "internal_error", "{name}");
        }
        assert!(!app.state.should_quit);
    }

    #[test]
    fn pane_exit_emits_layout_updated_when_tab_survives() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let mut workspace = crate::workspace::Workspace::test_new("pane-exit-layout");
        let dead_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let tab_id = app.public_tab_id(0, 0).expect("test precondition");

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });

        let events = event_hub.events_after(0);
        let pane_exited = events
            .iter()
            .position(|(_, event)| event.event == crate::api::schema::EventKind::PaneExited)
            .expect("pane.exited should be emitted");
        let layout_updated = events
            .iter()
            .position(|(_, event)| event.event == crate::api::schema::EventKind::LayoutUpdated)
            .expect("layout.updated should be emitted");
        assert!(pane_exited < layout_updated);
        assert!(matches!(
            &events[layout_updated].1.data,
            crate::api::schema::EventData::LayoutUpdated { layout }
                if layout.tab_id == tab_id && layout.panes.len() == 1
        ));
    }

    #[test]
    fn pane_exit_announces_the_tab_and_workspace_it_empties() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let mut workspace = crate::workspace::Workspace::test_new("pane-exit-tab");
        workspace.test_add_tab(Some("second"));
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let first_root = app.state.workspaces[0].tabs[0].root_pane;
        let second_root = app.state.workspaces[0].tabs[1].root_pane;
        let first_tab = app.public_tab_id(0, 0).expect("test precondition");
        let second_tab = app.public_tab_id(0, 1).expect("test precondition");
        let workspace_id = app.public_workspace_id(0);

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: first_root,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });
        // Only the removal events are this test's subject.
        let removals = |hub: &crate::api::EventHub, after: u64| {
            hub.events_after(after)
                .into_iter()
                .map(|(_, event)| event)
                .filter(|event| {
                    matches!(
                        event.event,
                        crate::api::schema::EventKind::PaneExited
                            | crate::api::schema::EventKind::PaneClosed
                            | crate::api::schema::EventKind::TabClosed
                            | crate::api::schema::EventKind::WorkspaceClosed
                    )
                })
                .collect::<Vec<_>>()
        };
        let events = removals(&event_hub, 0);
        assert_eq!(
            events.iter().map(|event| event.event).collect::<Vec<_>>(),
            [
                crate::api::schema::EventKind::PaneExited,
                crate::api::schema::EventKind::TabClosed
            ]
        );
        assert!(matches!(
            &events[1].data,
            crate::api::schema::EventData::TabClosed { tab_id, .. } if tab_id == &first_tab
        ));

        let before = event_hub.current_sequence();
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: second_root,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });
        let events = removals(&event_hub, before);
        assert_eq!(
            events.iter().map(|event| event.event).collect::<Vec<_>>(),
            [
                crate::api::schema::EventKind::PaneExited,
                crate::api::schema::EventKind::TabClosed,
                crate::api::schema::EventKind::WorkspaceClosed
            ]
        );
        assert!(matches!(
            &events[1].data,
            crate::api::schema::EventData::TabClosed { tab_id, .. } if tab_id == &second_tab
        ));
        assert!(matches!(
            &events[2].data,
            crate::api::schema::EventData::WorkspaceClosed { workspace_id: closed, .. }
                if closed == &workspace_id
        ));
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn idle_agent_exit_emits_release_event_without_a_state_change() {
        for agent_name in [None, Some("reviewer")] {
            let event_hub = crate::api::EventHub::default();
            let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &crate::config::Config::default(),
                crate::app::AppPolicy::TEST,
                api_rx,
                event_hub.clone(),
            );
            let workspace = crate::workspace::Workspace::test_new("idle-agent-exit");
            let pane_id = workspace.tabs[0].root_pane;
            let terminal_id = workspace
                .terminal_id(pane_id)
                .cloned()
                .expect("test precondition");
            app.state.workspaces = vec![workspace];
            app.state.ensure_test_terminals();
            let terminal = app
                .state
                .terminals
                .get_mut(&terminal_id)
                .expect("test precondition");
            terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
            if let Some(agent_name) = agent_name {
                terminal.set_agent_name(agent_name.into());
            }

            app.handle_internal_event(AppEvent::StateChanged {
                pane_id,
                agent: Some(Agent::Pi),
                state: AgentState::Idle,
                visible_blocker: false,
                process_exited: true,
                observed_at: std::time::Instant::now(),
            });

            // The release event is this test's subject; the name outliving the
            // observation is pinned by
            // `a_process_exit_observation_alone_does_not_free_the_name`.
            assert_eq!(
                app.state.terminals[&terminal_id].agent_name.as_deref(),
                agent_name
            );
            assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
                &event.data,
                crate::api::schema::EventData::PaneAgentDetected {
                    released: true,
                    final_status: Some(crate::api::schema::AgentStatus::Idle),
                    ..
                }
            )));
        }
    }

    #[test]
    fn process_exit_releases_a_newer_hook_owned_agent() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            event_hub.clone(),
        );
        let workspace = crate::workspace::Workspace::test_new("stale-agent-exit");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let observed_at = std::time::Instant::now();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
        terminal
            .set_hook_authority_at(
                "shepr:codex".into(),
                "codex".into(),
                AgentState::Working,
                None,
                None,
                Some(1),
                observed_at + std::time::Duration::from_secs(1),
            )
            .expect("test precondition");
        terminal.set_agent_name("reviewer".into());

        app.handle_internal_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Codex),
            state: AgentState::Idle,
            visible_blocker: false,
            process_exited: true,
            observed_at,
        });

        let terminal = &app.state.terminals[&terminal_id];
        assert_eq!(terminal.state, AgentState::Idle);
        // Releasing the registration does not free the name yet; a wrong
        // observation must not cost a live agent the handle its owner gave it.
        assert_eq!(terminal.agent_name.as_deref(), Some("reviewer"));
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            event.data,
            crate::api::schema::EventData::PaneAgentDetected { released: true, .. }
        )));
    }
}
