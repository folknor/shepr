use shepr_api::error::{ApiError, ApiErrorCode};
use shepr_protocol::command::{EndpointError, EndpointReply};
use shepr_protocol::{BootId, RequestId, ServerMessage};

pub(crate) use crate::limits::{MAX_ENDPOINT_BOOT_ID_BYTES, MAX_ENDPOINT_REQUEST_ID_BYTES};

/// The wire form of a failed endpoint command: the server error's code and
/// message.
fn endpoint_error(error: ApiError) -> EndpointError {
    EndpointError {
        code: error.code.as_str().to_owned(),
        message: error.into_message(),
    }
}

/// The one response to an endpoint command. The response crosses in a single
/// frame, so a result too large for one (a selection of a huge scrollback) is
/// answered with `endpoint_response_too_large`, naming its size, rather than
/// failing to send and leaving the client to wait out its command timeout.
pub(crate) fn response_message(
    boot_id: BootId,
    request_id: RequestId,
    result: Result<EndpointReply, ApiError>,
) -> ServerMessage {
    let message = ServerMessage::ClientShellEndpointResponse {
        boot_id: boot_id.clone(),
        request_id: request_id.clone(),
        result: result.map_err(endpoint_error),
    };
    let size = match shepr_protocol::codec::encoded_len(&message) {
        Ok(size) if shepr_protocol::frame_payload_fits(size) => return message,
        Ok(size) => format!("{size} bytes"),
        Err(error) => format!("unencodable: {error}"),
    };
    ServerMessage::ClientShellEndpointResponse {
        boot_id,
        request_id,
        result: Err(endpoint_error(ApiError::new(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_response_uses_the_client_request_id() {
        let message = response_message(
            shepr_test_fixtures::fixed_boot_id(1),
            "client-shell:1".into(),
            Ok(EndpointReply::Done),
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
    fn server_errors_keep_their_wire_code_and_message() {
        let message = response_message(
            shepr_test_fixtures::fixed_boot_id(1),
            "request-a".into(),
            Err(ApiError::pane_not_found("w1:p7")),
        );
        let ServerMessage::ClientShellEndpointResponse {
            result: Err(error), ..
        } = &message
        else {
            panic!("expected an error response, got {message:?}");
        };
        assert_eq!(
            error,
            &EndpointError {
                code: "pane_not_found".into(),
                message: "pane w1:p7 not found".into(),
            }
        );
    }

    #[test]
    fn a_response_too_large_for_one_frame_becomes_an_error() {
        let message = response_message(
            shepr_test_fixtures::fixed_boot_id(1),
            "request-a".into(),
            Ok(EndpointReply::PaneSelection {
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
}
