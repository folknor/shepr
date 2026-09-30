use shepr_protocol::command::{EndpointError, EndpointReply};
use shepr_protocol::{BootId, RequestId, ServerMessage};

pub(crate) use crate::limits::{MAX_ENDPOINT_BOOT_ID_BYTES, MAX_ENDPOINT_REQUEST_ID_BYTES};

/// The one response to an endpoint command. A large result (a selection of a
/// long scrollback) crosses in as many frames as it needs; only one past
/// `MAX_MESSAGE_SIZE`, which the client would refuse, is answered with
/// `EndpointError::ResponseTooLarge`, naming its size, rather than failing to
/// send and leaving the client to wait out its command timeout.
pub(crate) fn response_message(
    boot_id: BootId,
    request_id: RequestId,
    result: Result<EndpointReply, EndpointError>,
) -> ServerMessage {
    response_within(
        boot_id,
        request_id,
        result,
        shepr_protocol::MAX_MESSAGE_SIZE,
    )
}

/// The whole response envelope is measured, not just its result, so the bound
/// is the size the client actually reads.
fn response_within(
    boot_id: BootId,
    request_id: RequestId,
    result: Result<EndpointReply, EndpointError>,
    max: usize,
) -> ServerMessage {
    let message = ServerMessage::ClientShellEndpointResponse {
        boot_id: boot_id.clone(),
        request_id: request_id.clone(),
        result,
    };
    let refusal = match shepr_protocol::codec::encoded_len(&message) {
        Ok(size) if size <= max => return message,
        Ok(size) => EndpointError::ResponseTooLarge {
            size: u64::try_from(size).unwrap_or(u64::MAX),
            limit: u64::try_from(max).unwrap_or(u64::MAX),
        },
        Err(error) => {
            tracing::warn!(%error, "an endpoint response could not be encoded");
            EndpointError::Rejected("the response could not be encoded".to_owned())
        }
    };
    ServerMessage::ClientShellEndpointResponse {
        boot_id,
        request_id,
        result: Err(refusal),
    }
}

/// A refusal the server loop gives without running the command.
pub(crate) fn error_message(
    boot_id: BootId,
    request_id: RequestId,
    error: EndpointError,
) -> ServerMessage {
    response_message(boot_id, request_id, Err(error))
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
    fn server_errors_keep_their_variant_and_message() {
        for error in [
            EndpointError::Rejected("pane w1:p7 not found".into()),
            EndpointError::ShuttingDown,
            EndpointError::StaleBoot,
            EndpointError::SurfaceInactive,
        ] {
            let message = error_message(
                shepr_test_fixtures::fixed_boot_id(1),
                "request-a".into(),
                error.clone(),
            );
            let ServerMessage::ClientShellEndpointResponse {
                result: Err(sent), ..
            } = &message
            else {
                panic!("expected an error response, got {message:?}");
            };
            assert_eq!(sent, &error);
        }
    }

    #[test]
    fn a_response_larger_than_one_frame_crosses_in_parts() {
        let text = "x".repeat(shepr_protocol::MAX_FRAME_SIZE);
        let message = response_message(
            shepr_test_fixtures::fixed_boot_id(1),
            "request-a".into(),
            Ok(EndpointReply::PaneSelection {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                text: text.clone(),
            }),
        );
        let frames = shepr_protocol::encode_message(&message).expect("the reply frames");
        assert!(frames.len() > shepr_protocol::MAX_FRAME_SIZE);
        let read: ServerMessage =
            shepr_protocol::read_message(&mut frames.as_slice()).expect("the reply reads back");
        assert!(matches!(
            read,
            ServerMessage::ClientShellEndpointResponse {
                result: Ok(EndpointReply::PaneSelection { text: read, .. }),
                ..
            } if read == text
        ));
    }

    #[test]
    fn a_response_past_the_message_limit_becomes_a_bounded_refusal() {
        let message = response_within(
            shepr_test_fixtures::fixed_boot_id(1),
            "request-a".into(),
            Ok(EndpointReply::PaneSelection {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                text: "x".repeat(4096),
            }),
            1024,
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
        let EndpointError::ResponseTooLarge { size, limit } = error else {
            panic!("expected a too-large refusal, got {error:?}");
        };
        assert_eq!(*limit, 1024);
        assert!(*size > 1024);
    }
}
