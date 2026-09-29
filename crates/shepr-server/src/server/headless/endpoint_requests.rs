use super::*;
use crate::server::ClientId;
use shepr_protocol::command::EndpointCommand;

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
                shepr_api::error::ApiErrorCode::StaleBoot,
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
                    Ok(shepr_api::schema::ResponseResult::ClientShellSurfaceSet {
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
                shepr_api::error::ApiErrorCode::EndpointBusy,
                "this endpoint is still processing another command",
            );
            self.send_to_client(client_id, &message);
            return false;
        }
        if !surface_active {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                shepr_api::error::ApiErrorCode::SurfaceInactive,
                "this method requires an active client shell surface",
            );
            self.send_to_client(client_id, &message);
            return false;
        }

        let request = shepr_api::schema::Request {
            id: format!(
                "endpoint:{}:{client_id}:{request_id}",
                self.client_shell_boot_id
            ),
            method: command.into(),
        };
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
                shepr_api::error::ApiErrorCode::ServerUnavailable,
                format!("failed to start endpoint response bridge: {err}"),
            );
            self.send_to_client(client_id, &message);
            return false;
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.shell_state_mut().endpoint_command_in_flight = true;
        }
        let foreground_changed = self.promote_client_to_foreground(client_id);
        foreground_changed
            | self.handle_client_shell_api_request(
                client_id,
                shepr_api::ApiRequestMessage {
                    request,
                    respond_to,
                },
            )
    }
}
