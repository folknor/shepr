use std::path::Path;

use shepr_api::schema::{ErrorBody, ErrorResponse};

/// Builds the friendly `server_not_running` ErrorResponse shown when no
/// server is listening on the resolved API socket.
pub(super) fn response(
    request_id: &str,
    socket_path: &Path,
    paths: &shepr_config::AppPaths,
) -> ErrorResponse {
    // The local API client's socket is `paths.server_address().api_socket()`
    // (a `--machine` failure is answered by `target::remote_error` before
    // this), so the address's own attach command names the server that was
    // not found, whatever session or socket override selected it.
    let attach_command = paths.server_address().attach_command(paths.session_id());
    ErrorResponse {
        id: request_id.to_string(),
        error: ErrorBody {
            code: "server_not_running".into(),
            message: format!(
                "no shepr server is running at {}; run `{attach_command}` to start or attach it",
                socket_path.display()
            ),
        },
    }
}

pub(super) fn cli_error(response: ErrorResponse) -> super::CliError {
    super::CliError::Response(response)
}

#[cfg(test)]
pub(super) fn was_reported(error: &super::CliError) -> bool {
    matches!(error, super::CliError::Response(response) if response.error.code == "server_not_running")
}

#[cfg(test)]
pub(super) fn reported_response(error: &super::CliError) -> Option<&ErrorResponse> {
    match error {
        super::CliError::Response(response) if response.error.code == "server_not_running" => {
            Some(response)
        }
        _ => None,
    }
}
