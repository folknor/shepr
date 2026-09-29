use crate::server::ClientId;
use std::io;
use std::sync::mpsc;

use tokio::sync::mpsc as tokio_mpsc;

use shepr_api::schema::Method;
use shepr_protocol::MAX_ENDPOINT_RESPONSE_CHUNK_BYTES;

use super::client_transport::ServerEvent;

pub(crate) use crate::limits::{
    MAX_ENDPOINT_BOOT_ID_BYTES, MAX_ENDPOINT_COMMAND_BYTES, MAX_ENDPOINT_REQUEST_ID_BYTES,
};

pub(crate) fn supports_client_shell_method(method: &Method) -> bool {
    method.traits().client_shell
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
                // As in the chunk loop below: a failed send means the server
                // loop is gone, and the client with it.
                server_event_tx
                    .blocking_send(ServerEvent::ClientShellEndpointResponseChunkReady {
                        client_id,
                        boot_id,
                        request_id,
                        final_chunk: true,
                        data: Vec::new(),
                    })
                    .ok();
                return;
            }
            let chunk_count = response.len().div_ceil(MAX_ENDPOINT_RESPONSE_CHUNK_BYTES);
            for (index, chunk) in response
                .chunks(MAX_ENDPOINT_RESPONSE_CHUNK_BYTES)
                .enumerate()
            {
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
            shepr_api::schema::ServerStopParams::default(),
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
            shepr_test_fixtures::fixed_boot_id(1),
            "request-a".into(),
            response_rx,
            event_tx,
        )
        .expect("test precondition");
        let response = "x".repeat(MAX_ENDPOINT_RESPONSE_CHUNK_BYTES + 17);
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
            assert_eq!(boot_id, shepr_test_fixtures::fixed_boot_id(1));
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
