use super::*;
use crate::server::ClientId;

impl HeadlessServer {
    pub(super) fn handle_client_shell_endpoint_request(
        &mut self,
        client_id: ClientId,
        boot_id: shepr_protocol::BootId,
        mut request: shepr_api::schema::Request,
    ) -> bool {
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        let shell = client.shell_state();
        let request_id: shepr_protocol::RequestId = request.id.clone().into();
        if !crate::server::client_commands::supports_client_shell_method(&request.method) {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                shepr_api::error::ApiErrorCode::UnsupportedEndpointCommand,
                "this method is not available through the client shell command lane",
            );
            self.send_to_client(client_id, &message);
            return false;
        }
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
        if let shepr_api::schema::Method::ClientShellSurfaceSet(params) = &request.method {
            let Some((changed, projection_revision)) =
                self.set_client_shell_surface_active(client_id, params.active)
            else {
                return false;
            };
            self.send_to_client(
                client_id,
                &crate::server::client_commands::success_message_with_result(
                    boot_id,
                    request_id,
                    shepr_api::schema::ResponseResult::ClientShellSurfaceSet {
                        active: params.active,
                        projection_revision,
                    },
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

        let api_request_id = format!(
            "endpoint:{}:{client_id}:{request_id}",
            self.client_shell_boot_id
        );
        request.id = api_request_id.clone();
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
