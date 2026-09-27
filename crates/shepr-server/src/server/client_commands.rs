use crate::server::ClientId;
use std::io;
use std::sync::mpsc;

use tokio::sync::mpsc as tokio_mpsc;

use shepr_api::schema::Method;

use super::client_transport::ServerEvent;

pub(crate) const MAX_ENDPOINT_COMMAND_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_ENDPOINT_BOOT_ID_BYTES: usize = 128;
pub(crate) const MAX_ENDPOINT_REQUEST_ID_BYTES: usize = 128;
// The wire field cap; a larger chunk would fail to encode.
const ENDPOINT_RESPONSE_CHUNK_BYTES: usize = shepr_protocol::MAX_ENDPOINT_RESPONSE_CHUNK_BYTES;

const CLIENT_SHELL_METHODS: &[&str] = &[
    "client_shell.surface.set",
    "layout.set_split_ratio",
    "pane.clear",
    "pane.close",
    "pane.copy_motion",
    "pane.copy_search",
    "pane.focus",
    "pane.focus_direction",
    "pane.input.set",
    "pane.rename",
    "pane.resize",
    "pane.scroll",
    "pane.selection.read",
    "pane.split",
    "pane.swap",
    "pane.zoom",
    "tab.close",
    "tab.create",
    "tab.focus",
    "tab.move",
    "tab.rename",
    "workspace.close",
    "workspace.create",
    "workspace.focus",
    "workspace.move",
    "workspace.move_block",
    "workspace.rename",
];

pub(crate) fn supports_client_shell_method_name(method: &str) -> bool {
    CLIENT_SHELL_METHODS.contains(&method)
}

pub(crate) fn supports_client_shell_method(method: &Method) -> bool {
    supports_client_shell_method_name(method.traits().name)
}

pub(crate) fn error_response(
    id: &str,
    code: impl Into<shepr_api::error::ApiErrorCode>,
    message: impl Into<String>,
) -> String {
    shepr_api::error::encode_result(
        id.to_owned(),
        Err(shepr_api::error::ApiError::new(code.into(), message)),
    )
}

pub(crate) fn success_message_with_result(
    boot_id: shepr_protocol::BootId,
    request_id: shepr_protocol::RequestId,
    result: shepr_api::schema::ResponseResult,
) -> shepr_protocol::ServerMessage {
    let success = shepr_api::schema::SuccessResponse {
        id: request_id.to_string(),
        result,
    };
    let response = shepr_api::serialize_response_or_error(&request_id, &success);
    shepr_protocol::ServerMessage::ClientShellEndpointResponseChunk {
        boot_id,
        request_id,
        final_chunk: true,
        data: response.into_bytes(),
    }
}

pub(crate) fn error_message(
    boot_id: shepr_protocol::BootId,
    request_id: shepr_protocol::RequestId,
    code: impl Into<shepr_api::error::ApiErrorCode>,
    message: impl Into<String>,
) -> shepr_protocol::ServerMessage {
    let response = error_response(&request_id, code, message);
    shepr_protocol::ServerMessage::ClientShellEndpointResponseChunk {
        boot_id,
        request_id,
        final_chunk: true,
        data: response.into_bytes(),
    }
}

pub(crate) fn spawn_response_waiter(
    client_id: ClientId,
    boot_id: shepr_protocol::BootId,
    request_id: shepr_protocol::RequestId,
    response_rx: mpsc::Receiver<shepr_api::error::ApiResult>,
    server_event_tx: tokio_mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("shepr-client-endpoint-response".into())
        .spawn(move || {
            let response = response_rx.recv().unwrap_or_else(|_| {
                Err(shepr_api::error::ApiError::new(
                    shepr_api::error::ApiErrorCode::ServerUnavailable,
                    "endpoint command ended without a response",
                ))
            });
            let response =
                shepr_api::error::encode_result(request_id.to_string(), response).into_bytes();
            if response.is_empty() {
                let _ = server_event_tx.blocking_send(
                    ServerEvent::ClientShellEndpointResponseChunkReady {
                        client_id,
                        boot_id,
                        request_id,
                        final_chunk: true,
                        data: Vec::new(),
                    },
                );
                return;
            }
            let chunk_count = response.len().div_ceil(ENDPOINT_RESPONSE_CHUNK_BYTES);
            for (index, chunk) in response.chunks(ENDPOINT_RESPONSE_CHUNK_BYTES).enumerate() {
                if server_event_tx
                    .blocking_send(ServerEvent::ClientShellEndpointResponseChunkReady {
                        client_id,
                        boot_id: boot_id.clone(),
                        request_id: request_id.clone(),
                        final_chunk: index + 1 == chunk_count,
                        data: chunk.to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_client_shell_methods_are_sorted_and_unique() {
        assert!(
            CLIENT_SHELL_METHODS
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
    }

    #[test]
    fn advertised_client_shell_methods_all_exist() {
        // Every advertised name must be a real `Method`; a stripped feature
        // must not leave names behind that the lane would claim to support.
        for name in CLIENT_SHELL_METHODS {
            // Params may be rejected (they are empty here); only the method
            // tag itself has to be known.
            let method = serde_json::json!({ "method": name, "params": {} });
            if let Err(error) = serde_json::from_value::<Method>(method) {
                assert!(
                    !error.to_string().contains("unknown variant"),
                    "{name} is advertised but is not an API method: {error}"
                );
            }
        }
    }

    #[test]
    fn client_shell_lane_excludes_api_front_door_and_lifecycle_methods() {
        assert!(supports_client_shell_method(
            &Method::ClientShellSurfaceSet(shepr_api::schema::ClientShellSurfaceSetParams {
                active: false,
            })
        ));
        assert!(supports_client_shell_method(&Method::PaneClear(
            shepr_api::schema::PaneTarget {
                pane_id: "pane-1".into(),
            },
        )));
        assert!(!supports_client_shell_method(&Method::Ping(
            shepr_api::schema::PingParams::default(),
        )));
        assert!(!supports_client_shell_method(&Method::ServerStop(
            shepr_api::schema::EmptyParams::default(),
        )));
    }

    #[test]
    fn endpoint_response_uses_the_client_request_id() {
        let correlated = shepr_api::error::encode_result(
            "client-shell:1".into(),
            Ok(shepr_api::schema::ResponseResult::Ok {}),
        );
        let decoded: serde_json::Value = serde_json::from_str(&correlated).expect("response json");

        assert_eq!(decoded["id"], "client-shell:1");
    }

    #[test]
    fn endpoint_responses_are_chunked_without_truncation() {
        let (response_tx, response_rx) = mpsc::channel();
        let (event_tx, mut event_rx) = tokio_mpsc::channel(8);
        spawn_response_waiter(
            ClientId::test_new(7),
            "boot-a".into(),
            "request-a".into(),
            response_rx,
            event_tx,
        )
        .expect("test precondition");
        let response = "x".repeat(ENDPOINT_RESPONSE_CHUNK_BYTES + 17);
        response_tx
            .send(Err(shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::InternalError,
                response.clone(),
            )))
            .expect("test precondition");

        let mut received = Vec::new();
        loop {
            let ServerEvent::ClientShellEndpointResponseChunkReady {
                client_id,
                boot_id,
                request_id,
                final_chunk,
                data,
            } = event_rx.blocking_recv().expect("response chunk")
            else {
                panic!("expected response chunk");
            };
            assert_eq!(client_id, 7);
            assert_eq!(boot_id, "boot-a");
            assert_eq!(request_id, "request-a");
            received.extend(data);
            if final_chunk {
                break;
            }
        }

        let decoded: shepr_api::schema::ErrorResponse =
            serde_json::from_slice(&received).expect("response json");
        assert_eq!(decoded.error.message, response);
    }
}
