use std::fmt;

use crate::api::schema::{ErrorBody, ErrorResponse};

#[derive(Debug)]
pub(super) struct ProtocolMismatchReported;

impl fmt::Display for ProtocolMismatchReported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("protocol mismatch was already reported")
    }
}

impl std::error::Error for ProtocolMismatchReported {}

pub(super) fn mismatch_response(
    request_id: &str,
    server_protocol: u32,
    restart_guidance: &str,
) -> Option<ErrorResponse> {
    let client_protocol = crate::protocol::PROTOCOL_VERSION;
    if client_protocol == server_protocol {
        return None;
    }

    // Protocol versions are folded from a source fingerprint (see `build.rs`),
    // so comparing them says nothing about which build is newer. Report a
    // different build and give the same restart guidance either way: the
    // server that is running is the one that has to go.
    let message = format!(
        "this shepr client (protocol {client_protocol}, {}) is a different build from the running server (protocol {server_protocol}); restart the Shepr server with this build before using this command. {restart_guidance}",
        crate::build_info::version()
    );

    Some(ErrorResponse {
        id: request_id.to_string(),
        error: ErrorBody {
            code: "protocol_mismatch".into(),
            message,
        },
    })
}

pub(super) fn reported_error() -> std::io::Error {
    std::io::Error::other(ProtocolMismatchReported)
}

pub(super) fn was_reported(err: &std::io::Error) -> bool {
    err.get_ref()
        .and_then(|source| source.downcast_ref::<ProtocolMismatchReported>())
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_protocol_has_no_error() {
        assert!(mismatch_response("req", crate::protocol::PROTOCOL_VERSION, "restart").is_none());
    }

    #[test]
    fn mismatch_error_preserves_request_id_and_guidance() {
        let response = mismatch_response(
            "cli:agent:wait",
            crate::protocol::PROTOCOL_VERSION - 1,
            "Run the session stop command, then restart.",
        )
        .expect("test precondition");

        assert_eq!(response.id, "cli:agent:wait");
        assert_eq!(response.error.code, "protocol_mismatch");
        assert!(
            response
                .error
                .message
                .contains(&format!("protocol {}", crate::protocol::PROTOCOL_VERSION))
        );
        assert!(response.error.message.contains(&format!(
            "protocol {}",
            crate::protocol::PROTOCOL_VERSION - 1
        )));
        assert!(
            response
                .error
                .message
                .contains("Run the session stop command, then restart.")
        );
    }

    #[test]
    fn mismatch_wording_does_not_depend_on_which_number_is_larger() {
        // Fingerprint-derived versions have no order, so a higher and a lower
        // server version must produce the same "different build" report with
        // the restart guidance.
        for server_protocol in [
            crate::protocol::PROTOCOL_VERSION - 1,
            crate::protocol::PROTOCOL_VERSION + 1,
        ] {
            let message = mismatch_response("req", server_protocol, "restart guidance")
                .expect("test precondition")
                .error
                .message;
            assert!(message.contains("different build"), "{message}");
            assert!(message.contains("restart guidance"), "{message}");
            assert!(!message.contains("newer"), "{message}");
            assert!(!message.contains("older"), "{message}");
        }
    }

    #[test]
    fn reported_error_is_recognizable_without_string_matching() {
        assert!(was_reported(&reported_error()));
        assert!(!was_reported(&std::io::Error::other(
            "protocol mismatch was already reported"
        )));
    }
}
