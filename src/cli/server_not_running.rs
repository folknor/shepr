use std::path::Path;

use shepr_api::schema::{ErrorBody, ErrorResponse};

/// Builds the friendly `server_not_running` ErrorResponse shown when no
/// server is listening on the resolved API socket.
pub(super) fn response(
    request_id: &str,
    socket_path: &Path,
    paths: &shepr_config::AppPaths,
) -> ErrorResponse {
    // The local API client's socket is `paths.server_address().api_socket()`,
    // so this command names the server that was not found, including the
    // current profile's executable and any socket override.
    let attach_command = super::target::attach_command(paths);
    let message = shepr_api::guidance::operator_guidance(
        shepr_api::guidance::OperatorGuidance::ServerNotRunning {
            socket_path,
            attach_command: &attach_command,
        },
    );
    ErrorResponse {
        id: request_id.to_string(),
        error: ErrorBody::new(&shepr_api::error::ApiErrorCode::ServerNotRunning, message),
    }
}

pub(super) fn cli_error(response: ErrorResponse) -> super::CliError {
    super::CliError::Response(response)
}

#[cfg(test)]
pub(super) fn was_reported(error: &super::CliError) -> bool {
    reported_response(error).is_some()
}

#[cfg(test)]
pub(super) fn reported_response(error: &super::CliError) -> Option<&ErrorResponse> {
    match error {
        super::CliError::Response(response)
            if response.error.code == shepr_api::error::ApiErrorCode::ServerNotRunning.as_str() =>
        {
            Some(response)
        }
        _ => None,
    }
}
