use shepr_protocol::command::{EndpointError, EndpointReply};
use shepr_protocol::{BootId, RequestId, ServerMessage};

use crate::limits::MAX_ENDPOINT_RESPONSE_ENCODED_BYTES;

/// The one response to an endpoint command. A large result (a selection of a
/// long scrollback) crosses in as many frames as it needs, up to the control
/// queue's byte cap including frame prefixes. A larger result becomes
/// a typed size-limit error instead of closing the client connection
/// and leaving it to wait out its command timeout. Held replies wait for
/// earlier control traffic to drain before entering the bounded control lane.
pub(crate) fn response_message(
    boot_id: BootId,
    request_id: RequestId,
    result: Result<EndpointReply, EndpointError>,
) -> ServerMessage {
    response_within(
        boot_id,
        request_id,
        result,
        MAX_ENDPOINT_RESPONSE_ENCODED_BYTES,
    )
}

/// Measures the whole response envelope, not just its result. The public
/// bound leaves room for the frame prefixes added when the client reads it.
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
        Ok(size) => EndpointError::LimitExceeded(shepr_protocol::LimitExceeded::new(
            shepr_protocol::Limit::new(shepr_protocol::LimitKind::EndpointResponseBytes, max),
            size,
        )),
        Err(error) => {
            shepr_platform::structured_log!(WARN, event = endpoint.response_encode, outcome = "error", %error, "an endpoint response could not be encoded");
            EndpointError::Internal("the response could not be encoded".to_owned())
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
        let id = RequestId::allocate();
        let message = response_message(
            shepr_test_fixtures::fixed_boot_id(1),
            id.clone(),
            Ok(EndpointReply::Done),
        );
        assert!(matches!(
            message,
            ServerMessage::ClientShellEndpointResponse {
                request_id,
                result: Ok(EndpointReply::Done),
                ..
            } if request_id == id
        ));
    }

    #[test]
    fn server_errors_keep_their_variant_and_message() {
        for error in [
            EndpointError::PaneGone(shepr_test_fixtures::id("w1:p7")),
            EndpointError::InvalidArgument("not a directory".into()),
            EndpointError::ShuttingDown,
            EndpointError::StaleBoot,
            EndpointError::SurfaceInactive,
        ] {
            let message = error_message(
                shepr_test_fixtures::fixed_boot_id(1),
                RequestId::allocate(),
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
            RequestId::allocate(),
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
        let id = RequestId::allocate();
        let message = response_within(
            shepr_test_fixtures::fixed_boot_id(1),
            id.clone(),
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
        assert_eq!(request_id, &id);
        let EndpointError::LimitExceeded(error) = error else {
            panic!("expected a too-large refusal, got {error:?}");
        };
        assert_eq!(error.limit.max(), 1024);
        assert!(error.actual > 1024);
    }

    #[test]
    fn a_response_past_the_control_queue_limit_becomes_a_bounded_refusal() {
        let message = response_message(
            shepr_test_fixtures::fixed_boot_id(1),
            RequestId::allocate(),
            Ok(EndpointReply::PaneSelection {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                text: "x".repeat(MAX_ENDPOINT_RESPONSE_ENCODED_BYTES + 1),
            }),
        );
        let ServerMessage::ClientShellEndpointResponse {
            result: Err(EndpointError::LimitExceeded(error)),
            ..
        } = &message
        else {
            panic!("expected an error response, got {message:?}");
        };
        assert_eq!(error.limit.max(), MAX_ENDPOINT_RESPONSE_ENCODED_BYTES);
        assert!(error.actual > MAX_ENDPOINT_RESPONSE_ENCODED_BYTES);
    }
}
