use super::*;
use crate::server::ClientId;
use shepr_api::error::{ApiError, ApiErrorCode};
use shepr_protocol::command::{EndpointCommand, EndpointReply};

impl HeadlessServer {
    pub(super) fn handle_client_shell_endpoint_request(
        &mut self,
        client_id: ClientId,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        command: EndpointCommand,
    ) -> bool {
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        let shell = client.shell_state();
        if boot_id != self.client_shell_boot_id {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                ApiErrorCode::StaleBoot,
                "endpoint command targeted an earlier server boot",
            );
            self.send_to_client(client_id, &message);
            return false;
        }
        let surface_active = shell.surface_active;
        if let EndpointCommand::ClientShellSurfaceSet(params) = &command {
            let Some((changed, projection_revision)) =
                self.set_client_shell_surface_active(client_id, params.active)
            else {
                return false;
            };
            self.send_to_client(
                client_id,
                &crate::server::client_commands::response_message(
                    boot_id,
                    request_id,
                    Ok(EndpointReply::ClientShellSurfaceSet {
                        active: params.active,
                        projection_revision,
                    }),
                ),
            );
            return changed;
        }
        if shell.endpoint_command_in_flight {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                ApiErrorCode::EndpointBusy,
                "this endpoint is still processing another command",
            );
            self.send_to_client(client_id, &message);
            return false;
        }
        if !surface_active {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                ApiErrorCode::SurfaceInactive,
                "this method requires an active client shell surface",
            );
            self.send_to_client(client_id, &message);
            return false;
        }

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        if let Err(err) = crate::server::client_commands::spawn_response_waiter(
            client_id,
            boot_id.clone(),
            request_id.clone(),
            response_rx,
            self.server_event_tx.clone(),
        ) {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                ApiErrorCode::ServerUnavailable,
                format!("failed to start endpoint response bridge: {err}"),
            );
            self.send_to_client(client_id, &message);
            return false;
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.shell_state_mut().endpoint_command_in_flight = true;
        }
        let foreground_changed = self.promote_client_to_foreground(client_id);
        let (changed, result) = self.handle_client_shell_command(client_id, command);
        // The waiter hands the answer back to the loop, so it goes out after
        // this command's render. A failed send means the waiter is gone, and
        // its own failure path already answered.
        if respond_to.send(result).is_err() {
            tracing::debug!(
                ?client_id,
                ?request_id,
                "endpoint response waiter ended before the command answered"
            );
        }
        foreground_changed | changed
    }

    /// Runs one client-shell command for `client_id`: moves that client's own
    /// location for a focus command, runs the command in the app, and then
    /// reconciles shell locations, the tab geometry and the pane focus reports
    /// the command may have changed. Returns whether a render is needed, and
    /// the command's answer.
    pub(super) fn handle_client_shell_command(
        &mut self,
        client_id: ClientId,
        command: EndpointCommand,
    ) -> (bool, Result<EndpointReply, ApiError>) {
        let traits = command.traits();
        let reconcile = traits.changes_topology;
        let navigation_changed = self.apply_shell_navigation_command(client_id, &command);
        let default_target_changed = self.set_default_shell_target_from_client(client_id);
        let (changed, result) = self.run_endpoint_command_in_app(command);
        self.focus_shell_client_on_default_target(client_id);
        if reconcile {
            self.reconcile_client_shell_locations();
        }
        let geometry_changed = traits.claims_shell_geometry
            && if reconcile {
                self.reapply_controlled_shell_tab_geometry(false)
            } else {
                self.claim_shell_tab_geometry(client_id, false)
                    || self.resize_shell_tabs_sized_for(client_id, false)
            };
        self.sync_pane_focus();
        (
            changed | navigation_changed | default_target_changed | geometry_changed,
            result,
        )
    }

    /// A focus command moves the requesting client's own location first, so
    /// the default target it then sets comes from where the client now is.
    fn apply_shell_navigation_command(
        &mut self,
        client_id: ClientId,
        command: &EndpointCommand,
    ) -> bool {
        match command {
            EndpointCommand::WorkspaceFocus(target) => {
                let Some(workspace_index) = self.app.parse_workspace_id(&target.workspace_id)
                else {
                    return false;
                };
                let Some(workspace_id) = self
                    .app
                    .state
                    .workspaces
                    .get(workspace_index)
                    .map(|workspace| workspace.id.clone())
                else {
                    return false;
                };
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                let Some(location) = client.shell_state_mut().location.as_mut() else {
                    return false;
                };
                location.focus_workspace(workspace_id);
                true
            }
            EndpointCommand::TabFocus(target) => target
                .tab_id
                .parse()
                .is_ok_and(|tab_id| self.focus_shell_client_on_tab(client_id, &tab_id)),
            EndpointCommand::PaneFocus(target) => self
                .app
                .parse_pane_id(&target.pane_id)
                .and_then(|(workspace_index, pane_id)| {
                    let tab_index = self.app.state.workspaces[workspace_index]
                        .find_tab_index_for_pane(pane_id)?;
                    self.app.public_tab_id(workspace_index, tab_index)
                })
                .is_some_and(|tab_id| self.focus_shell_client_on_tab(client_id, &tab_id)),
            _ => false,
        }
    }

    /// The app half of a client-shell command: refuses it once the server is
    /// stopping, drains pending internal events, dispatches the command and
    /// makes sure a shell client still has a workspace to show.
    fn run_endpoint_command_in_app(
        &mut self,
        command: EndpointCommand,
    ) -> (bool, Result<EndpointReply, ApiError>) {
        if self.lifecycle.stop_requested(self.app.state.should_quit) {
            self.initiate_shutdown();
        }
        if self.lifecycle.phase() == ShutdownPhase::Stopping {
            let error = self.lifecycle.shutdown_error().map_or_else(
                || ApiError::new(ApiErrorCode::ServerUnavailable, "server is shutting down"),
                ApiError::from_body,
            );
            return (false, Err(error));
        }
        self.immediate_pty_sources_dirty = true;

        let mut changed = self.drain_all_internal_events_with_forwarding();

        // Command handlers read each tab's recorded layout area for
        // directional focus, resize steps and spawn sizes; the geometry paths
        // keep it current, so there is nothing to project first.
        let outcome = self.app.handle_endpoint_command_with_render(command);
        changed |= outcome.render != RenderDemand::None;

        if self.clients.latest_shell_client().is_some() {
            changed |= self.app.ensure_default_workspace();
        }

        (changed, outcome.result)
    }
}
