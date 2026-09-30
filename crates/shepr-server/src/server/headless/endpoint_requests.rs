use super::*;
use crate::app::EndpointContext;
use crate::server::ClientId;
use shepr_protocol::command::{EndpointCommand, EndpointError, EndpointReply};

impl HeadlessServer {
    /// Runs one endpoint command from a client shell. Refusals (an earlier
    /// boot, an inactive surface) and the surface-set acknowledgement, which
    /// carries the projection revision the client waits for, go out at once.
    /// Any other command's answer is held in the endpoint reply outbox and
    /// sent after the render its effect needs (`flush_endpoint_replies`).
    /// Commands from one client run in arrival order on this loop, so a
    /// second command sent before the first was answered simply runs after
    /// it, and the replies leave in the same order.
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
        let surface_active = client.shell_state().surface_active;
        if boot_id != self.client_shell_boot_id {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                EndpointError::StaleBoot,
            );
            self.send_to_client(client_id, &message);
            return false;
        }
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
        if !surface_active {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                EndpointError::SurfaceInactive,
            );
            self.send_to_client(client_id, &message);
            return false;
        }

        let foreground_changed = self.promote_client_to_foreground(client_id);
        let (changed, result) = self.handle_client_shell_command(client_id, command);
        self.queue_endpoint_reply(
            client_id,
            crate::server::client_commands::response_message(boot_id, request_id, result),
        );
        foreground_changed | changed
    }

    /// Runs one client-shell command for `client_id`, in this order:
    ///
    /// 1. drain the internal events queued so far;
    /// 2. resolve the command's targets, its creation source and the recorded
    ///    geometry against that drained state, and execute it (the app);
    /// 3. apply the command's navigation effect to the requesting client;
    /// 4. reconcile every client's location, and the workspace geometry;
    /// 5. settle geometry controllers on the workspace the requester views
    ///    now, never on the workspace the command acted on;
    /// 6. fill the reply's focus flags against the requester's location.
    ///
    /// Pane focus is shared per workspace; only which workspace a client views
    /// is its own. A command that failed moves nobody. Returns whether a
    /// render is needed, and the command's answer.
    pub(super) fn handle_client_shell_command(
        &mut self,
        client_id: ClientId,
        command: EndpointCommand,
    ) -> (bool, Result<EndpointReply, EndpointError>) {
        if self.lifecycle.stop_requested(self.app.state.should_quit) {
            self.initiate_shutdown();
        }
        if self.lifecycle.phase() == ShutdownPhase::Stopping {
            return (false, Err(EndpointError::ShuttingDown));
        }
        self.immediate_pty_sources_dirty = true;
        let traits = command.traits();

        let mut changed = self.drain_all_internal_events_with_forwarding();

        // Command handlers read each workspace's recorded geometry for
        // directional focus, resize steps and spawn sizes; the geometry paths
        // and workspace creation keep it current, so there is nothing to
        // project first. The requester's own geometry only sizes a workspace
        // with none recorded.
        let ctx = EndpointContext {
            requester_geometry: self.client_geometry(client_id),
        };
        let mut outcome = self.app.handle_endpoint_command_with_render(command, &ctx);
        changed |= outcome.render != RenderDemand::None;

        if let Some(workspace_id) = &outcome.navigate {
            changed |= self.navigate_shell_client(client_id, workspace_id);
        }
        // A command can empty the session (the last pane closing), and a
        // session some client looks at is never left without a workspace.
        changed |= self.create_automatic_workspace(Some(client_id));
        changed |= self.reconcile_client_shell_locations();
        if traits.claims_shell_geometry {
            changed |= if traits.changes_topology {
                self.reapply_controlled_shell_workspace_geometry(false)
            } else {
                self.claim_shell_workspace_geometry(client_id, false)
                    || self.resize_shell_workspaces_sized_for(client_id, false)
            };
        }
        if let Ok(reply) = &mut outcome.result {
            let viewed = self.shell_target_for_client(client_id);
            self.app.fill_reply_focus(reply, viewed.as_ref());
        }
        self.sync_pane_focus();
        (changed, outcome.result)
    }

    /// Answers an endpoint command that reached the loop after the stop began
    /// with the shutdown refusal, so its client is told at once rather than
    /// waiting out its command timeout. Held replies go out first, so the
    /// refusal still follows any reply an earlier command was given, and the
    /// shutdown flush waits for it like it waits for the shutdown notice.
    pub(super) fn reject_endpoint_request_for_shutdown(
        &mut self,
        client_id: ClientId,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
    ) {
        self.flush_endpoint_replies();
        let sent = self.send_to_client(
            client_id,
            &crate::server::client_commands::error_message(
                boot_id,
                request_id,
                EndpointError::ShuttingDown,
            ),
        );
        if sent && let Some(writer) = self.clients.get(&client_id).and_then(|c| c.writer.as_ref()) {
            self.shutdown_flushes.push(writer.flush());
        }
    }
}
