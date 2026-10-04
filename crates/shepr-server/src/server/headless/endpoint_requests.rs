use super::*;
use crate::app::EndpointContext;
use crate::server::ClientId;
use shepr_protocol::command::{
    EndpointAppCommand, EndpointCommand, EndpointError, EndpointLoopCommand, EndpointReply,
};

impl HeadlessServer {
    /// Runs one endpoint command from a client shell. Each answer enters that
    /// client's ordered outbox; ready replies leave after any render the
    /// command needs (`release_endpoint_replies`).
    pub(super) fn handle_client_shell_endpoint_request(
        &mut self,
        client_id: ClientId,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        command: EndpointCommand,
    ) {
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        let surface_active = client.shell_state().is_surface_active();
        if boot_id != self.client_shell_boot_id {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                EndpointError::StaleBoot,
            );
            self.queue_endpoint_reply(client_id, &message);
            return;
        }
        let command = match command.into_app_command() {
            Ok(command) => command,
            Err(EndpointLoopCommand::ClientShellSurfaceSet(params)) => {
                let Some(surface_interest::SurfaceActivation {
                    changed,
                    projection_revision,
                }) = self.set_client_shell_surface_active(client_id, params.active)
                else {
                    return;
                };
                self.queue_endpoint_reply(
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
                if changed && params.active {
                    self.mark_view_changed();
                }
                return;
            }
        };
        if !surface_active {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                EndpointError::SurfaceInactive,
            );
            self.queue_endpoint_reply(client_id, &message);
            return;
        }

        self.promote_client_to_foreground(client_id);
        let result = self.handle_client_shell_app_command(client_id, command);
        self.queue_endpoint_reply(
            client_id,
            &crate::server::client_commands::response_message(boot_id, request_id, result),
        );
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
    /// is its own. Shared changes advance the view epoch; navigation is
    /// derived from each client's location generation. Returns the command's answer.
    fn handle_client_shell_app_command(
        &mut self,
        client_id: ClientId,
        command: EndpointAppCommand,
    ) -> Result<EndpointReply, EndpointError> {
        let traits = command.traits();
        let gesture_step = matches!(
            command,
            EndpointAppCommand::LayoutSetSplitRatio(_) | EndpointAppCommand::PaneResize(_)
        );

        let mut changed = self.drain_all_internal_events_with_forwarding();
        changed |= self.sync_pending_terminal_titles();

        // Command handlers read each workspace's recorded geometry for
        // directional focus, resize steps and spawn sizes; the geometry paths
        // and workspace creation keep it current, so there is nothing to
        // project first. The requester's own geometry only sizes a workspace
        // with none recorded.
        let ctx = EndpointContext {
            requester_geometry: self.client_geometry(client_id),
        };
        let outcome = self
            .app
            .handle_endpoint_app_command_with_render(command, &ctx);
        match outcome.invalidation {
            crate::app::Invalidation::None => {}
            crate::app::Invalidation::Shared => changed = true,
            crate::app::Invalidation::PaneViewers(pane) => self.invalidate_pane_viewers(pane),
        }
        let mut immediate_sources_changed = outcome.effects.changes_immediate_pty_sources();

        let mut navigated = false;
        if let Some(workspace_id) = &outcome.navigate {
            navigated = self.navigate_shell_client(client_id, workspace_id);
        }
        // A command can empty the session (the last pane closing), and a
        // session some client looks at is never left without a workspace.
        // Both move what some client views, so both change the sources.
        let created = self.create_automatic_workspace(Some(client_id));
        self.reconcile_client_shell_locations();
        changed |= created;
        immediate_sources_changed |= created;
        if immediate_sources_changed {
            self.immediate_pty_sources_dirty = true;
        }
        changed |= self.claim_client_geometry(
            client_id,
            super::client_views::GeometryClaimReason::Command {
                claims: traits.claims_shell_geometry,
                topology: traits.changes_topology,
                navigated,
                gesture_step,
            },
        );
        self.sync_pane_focus();
        if changed {
            self.mark_view_changed();
        }
        outcome.result
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
        self.queue_endpoint_reply(
            client_id,
            &crate::server::client_commands::error_message(
                boot_id,
                request_id,
                EndpointError::ShuttingDown,
            ),
        );
        self.release_endpoint_replies(ReleaseMode::Shutdown);
        if let Some(client) = self.clients.get(&client_id) {
            self.shutdown_flushes.push(client.outbox.flush_barrier());
        }
    }

    /// Test adapter for the wire command: runs an app command through
    /// `handle_client_shell_app_command` and refuses a loop-owned one.
    #[cfg(test)]
    pub(super) fn handle_client_shell_command(
        &mut self,
        client_id: ClientId,
        command: EndpointCommand,
    ) -> Result<EndpointReply, EndpointError> {
        match command.into_app_command() {
            Ok(command) => self.handle_client_shell_app_command(client_id, command),
            Err(command) => Err(EndpointError::Internal(format!(
                "{} is handled by the server loop",
                command.name()
            ))),
        }
    }
}
