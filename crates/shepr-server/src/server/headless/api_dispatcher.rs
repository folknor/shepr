use crate::server::ClientId;

impl super::HeadlessServer {
    pub(super) fn handle_api_request_with_shutdown_check(
        &mut self,
        msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        let method_traits = msg.request.method.traits();
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
        let focus_requested = match &msg.request.method {
            shepr_api::schema::Method::WorkspaceCreate(params) => params.focus,
            shepr_api::schema::Method::TabCreate(params) => params.focus,
            _ => false,
        };
        let reconcile = method_traits.changes_topology;
        let changed = self.handle_api_request_with_shutdown_check_inner(msg);
        let target_changed = self.default_shell_target() != target_before;
        let explicit_focus_succeeded = explicit_public_focus_target
            .is_some_and(|target| self.default_shell_target() == Some(target));
        let public_focus_succeeded =
            explicit_focus_succeeded || (focus_requested && target_changed);
        if public_focus_succeeded {
            self.focus_all_shell_clients_on_default_target();
        }
        if reconcile || target_changed {
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
            self.app.accept_current_focus();
            self.send_shell_navigation_focus_events(
                focus_before.as_ref(),
                focus_after.as_ref(),
                &focused_tabs_before,
                &focused_tabs_after,
            );
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
}
