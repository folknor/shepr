use crate::limits::AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY;
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

impl super::HeadlessServer {
    pub(super) fn handle_api_request_with_shutdown_check(
        &mut self,
        mut msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        if matches!(
            &msg.request.method,
            shepr_api::schema::Method::ServerReloadAgentManifests(_)
        ) {
            self.defer_agent_manifest_reload(msg);
            return false;
        }

        let request_id = msg.request.id.clone();
        let method_traits = msg.request.method.traits();
        let method = method_traits.name;
        let target_before = self.default_shell_target();
        let method_claims_geometry = method_traits.changes_geometry;
        let explicit_public_focus_target = match &msg.request.method {
            shepr_api::schema::Method::WorkspaceFocus(params) => self
                .app
                .parse_workspace_id(&params.workspace_id)
                .and_then(|workspace_index| {
                    let workspace = self.app.state.workspaces.get(workspace_index)?;
                    crate::ui::TabSurfaceTarget::from_indices(
                        &self.app.state,
                        workspace_index,
                        workspace.active_tab_index(),
                    )
                }),
            shepr_api::schema::Method::TabFocus(params) => self
                .app
                .parse_tab_id(&params.tab_id)
                .and_then(|(workspace_index, tab_index)| {
                    crate::ui::TabSurfaceTarget::from_indices(
                        &self.app.state,
                        workspace_index,
                        tab_index,
                    )
                }),
            shepr_api::schema::Method::PaneFocus(params) => self
                .app
                .parse_pane_id(&params.pane_id)
                .and_then(|(workspace_index, pane_id)| {
                    let workspace = self.app.state.workspaces.get(workspace_index)?;
                    crate::ui::TabSurfaceTarget::from_indices(
                        &self.app.state,
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
        let focus_requested = match &msg.request.method {
            shepr_api::schema::Method::WorkspaceCreate(params) => params.focus,
            shepr_api::schema::Method::TabCreate(params) => params.focus,
            shepr_api::schema::Method::LayoutApply(params) => params.focus,
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
        let reconcile = method_traits.changes_topology;
        let changed = self.handle_api_request_with_shutdown_check_inner(msg);
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
                    self.app
                        .resolve_agent_target(target)
                        .ok()
                        .and_then(|resolved| {
                            crate::ui::TabSurfaceTarget::from_indices(
                                &self.app.state,
                                resolved.ws_idx,
                                resolved.tab_idx,
                            )
                        })
                })
            })
            .flatten();
        let target_changed = self.default_shell_target() != target_before;
        let explicit_focus_succeeded = successful_agent_focus_target
            .or(explicit_public_focus_target)
            .is_some_and(|target| self.default_shell_target() == Some(target));
        let public_focus_succeeded = explicit_focus_succeeded
            || (focus_requested && target_changed)
            || pane_move_focus_succeeded;
        if public_focus_succeeded {
            self.focus_all_shell_clients_on_default_target();
        }
        if reconcile || target_changed || pane_move_focus_succeeded {
            self.reconcile_client_shell_locations();
        }
        let geometry_changed =
            method_claims_geometry && self.reapply_controlled_shell_tab_geometry(false);
        changed | geometry_changed
    }

    pub(super) fn handle_client_shell_api_request(
        &mut self,
        client_id: ClientId,
        msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        if matches!(
            &msg.request.method,
            shepr_api::schema::Method::ServerReloadAgentManifests(_)
        ) {
            self.defer_agent_manifest_reload(msg);
            return false;
        }

        let method_traits = msg.request.method.traits();
        let focus_before = self.shell_focus_target(client_id);
        let focused_tabs_before = self.focused_shell_tabs();
        let method_claims_geometry = method_traits.claims_shell_geometry;
        let reconcile = method_traits.changes_topology;
        let all_focus_before = reconcile.then(|| self.shell_focus_targets());
        let navigation_changed =
            self.apply_shell_navigation_request(client_id, &msg.request.method);
        let default_target_changed = self.set_default_shell_target_from_client(client_id);
        let changed = self.handle_api_request_with_shutdown_check_inner(msg);
        self.focus_shell_client_on_default_target(client_id);
        if reconcile {
            self.reconcile_client_shell_locations();
        }
        let focus_after = self.shell_focus_target(client_id);
        if let Some(all_focus_before) = all_focus_before {
            self.finish_shell_location_reconciliation(all_focus_before, &focused_tabs_before);
        } else {
            let focused_tabs_after = self.focused_shell_tabs();
            self.app.accept_current_focus_without_events();
            self.send_shell_navigation_focus_events(
                focus_before.as_ref(),
                focus_after.as_ref(),
                &focused_tabs_before,
                &focused_tabs_after,
            );
        }
        if focus_before != focus_after
            && let Some(target) = focus_after
            && let Some(workspace_index) = self
                .app
                .state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == target.workspace_id)
        {
            self.app
                .emit_focus_api_events(workspace_index, target.pane_id);
        }
        let geometry_changed = method_claims_geometry
            && if reconcile {
                self.reapply_controlled_shell_tab_geometry(false)
            } else {
                self.claim_shell_tab_geometry(client_id, false)
                    || self.resize_shell_tab_if_controller(client_id, false)
            };
        changed | navigation_changed | default_target_changed | geometry_changed
    }

    pub(super) fn process_deferred_alt_screen_reads(&mut self) -> bool {
        let deferred = self.take_deferred_alt_screen_read_requests();
        let mut changed = false;
        for msg in deferred {
            let conflict = self.alt_screen_read_conflict(&msg.request);
            match conflict {
                AltScreenReadConflict::None => {
                    changed |= self.handle_api_request_with_shutdown_check(msg);
                }
                AltScreenReadConflict::Frozen(_) | AltScreenReadConflict::Defer => {
                    self.defer_alt_screen_read_request(msg);
                }
            }
        }
        changed
    }

    pub(super) fn drain_api_requests_with_shutdown_check(&mut self) -> bool {
        let mut changed = false;
        while !self.lifecycle.stop_requested(self.app.state.should_quit) {
            let Ok(msg) = self.app.api_rx.try_recv() else {
                break;
            };
            changed |= self.handle_api_request_with_shutdown_check(msg);
        }
        changed
    }

    pub(super) fn reject_queued_api_requests_for_shutdown(&mut self) {
        self.app.api_rx.close();
        while let Ok(msg) = self.app.api_rx.try_recv() {
            self.reject_api_request_for_shutdown(&msg);
        }
    }

    pub(super) fn finish_alt_screen_reads_for_shutdown(&mut self) {
        for msg in self.take_deferred_alt_screen_read_requests() {
            self.reject_api_request_for_shutdown(&msg);
        }
        for read in self.take_pending_alt_screen_reads() {
            read.finish_for_shutdown();
        }
        let reloads = std::mem::take(&mut self.running_agent_manifest_reload)
            .into_iter()
            .chain(std::mem::take(&mut self.queued_agent_manifest_reloads));
        for msg in reloads {
            self.reject_api_request_for_shutdown(&msg);
        }
    }

    /// Keep manifest parsing and regex compilation off the tokio event loop.
    /// The app updates summaries and resets detection only after the registry
    /// worker has atomically installed its complete replacement. One reload
    /// runs at a time, so registry installs and applied summaries stay in the
    /// same order.
    fn defer_agent_manifest_reload(&mut self, msg: shepr_api::ApiRequestMessage) {
        if self.running_agent_manifest_reload.is_empty() {
            self.running_agent_manifest_reload.push(msg);
            self.start_agent_manifest_reload();
        } else if self.queued_agent_manifest_reloads.len() >= AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY {
            shepr_api::send_api_response(
                &msg.respond_to,
                &msg.request.id,
                msg.request.method.traits().name,
                Err(shepr_api::error::ApiError::new(
                    shepr_api::error::ApiErrorCode::EndpointBusy,
                    format!(
                        "agent manifest reload queue is full (limit {AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY})"
                    ),
                )),
            );
        } else {
            self.queued_agent_manifest_reloads.push(msg);
        }
    }

    fn start_agent_manifest_reload(&mut self) {
        self.agent_manifest_reload_token = self.agent_manifest_reload_token.wrapping_add(1);
        let token = self.agent_manifest_reload_token;
        let config_dir = self.app.paths.config_dir().to_path_buf();
        let completion_tx = self.agent_manifest_reload_tx.clone();
        let reload_task = tokio::task::spawn_blocking(move || {
            shepr_agent::detect::manifest::reload_manifests(&config_dir)
        });
        let _completion_task = tokio::spawn(async move {
            let result = reload_task
                .await
                .map_err(|error| format!("manifest reload worker failed: {error}"));
            if completion_tx
                .send(super::AgentManifestReloadCompletion {
                    request_token: token,
                    result,
                })
                .is_err()
            {
                tracing::debug!("manifest reload completion receiver closed");
            }
        });
    }

    pub(super) fn complete_agent_manifest_reload(
        &mut self,
        completion: super::AgentManifestReloadCompletion,
    ) -> bool {
        if completion.request_token != self.agent_manifest_reload_token
            || self.running_agent_manifest_reload.is_empty()
        {
            return false;
        }
        let answered = std::mem::take(&mut self.running_agent_manifest_reload);

        if self.lifecycle.stop_requested(self.app.state.should_quit) {
            self.initiate_shutdown();
        }
        if self.lifecycle.phase() == super::ShutdownPhase::Stopping {
            for msg in &answered {
                self.reject_api_request_for_shutdown(msg);
            }
            return false;
        }

        let mut changed = self.drain_all_internal_events_with_forwarding();
        if self.lifecycle.stop_requested(self.app.state.should_quit) {
            self.initiate_shutdown();
        }
        if self.lifecycle.phase() == super::ShutdownPhase::Stopping {
            for msg in &answered {
                self.reject_api_request_for_shutdown(msg);
            }
            return changed;
        }

        let response = match completion.result {
            Ok(summaries) => {
                changed = true;
                Ok(self.app.complete_agent_manifest_reload(summaries))
            }
            Err(error) => Err(shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::InternalError,
                format!("agent manifest reload failed: {error}"),
            )),
        };
        for msg in &answered {
            shepr_api::send_api_response(
                &msg.respond_to,
                &msg.request.id,
                msg.request.method.traits().name,
                response.clone(),
            );
        }
        // Apply this run's summaries before a queued worker can install a newer
        // registry. Both changes are observed by detection on the event loop.
        if !self.queued_agent_manifest_reloads.is_empty() {
            self.running_agent_manifest_reload =
                std::mem::take(&mut self.queued_agent_manifest_reloads);
            self.start_agent_manifest_reload();
        }
        changed
    }

    pub(super) fn reject_api_request_for_shutdown(&self, msg: &shepr_api::ApiRequestMessage) {
        let error = self.lifecycle.shutdown_error().unwrap_or_else(|| {
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

    pub(super) fn next_pending_alt_screen_read_deadline(&self) -> Option<std::time::Instant> {
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

    pub(super) fn push_pending_alt_screen_read(
        &mut self,
        read: crate::server::alt_screen_read::PendingAltScreenRead,
    ) {
        self.pending_alt_screen_reads.push(read);
    }

    fn take_pending_alt_screen_reads(
        &mut self,
    ) -> Vec<crate::server::alt_screen_read::PendingAltScreenRead> {
        std::mem::take(&mut self.pending_alt_screen_reads)
    }

    pub(super) fn defer_alt_screen_read_request(&mut self, msg: shepr_api::ApiRequestMessage) {
        self.deferred_alt_screen_reads.push(msg);
    }

    fn take_deferred_alt_screen_read_requests(&mut self) -> Vec<shepr_api::ApiRequestMessage> {
        std::mem::take(&mut self.deferred_alt_screen_reads)
    }

    #[cfg(test)]
    pub(super) fn has_deferred_alt_screen_read_requests(&self) -> bool {
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
        request: &shepr_api::schema::Request,
    ) -> AltScreenReadConflict {
        let terminal_id = match &request.method {
            shepr_api::schema::Method::AgentRead(params) => self
                .app
                .resolve_agent_target(&params.target)
                .ok()
                .map(|target| target.terminal_id.clone()),
            shepr_api::schema::Method::PaneRead(params) => self
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
        let target = self.app.resolve_agent_target(&params.target).ok()?;
        let terminal = self.app.state.terminals.get(&target.terminal_id)?;
        if terminal.effective_known_agent().is_none()
            || terminal.state == shepr_agent::detect::AgentState::Idle
        {
            return None;
        }
        let runtime = self.app.terminal_runtimes.get(&terminal.id)?;
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
        request: &shepr_api::schema::Request,
    ) -> Option<AltScreenReadSpec> {
        use shepr_api::schema::{Method, ReadFormat, ReadIntent, ReadSource};

        let (target, source, lines, format) = match &request.method {
            Method::AgentRead(params) => (
                self.app.resolve_agent_target(&params.target).ok()?,
                params.source,
                params.lines,
                params.format,
            ),
            Method::PaneRead(params) if params.intent == ReadIntent::Interactive => (
                self.app.resolve_terminal_target(&params.pane_id).ok()?,
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
            || self.clients.has_attach_owner(&target.terminal_id)
            || self.has_pending_read_for(target.terminal_id.as_str())
        {
            return None;
        }
        let terminal = self.app.state.terminals.get(&target.terminal_id)?;
        if terminal.effective_known_agent().is_none()
            || terminal.state != shepr_agent::detect::AgentState::Idle
        {
            return None;
        }
        let runtime = self.app.terminal_runtimes.get(&terminal.id)?;
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

    pub(super) fn poll_pending_alt_screen_reads(&mut self, now: std::time::Instant) {
        let pending = self.take_pending_alt_screen_reads();
        for read in pending {
            let runtime = self.app.terminal_runtimes.get(&read.terminal_id);
            let remains_idle = self
                .app
                .state
                .terminals
                .get(&read.terminal_id)
                .is_some_and(|terminal| terminal.state == shepr_agent::detect::AgentState::Idle);
            let attached = self.clients.has_attach_owner(&read.terminal_id);
            let outcome = if remains_idle && !attached {
                read.poll(runtime, now)
            } else {
                read.abort(runtime, now)
            };
            if let Some(read) = outcome {
                self.push_pending_alt_screen_read(read);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn excess_manifest_reloads_are_rejected_when_the_queue_is_full() {
        let mut server = crate::server::headless::tests::test_headless_server();
        let reload = |id: &str| {
            let (respond_to, response_rx) = std::sync::mpsc::channel();
            let msg = shepr_api::ApiRequestMessage {
                request: shepr_api::schema::Request {
                    id: id.into(),
                    method: shepr_api::schema::Method::ServerReloadAgentManifests(
                        shepr_api::schema::EmptyParams::default(),
                    ),
                },
                respond_to,
            };
            (msg, response_rx)
        };

        let (running, _running_rx) = reload("running");
        server.running_agent_manifest_reload.push(running);
        for index in 0..crate::limits::AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY {
            let id = format!("queued-{index}");
            let (msg, _response_rx) = reload(&id);
            assert!(!server.handle_api_request_with_shutdown_check(msg));
        }
        assert_eq!(
            server.queued_agent_manifest_reloads.len(),
            crate::limits::AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY
        );

        let (overflow, response_rx) = reload("overflow");
        assert!(!server.handle_api_request_with_shutdown_check(overflow));
        let error = response_rx
            .recv()
            .expect("full reload queue responds immediately")
            .expect_err("full reload queue must be rejected");
        assert_eq!(error.code, shepr_api::error::ApiErrorCode::EndpointBusy);
        assert_eq!(
            server.queued_agent_manifest_reloads.len(),
            crate::limits::AGENT_MANIFEST_RELOAD_QUEUE_CAPACITY
        );
        crate::server::headless::tests::shutdown_test_runtimes(&mut server);
    }

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
        let mut server = crate::server::headless::tests::test_headless_server();

        server.defer_alt_screen_read_request(msg);
        assert!(server.has_deferred_alt_screen_read_requests());
        let deferred = server.take_deferred_alt_screen_read_requests();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].request.id, "queued");
        assert!(!server.has_deferred_alt_screen_read_requests());
        crate::server::headless::tests::shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn agent_manifest_reload_request_completes_after_background_load() {
        let mut server = crate::server::headless::tests::test_headless_server();
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let request = shepr_api::schema::Request {
            id: "reload-manifests".into(),
            method: shepr_api::schema::Method::ServerReloadAgentManifests(
                shepr_api::schema::EmptyParams::default(),
            ),
        };
        let msg = shepr_api::ApiRequestMessage {
            request,
            respond_to,
        };

        assert!(!server.handle_api_request_with_shutdown_check(msg));
        assert!(response_rx.try_recv().is_err());
        let completion = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server.agent_manifest_reload_rx.recv(),
        )
        .await
        .expect("manifest reload worker should complete")
        .expect("manifest reload completion channel should remain open");
        assert!(server.complete_agent_manifest_reload(completion));

        let response = response_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("manifest reload should answer the request")
            .expect("manifest reload should succeed");
        let shepr_api::schema::ResponseResult::AgentManifestReload { manifests } = response else {
            panic!("expected manifest reload response");
        };
        assert!(!manifests.is_empty());
        crate::server::headless::tests::shutdown_test_runtimes(&mut server);
    }

    async fn next_reload_completion(
        server: &mut crate::server::headless::HeadlessServer,
    ) -> crate::server::headless::AgentManifestReloadCompletion {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server.agent_manifest_reload_rx.recv(),
        )
        .await
        .expect("manifest reload worker should complete")
        .expect("manifest reload completion channel should remain open")
    }

    #[tokio::test]
    async fn a_reload_requested_during_another_waits_for_its_own_run() {
        let mut server = crate::server::headless::tests::test_headless_server();
        let reload = |id: &str| {
            let (respond_to, response_rx) = std::sync::mpsc::channel();
            let msg = shepr_api::ApiRequestMessage {
                request: shepr_api::schema::Request {
                    id: id.into(),
                    method: shepr_api::schema::Method::ServerReloadAgentManifests(
                        shepr_api::schema::EmptyParams::default(),
                    ),
                },
                respond_to,
            };
            (msg, response_rx)
        };
        let (first, first_rx) = reload("first");
        let (second, second_rx) = reload("second");

        assert!(!server.handle_api_request_with_shutdown_check(first));
        assert!(!server.handle_api_request_with_shutdown_check(second));
        let completion = next_reload_completion(&mut server).await;
        assert!(server.complete_agent_manifest_reload(completion));
        assert!(first_rx.try_recv().is_ok_and(|response| response.is_ok()));
        assert!(
            second_rx.try_recv().is_err(),
            "the queued request is answered by the reload started after the first"
        );
        // The queued request's worker starts only once the first run's
        // summaries are applied and answered, under a fresh token.
        assert_eq!(server.agent_manifest_reload_token, 2);
        assert!(server.queued_agent_manifest_reloads.is_empty());
        assert_eq!(
            server
                .running_agent_manifest_reload
                .iter()
                .map(|msg| msg.request.id.as_str())
                .collect::<Vec<_>>(),
            ["second"]
        );

        let completion = next_reload_completion(&mut server).await;
        assert!(server.complete_agent_manifest_reload(completion));
        assert!(second_rx.try_recv().is_ok_and(|response| response.is_ok()));
        crate::server::headless::tests::shutdown_test_runtimes(&mut server);
    }
}
