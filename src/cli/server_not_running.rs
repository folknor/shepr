use std::fmt;
use std::path::Path;

use crate::api::schema::{ErrorBody, ErrorResponse};

/// Marker error signalling a dead API socket. Carries the `ErrorResponse` that
/// should be printed at the edge that finally surfaces the error, so a caller
/// that drops the error (such as `agent start`'s best-effort reconcile request
/// at its deadline) prints nothing. Mirrors `ProtocolMismatchReported`, except
/// that printing is deferred to that edge.
#[derive(Debug)]
pub(super) struct ServerNotRunningReported {
    pub(super) response: ErrorResponse,
}

impl fmt::Display for ServerNotRunningReported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Delegate to the carried message so pre-existing paths that
        // stringify transport errors still show the actionable text.
        f.write_str(&self.response.error.message)
    }
}

impl std::error::Error for ServerNotRunningReported {}

/// Builds the friendly `server_not_running` ErrorResponse shown when no
/// server is listening on the resolved API socket.
pub(super) fn response(request_id: &str, socket_path: &Path) -> ErrorResponse {
    let attach_command = startup_command(socket_path);
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

fn startup_command(socket_path: &Path) -> String {
    let session_socket =
        crate::session::api_socket_path_for(crate::session::active_name().as_deref());
    if socket_path == session_socket {
        crate::session::local_attach_command()
    } else {
        // A socket override wins over an inherited SHEPR_SESSION. Keep the
        // command in the current environment so it starts the overridden
        // target instead of directing the user to an unrelated session.
        "shepr".to_string()
    }
}

/// Wraps the response in the recognizable marker WITHOUT printing. The caller
/// that ultimately surfaces the error prints the carried response (see
/// `reported_response`); recovering callers simply drop it.
pub(super) fn reported_error(response: ErrorResponse) -> std::io::Error {
    std::io::Error::other(ServerNotRunningReported { response })
}

pub(super) fn was_reported(err: &std::io::Error) -> bool {
    err.get_ref()
        .and_then(|source| source.downcast_ref::<ServerNotRunningReported>())
        .is_some()
}

/// Returns the `ErrorResponse` carried by a `server_not_running` marker, if any,
/// so the surfacing edge can print it exactly once.
pub(super) fn reported_response(err: &std::io::Error) -> Option<&ErrorResponse> {
    err.get_ref()
        .and_then(|source| source.downcast_ref::<ServerNotRunningReported>())
        .map(|reported| &reported.response)
}
