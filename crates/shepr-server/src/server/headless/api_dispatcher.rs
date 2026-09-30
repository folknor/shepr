impl super::HeadlessServer {
    pub(super) fn handle_api_request_with_shutdown_check(
        &mut self,
        msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        // No socket method moves focus or changes geometry; an internal event
        // drained before the request runs can still change the session's
        // workspaces (a pane dying), which the client locations, the geometry
        // controllers and pane focus follow. That is a topology change: the
        // workspaces present or their order differ from before.
        let topology_before = self.workspace_order();
        let mut changed = self.handle_api_request_with_shutdown_check_inner(msg);
        if self.workspace_order() != topology_before {
            changed |= self.reconcile_client_shell_locations();
            self.reapply_controlled_shell_workspace_geometry(false);
        }
        self.sync_pane_focus();
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

    pub(super) fn reject_api_request_for_shutdown(&self, msg: &shepr_api::ApiRequestMessage) {
        let error = self.lifecycle.shutdown_error().unwrap_or_else(|| {
            shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::ServerUnavailable,
                "server is shutting down",
            )
        });
        let request_id = msg.request.id.clone();
        let method = msg.request.method.traits().name;
        let response = Err(error);
        shepr_api::send_api_response(&msg.respond_to, &request_id, method, response);
    }
}
