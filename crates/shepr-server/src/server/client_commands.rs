use crate::server::ClientId;
use std::io;
use std::sync::mpsc;

use tokio::sync::mpsc as tokio_mpsc;

use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};
use shepr_protocol::command::{EndpointError, EndpointReply};
use shepr_protocol::{BootId, RequestId, ServerMessage};

use super::client_transport::ServerEvent;

pub(crate) use crate::limits::{MAX_ENDPOINT_BOOT_ID_BYTES, MAX_ENDPOINT_REQUEST_ID_BYTES};

/// The one response to an endpoint command. The response crosses in a single
/// frame, so a result too large for one (a selection of a huge scrollback) is
/// answered with `endpoint_response_too_large`, naming its size, rather than
/// failing to send and leaving the client to wait out its command timeout.
pub(crate) fn response_message(
    boot_id: BootId,
    request_id: RequestId,
    result: ApiResult,
) -> ServerMessage {
    let message = ServerMessage::ClientShellEndpointResponse {
        boot_id: boot_id.clone(),
        request_id: request_id.clone(),
        result: result.map(EndpointReply::from).map_err(EndpointError::from),
    };
    let size = match shepr_protocol::codec::encoded_len(&message) {
        Ok(size) if shepr_protocol::frame_payload_fits(size) => return message,
        Ok(size) => format!("{size} bytes"),
        Err(error) => format!("unencodable: {error}"),
    };
    ServerMessage::ClientShellEndpointResponse {
        boot_id,
        request_id,
        result: Err(EndpointError::from(ApiError::new(
            ApiErrorCode::EndpointResponseTooLarge,
            format!(
                "the response is too large to send ({size}; the limit is {} bytes)",
                shepr_protocol::MAX_FRAME_SIZE
            ),
        ))),
    }
}

pub(crate) fn error_message(
    boot_id: BootId,
    request_id: RequestId,
    code: ApiErrorCode,
    message: impl Into<String>,
) -> ServerMessage {
    response_message(boot_id, request_id, Err(ApiError::new(code, message)))
}

/// Waits on its own thread for the API's answer to one endpoint command and
/// hands it to the server loop, which sends it if the command is still the
/// client's in-flight one.
pub(crate) fn spawn_response_waiter(
    client_id: ClientId,
    boot_id: BootId,
    request_id: RequestId,
    response_rx: mpsc::Receiver<ApiResult>,
    server_event_tx: tokio_mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("shepr-client-endpoint-response".into())
        .spawn(move || {
            let result = response_rx.recv().unwrap_or_else(|_| {
                Err(ApiError::new(
                    ApiErrorCode::ServerUnavailable,
                    "endpoint command ended without a response",
                ))
            });
            // A failed send means the server loop is gone, and the client with it.
            server_event_tx
                .blocking_send(ServerEvent::ClientShellEndpointResponseReady {
                    client_id,
                    boot_id,
                    request_id,
                    result: Box::new(result),
                })
                .ok();
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_response_uses_the_client_request_id() {
        let message = response_message(
            shepr_test_fixtures::fixed_boot_id(1),
            "client-shell:1".into(),
            Ok(shepr_api::schema::ResponseResult::Ok {}),
        );
        assert!(matches!(
            message,
            ServerMessage::ClientShellEndpointResponse {
                request_id,
                result: Ok(EndpointReply::Done),
                ..
            } if request_id == "client-shell:1"
        ));
    }

    #[test]
    fn a_response_too_large_for_one_frame_becomes_an_error() {
        let message = response_message(
            shepr_test_fixtures::fixed_boot_id(1),
            "request-a".into(),
            Ok(shepr_api::schema::ResponseResult::PaneSelection {
                pane_id: "w1:p1".into(),
                text: "x".repeat(shepr_protocol::MAX_FRAME_SIZE),
            }),
        );
        let ServerMessage::ClientShellEndpointResponse {
            request_id,
            result: Err(error),
            ..
        } = &message
        else {
            panic!("expected an error response, got {message:?}");
        };
        assert_eq!(request_id, "request-a");
        assert_eq!(error.code, "endpoint_response_too_large");
        assert!(shepr_protocol::encode_frame(&message).is_ok());
    }

    #[test]
    fn the_waiter_forwards_the_api_result_once() {
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
        response_tx
            .send(Err(ApiError::new(ApiErrorCode::InternalError, "boom")))
            .expect("test precondition");

        let ServerEvent::ClientShellEndpointResponseReady {
            client_id,
            boot_id,
            request_id,
            result,
        } = event_rx.blocking_recv().expect("response event")
        else {
            panic!("expected an endpoint response event");
        };
        assert_eq!(client_id, 7);
        assert_eq!(boot_id, shepr_test_fixtures::fixed_boot_id(1));
        assert_eq!(request_id, "request-a");
        assert!(matches!(*result, Err(error) if error.code == ApiErrorCode::InternalError));
        assert!(event_rx.blocking_recv().is_none());
    }
}
