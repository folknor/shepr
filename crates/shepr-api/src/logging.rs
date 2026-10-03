/// Request params are deliberately not logged, here or in the other request
/// events: request params can carry user content. Keep them out when adding
/// fields.
pub(crate) fn api_request_started(
    request_id: &str,
    method_name: &str,
    mutates_ui: bool,
    routine: bool,
) {
    let event = "api.request.start";
    let subsystem = "api";
    let outcome = "started";
    let message = "api request received";
    if mutates_ui && !routine {
        tracing::info!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method_name,
            changes_ui = mutates_ui,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method_name,
            changes_ui = mutates_ui,
            "{message}"
        );
    }
}

pub(crate) fn api_request_completed(
    request_id: &str,
    method_name: &str,
    mutates_ui: bool,
    routine: bool,
    outcome: &'static str,
) {
    let event = "api.request.complete";
    let subsystem = "api";
    let message = "api request completed";
    if outcome != "ok" || (mutates_ui && !routine) {
        tracing::info!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method_name,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method_name,
            "{message}"
        );
    }
}

/// A client that disconnected before its response was written never reaches
/// this: the server write paths treat
/// `shepr_platform::ipc::StreamFailure::PeerGone` as a finished request.
/// What remains is a real delivery failure, so it is logged as an error.
pub(crate) fn api_request_failed(request_id: &str, method_name: &str, err: &str) {
    tracing::error!(
        event = "api.request.fail",
        subsystem = "api",
        outcome = "error",
        request_id,
        method = method_name,
        error = err,
        "api request failed"
    );
}
