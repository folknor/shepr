use shepr_api::schema::{ErrorBody, ErrorResponse};

/// The error reported for a server whose protocol is another build's, as
/// classified by `shepr_protocol::Compatibility`.
pub(super) fn mismatch_response(
    request_id: &str,
    server_protocol: u32,
    restart_guidance: &str,
) -> ErrorResponse {
    let client_protocol = shepr_protocol::PROTOCOL_VERSION;
    // Protocol versions are folded from a source fingerprint (see `build.rs`),
    // so comparing them says nothing about which build is newer. Report a
    // different build and give the same restart guidance either way: the
    // server that is running is the one that has to go.
    let message = format!(
        "this shepr client (protocol {client_protocol}, {}) is a different build from the running server (protocol {server_protocol}); restart the Shepr server with this build before using this command. {restart_guidance}",
        shepr_protocol::build_version()
    );

    ErrorResponse {
        id: request_id.to_string(),
        error: ErrorBody {
            code: "protocol_mismatch".into(),
            message,
        },
    }
}

pub(super) fn cli_error(response: ErrorResponse) -> super::CliError {
    super::CliError::Response(response)
}

#[cfg(test)]
pub(super) fn was_reported(error: &super::CliError) -> bool {
    matches!(error, super::CliError::Response(response) if response.error.code == "protocol_mismatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatch_error_preserves_request_id_and_guidance() {
        let response = mismatch_response(
            "cli:agent:wait",
            shepr_protocol::PROTOCOL_VERSION - 1,
            "Run the session stop command, then restart.",
        );

        assert_eq!(response.id, "cli:agent:wait");
        assert_eq!(response.error.code, "protocol_mismatch");
        assert!(
            response
                .error
                .message
                .contains(&format!("protocol {}", shepr_protocol::PROTOCOL_VERSION))
        );
        assert!(response.error.message.contains(&format!(
            "protocol {}",
            shepr_protocol::PROTOCOL_VERSION - 1
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
            shepr_protocol::PROTOCOL_VERSION - 1,
            shepr_protocol::PROTOCOL_VERSION + 1,
        ] {
            let message = mismatch_response("req", server_protocol, "restart guidance")
                .error
                .message;
            assert!(message.contains("different build"), "{message}");
            assert!(message.contains("restart guidance"), "{message}");
            assert!(!message.contains("newer"), "{message}");
            assert!(!message.contains("older"), "{message}");
        }
    }

    #[test]
    fn cli_error_is_recognizable_without_string_matching() {
        assert!(was_reported(&cli_error(mismatch_response(
            "req", 0, "restart"
        ))));
        assert!(!was_reported(&super::super::CliError::Io(
            std::io::Error::other("unrelated")
        )));
    }
}
