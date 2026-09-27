use crate::server::ClientId;

pub(super) enum AltScreenReadConflict {
    None,
    Frozen(shepr_mux::terminal::TerminalReadSnapshot),
    Defer,
}

pub(super) struct AltScreenReadSpec {
    pub(super) terminal_id: shepr_protocol::TerminalId,
    pub(super) lines: usize,
    pub(super) unwrap: bool,
    pub(super) initial: shepr_mux::terminal::ScreenSnapshot,
    pub(super) content_seq: u64,
}

/// Routes API requests and owns requests parked behind alternate-screen reads.
///
/// `HeadlessServer` performs app and terminal effects; this object owns API
/// ordering, focus projection, and deferral around active history reads.
#[derive(Default)]
pub(super) struct ApiDispatcher {
    pending_alt_screen_reads: Vec<crate::server::alt_screen_read::PendingAltScreenRead>,
    deferred_alt_screen_reads: Vec<shepr_api::ApiRequestMessage>,
}

impl ApiDispatcher {
    fn with_server_dispatcher<R>(
        &mut self,
        server: &mut super::HeadlessServer,
        dispatch: impl FnOnce(&mut super::HeadlessServer) -> R,
    ) -> R {
        std::mem::swap(self, &mut server.api_dispatcher);
        let result = dispatch(server);
        std::mem::swap(self, &mut server.api_dispatcher);
        result
    }

    pub(super) fn dispatch_request(
        &mut self,
        server: &mut super::HeadlessServer,
        mut msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        let request_id = msg.request.id.clone();
        let method = msg.request.method.traits().name;
        let target_before = server.default_shell_target();
        let method_claims_geometry =
            super::HeadlessServer::public_request_may_change_geometry(&msg.request.method);
        let explicit_public_focus_target = match &msg.request.method {
            shepr_api::schema::Method::WorkspaceFocus(params) => server
                .app
                .parse_workspace_id(&params.workspace_id)
                .and_then(|workspace_index| {
                    let workspace = server.app.state.workspaces.get(workspace_index)?;
                    crate::ui::TabSurfaceTarget::from_indices(
                        &server.app.state,
                        workspace_index,
                        workspace.active_tab_index(),
                    )
                }),
            shepr_api::schema::Method::TabFocus(params) => server
                .app
                .parse_tab_id(&params.tab_id)
                .and_then(|(workspace_index, tab_index)| {
                    crate::ui::TabSurfaceTarget::from_indices(
                        &server.app.state,
                        workspace_index,
                        tab_index,
                    )
                }),
            shepr_api::schema::Method::PaneFocus(params) => server
                .app
                .parse_pane_id(&params.pane_id)
                .and_then(|(workspace_index, pane_id)| {
                    let workspace = server.app.state.workspaces.get(workspace_index)?;
                    crate::ui::TabSurfaceTarget::from_indices(
                        &server.app.state,
                        workspace_index,
                        workspace.find_tab_index_for_pane(pane_id)?,
                    )
                }),
            _ => None,
        };
        let agent_focus_target = match &msg.request.method {
            shepr_api::schema::Method::AgentFocus(params) => Some(params.target.clone()),
            _ => None,
        };
        let create_focus_requested = match &msg.request.method {
            shepr_api::schema::Method::WorkspaceCreate(params) => params.focus,
            shepr_api::schema::Method::TabCreate(params) => params.focus,
            _ => false,
        };
        let inspect_pane_move = matches!(
            &msg.request.method,
            shepr_api::schema::Method::PaneMove(params) if params.focus
        );
        let response_proxy = (agent_focus_target.is_some() || inspect_pane_move).then(|| {
            let (proxy_tx, proxy_rx) = std::sync::mpsc::channel();
            let original = std::mem::replace(&mut msg.respond_to, proxy_tx);
            (request_id.clone(), method, original, proxy_rx)
        });
        let reconcile =
            super::HeadlessServer::shell_locations_may_need_reconcile(&msg.request.method);
        let changed = self.with_server_dispatcher(server, |server| {
            server.handle_api_request_with_shutdown_check_inner(msg)
        });
        let proxied_result = super::client_views::forward_proxied_api_response(response_proxy);
        let proxied_request_succeeded = proxied_result.is_some();
        // Same-tab and zoomed moves succeed without moving or requesting focus.
        let pane_move_focus_succeeded = inspect_pane_move
            && matches!(
                &proxied_result,
                Some(shepr_api::schema::ResponseResult::PaneMove { move_result }) if move_result.changed
            );
        let successful_agent_focus_target = proxied_request_succeeded
            .then(|| {
                agent_focus_target.as_deref().and_then(|target| {
                    server
                        .app
                        .resolve_agent_target(target)
                        .ok()
                        .and_then(|resolved| {
                            crate::ui::TabSurfaceTarget::from_indices(
                                &server.app.state,
                                resolved.ws_idx,
                                resolved.tab_idx,
                            )
                        })
                })
            })
            .flatten();
        let target_changed = server.default_shell_target() != target_before;
        let explicit_focus_succeeded = successful_agent_focus_target
            .or(explicit_public_focus_target)
            .is_some_and(|target| server.default_shell_target() == Some(target));
        let public_focus_succeeded = explicit_focus_succeeded
            || (create_focus_requested && target_changed)
            || pane_move_focus_succeeded;
        if public_focus_succeeded {
            server.focus_all_shell_clients_on_default_target();
        }
        if reconcile || target_changed || pane_move_focus_succeeded {
            server.reconcile_client_shell_locations();
        }
        let geometry_changed =
            method_claims_geometry && server.reapply_controlled_shell_tab_geometry(false);
        changed | geometry_changed
    }

    pub(super) fn dispatch_shell_request(
        &mut self,
        server: &mut super::HeadlessServer,
        client_id: ClientId,
        msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        let focus_before = server.shell_focus_target(client_id);
        let focused_tabs_before = server.focused_shell_tabs();
        let method_claims_geometry =
            super::HeadlessServer::shell_endpoint_claims_geometry(&msg.request.method);
        let reconcile =
            super::HeadlessServer::shell_locations_may_need_reconcile(&msg.request.method);
        let all_focus_before = reconcile.then(|| server.shell_focus_targets());
        let navigation_changed =
            server.apply_shell_navigation_request(client_id, &msg.request.method);
        server.set_default_shell_target_from_client(client_id);
        let changed = self.with_server_dispatcher(server, |server| {
            server.handle_api_request_with_shutdown_check_inner(msg)
        });
        server.focus_shell_client_on_default_target(client_id);
        if reconcile {
            server.reconcile_client_shell_locations();
        }
        let focus_after = server.shell_focus_target(client_id);
        if let Some(all_focus_before) = all_focus_before {
            server.finish_shell_location_reconciliation(all_focus_before, &focused_tabs_before);
        } else {
            let focused_tabs_after = server.focused_shell_tabs();
            server.app.accept_current_focus_without_events();
            server.send_shell_navigation_focus_events(
                focus_before.as_ref(),
                focus_after.as_ref(),
                &focused_tabs_before,
                &focused_tabs_after,
            );
        }
        if focus_before != focus_after
            && let Some(target) = focus_after
            && let Some(workspace_index) = server
                .app
                .state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == target.workspace_id)
        {
            server
                .app
                .emit_focus_api_events(workspace_index, target.pane_id);
        }
        let geometry_changed = method_claims_geometry
            && if reconcile {
                server.reapply_controlled_shell_tab_geometry(false)
            } else {
                server.claim_shell_tab_geometry(client_id, false)
                    || server.resize_shell_tab_if_controller(client_id, false)
            };
        changed | navigation_changed | geometry_changed
    }

    pub(super) fn process_deferred(&mut self, server: &mut super::HeadlessServer) -> bool {
        let deferred = self.take_deferred();
        let mut changed = false;
        for msg in deferred {
            let conflict = self.alt_screen_read_conflict(server, &msg.request);
            match conflict {
                AltScreenReadConflict::None => changed |= self.dispatch_request(server, msg),
                AltScreenReadConflict::Frozen(_) | AltScreenReadConflict::Defer => {
                    self.defer(msg);
                }
            }
        }
        changed
    }

    pub(super) fn drain_requests(&mut self, server: &mut super::HeadlessServer) -> bool {
        let mut changed = false;
        while !server
            .lifecycle
            .stop_requested(server.app.state.should_quit)
        {
            let Ok(msg) = server.app.api_rx.try_recv() else {
                break;
            };
            changed |= self.dispatch_request(server, msg);
        }
        changed
    }

    pub(super) fn reject_queued_for_shutdown(&mut self, server: &mut super::HeadlessServer) {
        server.app.api_rx.close();
        while let Ok(msg) = server.app.api_rx.try_recv() {
            self.reject_request_for_shutdown(server, &msg);
        }
    }

    pub(super) fn finish_alt_screen_reads_for_shutdown(&mut self, server: &super::HeadlessServer) {
        for msg in self.take_deferred() {
            self.reject_request_for_shutdown(server, &msg);
        }
        for read in self.take_pending_reads() {
            read.finish_for_shutdown();
        }
    }

    pub(super) fn reject_request_for_shutdown(
        &self,
        server: &super::HeadlessServer,
        msg: &shepr_api::ApiRequestMessage,
    ) {
        let error = server.lifecycle.shutdown_error().unwrap_or_else(|| {
            shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::ServerUnavailable,
                "server is shutting down",
            )
            .into_body()
        });
        let request_id = msg.request.id.clone();
        let method = msg.request.method.traits().name;
        let response = Err(shepr_api::error::ApiError::from_body(error));
        shepr_api::send_api_response(&msg.respond_to, &request_id, method, response);
    }

    pub(super) fn next_deadline(&self) -> Option<std::time::Instant> {
        self.pending_alt_screen_reads
            .iter()
            .map(crate::server::alt_screen_read::PendingAltScreenRead::next_deadline)
            .min()
    }

    pub(super) fn has_pending_read_for(&self, terminal_id: &str) -> bool {
        self.pending_alt_screen_reads
            .iter()
            .any(|pending| pending.terminal_id.as_str() == terminal_id)
    }

    pub(super) fn push_pending_read(
        &mut self,
        read: crate::server::alt_screen_read::PendingAltScreenRead,
    ) {
        self.pending_alt_screen_reads.push(read);
    }

    pub(super) fn take_pending_reads(
        &mut self,
    ) -> Vec<crate::server::alt_screen_read::PendingAltScreenRead> {
        std::mem::take(&mut self.pending_alt_screen_reads)
    }

    pub(super) fn defer(&mut self, msg: shepr_api::ApiRequestMessage) {
        self.deferred_alt_screen_reads.push(msg);
    }

    pub(super) fn take_deferred(&mut self) -> Vec<shepr_api::ApiRequestMessage> {
        std::mem::take(&mut self.deferred_alt_screen_reads)
    }

    #[cfg(test)]
    pub(super) fn has_deferred(&self) -> bool {
        !self.deferred_alt_screen_reads.is_empty()
    }

    /// Classifies a request once its public target has been resolved to a
    /// terminal id. Text reads can use the stable initial snapshot; other
    /// formats wait until the active traversal releases the terminal.
    pub(super) fn read_conflict(
        &self,
        terminal_id: Option<&str>,
        request: &shepr_api::schema::Request,
    ) -> AltScreenReadConflict {
        let Some(terminal_id) = terminal_id else {
            return AltScreenReadConflict::None;
        };
        let Some(pending) = self
            .pending_alt_screen_reads
            .iter()
            .find(|pending| pending.terminal_id.as_str() == terminal_id)
        else {
            return AltScreenReadConflict::None;
        };
        let (source, lines, format) = match &request.method {
            shepr_api::schema::Method::AgentRead(params) => {
                (params.source, params.lines, params.format)
            }
            shepr_api::schema::Method::PaneRead(params) => {
                (params.source, params.lines, params.format)
            }
            _ => return AltScreenReadConflict::None,
        };
        if format == shepr_api::schema::ReadFormat::Text {
            AltScreenReadConflict::Frozen(pending.frozen_snapshot(source, lines))
        } else {
            AltScreenReadConflict::Defer
        }
    }

    pub(super) fn alt_screen_read_conflict(
        &self,
        server: &super::HeadlessServer,
        request: &shepr_api::schema::Request,
    ) -> AltScreenReadConflict {
        let terminal_id = match &request.method {
            shepr_api::schema::Method::AgentRead(params) => server
                .app
                .resolve_agent_target(&params.target)
                .ok()
                .map(|target| target.terminal_id.clone()),
            shepr_api::schema::Method::PaneRead(params) => server
                .app
                .resolve_terminal_target(&params.pane_id)
                .ok()
                .map(|target| target.terminal_id.clone()),
            _ => None,
        };
        self.read_conflict(
            terminal_id.as_ref().map(shepr_protocol::TerminalId::as_str),
            request,
        )
    }

    pub(super) fn agent_read_not_idle_error(
        &self,
        server: &super::HeadlessServer,
        request: &shepr_api::schema::Request,
    ) -> Option<shepr_api::schema::ErrorBody> {
        use shepr_api::schema::{Method, ReadFormat, ReadSource};

        let Method::AgentRead(params) = &request.method else {
            return None;
        };
        let requested = params.lines?;
        if params.format != ReadFormat::Text
            || !matches!(
                params.source,
                ReadSource::Recent | ReadSource::RecentUnwrapped
            )
        {
            return None;
        }
        let target = server.app.resolve_agent_target(&params.target).ok()?;
        let terminal = server.app.state.terminals.get(&target.terminal_id)?;
        if terminal.effective_known_agent().is_none()
            || terminal.state == shepr_agent::detect::AgentState::Idle
        {
            return None;
        }
        let runtime = server.app.terminal_runtimes.get(&terminal.id)?;
        let (screen, snapshot) = runtime.screen_text_snapshot()?;
        if screen != shepr_vt::ActiveScreen::Alternate
            || snapshot.rows.len() >= requested.min(1000) as usize
        {
            return None;
        }
        let status = shepr_agent::detect::manifest::agent_state_label(terminal.state);
        Some(shepr_api::error::ApiError::new(
            shepr_api::error::ApiErrorCode::AgentNotIdle,
            format!(
                "cannot read {requested} lines while {} is {status}: its alternate-screen history can only be captured by scrolling while idle. Wait and retry, or use --source visible",
                params.target
            ),
        ).into_body())
    }

    pub(super) fn alt_screen_read_spec(
        &self,
        server: &super::HeadlessServer,
        request: &shepr_api::schema::Request,
    ) -> Option<AltScreenReadSpec> {
        use shepr_api::schema::{Method, ReadFormat, ReadIntent, ReadSource};

        let (target, source, lines, format) = match &request.method {
            Method::AgentRead(params) => (
                server.app.resolve_agent_target(&params.target).ok()?,
                params.source,
                params.lines,
                params.format,
            ),
            Method::PaneRead(params) if params.intent == ReadIntent::Interactive => (
                server.app.resolve_terminal_target(&params.pane_id).ok()?,
                params.source,
                params.lines,
                params.format,
            ),
            _ => return None,
        };
        if format != ReadFormat::Text
            || !matches!(source, ReadSource::Recent | ReadSource::RecentUnwrapped)
        {
            return None;
        }
        let lines = lines.unwrap_or(80).min(1000) as usize;
        if lines == 0
            || server.clients.has_attach_owner(&target.terminal_id)
            || self.has_pending_read_for(target.terminal_id.as_str())
        {
            return None;
        }
        let terminal = server.app.state.terminals.get(&target.terminal_id)?;
        if terminal.effective_known_agent().is_none()
            || terminal.state != shepr_agent::detect::AgentState::Idle
        {
            return None;
        }
        let runtime = server.app.terminal_runtimes.get(&terminal.id)?;
        if runtime.wheel_routing() != Some(shepr_mux::pane::WheelRouting::MouseReport) {
            return None;
        }
        let (screen, initial, content_seq) = runtime.screen_text_snapshot_with_seq()?;
        if screen != shepr_vt::ActiveScreen::Alternate || initial.rows.len() >= lines {
            return None;
        }
        Some(AltScreenReadSpec {
            terminal_id: terminal.id.clone(),
            lines,
            unwrap: source == ReadSource::RecentUnwrapped,
            initial,
            content_seq,
        })
    }

    pub(super) fn poll_pending_reads(
        &mut self,
        server: &super::HeadlessServer,
        now: std::time::Instant,
    ) {
        let pending = self.take_pending_reads();
        for read in pending {
            let runtime = server.app.terminal_runtimes.get(&read.terminal_id);
            let remains_idle = server
                .app
                .state
                .terminals
                .get(&read.terminal_id)
                .is_some_and(|terminal| terminal.state == shepr_agent::detect::AgentState::Idle);
            let attached = server.clients.has_attach_owner(&read.terminal_id);
            let outcome = if remains_idle && !attached {
                read.poll(runtime, now)
            } else {
                read.abort(runtime, now)
            };
            if let Some(read) = outcome {
                self.push_pending_read(read);
            }
        }
    }
}

impl super::HeadlessServer {
    pub(super) fn with_api_dispatcher<R>(
        &mut self,
        operation: impl FnOnce(&mut ApiDispatcher, &mut Self) -> R,
    ) -> R {
        let mut dispatcher = std::mem::take(&mut self.api_dispatcher);
        let result = operation(&mut dispatcher, self);
        self.api_dispatcher = dispatcher;
        result
    }

    pub(super) fn handle_api_request_with_shutdown_check(
        &mut self,
        msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        self.with_api_dispatcher(|dispatcher, server| dispatcher.dispatch_request(server, msg))
    }

    pub(super) fn handle_client_shell_api_request(
        &mut self,
        client_id: ClientId,
        msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        self.with_api_dispatcher(|dispatcher, server| {
            dispatcher.dispatch_shell_request(server, client_id, msg)
        })
    }

    pub(super) fn poll_pending_alt_screen_reads(&mut self, now: std::time::Instant) {
        self.with_api_dispatcher(|dispatcher, server| dispatcher.poll_pending_reads(server, now));
    }

    pub(super) fn process_deferred_alt_screen_reads(&mut self) -> bool {
        self.with_api_dispatcher(ApiDispatcher::process_deferred)
    }

    pub(super) fn drain_api_requests_with_shutdown_check(&mut self) -> bool {
        self.with_api_dispatcher(ApiDispatcher::drain_requests)
    }

    pub(super) fn reject_queued_api_requests_for_shutdown(&mut self) {
        self.with_api_dispatcher(|dispatcher, server| {
            dispatcher.reject_queued_for_shutdown(server);
        });
    }

    pub(super) fn finish_alt_screen_reads_for_shutdown(&mut self) {
        self.with_api_dispatcher(|dispatcher, server| {
            dispatcher.finish_alt_screen_reads_for_shutdown(server);
        });
    }

    pub(super) fn reject_api_request_for_shutdown(&mut self, msg: &shepr_api::ApiRequestMessage) {
        self.with_api_dispatcher(|dispatcher, server| {
            dispatcher.reject_request_for_shutdown(server, msg);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_requests_are_owned_until_the_headless_loop_retries_them() {
        let (respond_to, _response_rx) = std::sync::mpsc::channel();
        let request = shepr_api::schema::Request {
            id: "queued".into(),
            method: shepr_api::schema::Method::Ping(shepr_api::schema::PingParams {}),
        };
        let msg = shepr_api::ApiRequestMessage {
            request,
            respond_to,
        };
        let mut dispatcher = ApiDispatcher::default();

        dispatcher.defer(msg);
        assert!(dispatcher.has_deferred());
        let deferred = dispatcher.take_deferred();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].request.id, "queued");
        assert!(!dispatcher.has_deferred());
    }
}
