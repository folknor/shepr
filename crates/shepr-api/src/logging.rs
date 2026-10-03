use crate::error::ApiLogOutcome;
use crate::schema::MethodTraits;

/// Request params are deliberately not logged, here or in the other request
/// events: request params can carry user content. Keep them out when adding
/// fields.
pub(crate) fn api_request_started(request_id: &str, method: MethodTraits) {
    let event = "api.request.start";
    let subsystem = "api";
    let outcome = "started";
    let message = "api request received";
    if method.mutates_ui && !method.routine {
        tracing::info!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method.name,
            changes_ui = method.mutates_ui,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method.name,
            changes_ui = method.mutates_ui,
            "{message}"
        );
    }
}

pub(crate) fn api_request_completed(
    request_id: &str,
    method: MethodTraits,
    outcome: ApiLogOutcome,
) {
    let event = "api.request.complete";
    let subsystem = "api";
    let message = "api request completed";
    let outcome_value = outcome.as_str();
    if outcome != ApiLogOutcome::Ok || (method.mutates_ui && !method.routine) {
        tracing::info!(
            event,
            subsystem,
            outcome = outcome_value,
            request_id,
            method = method.name,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome = outcome_value,
            request_id,
            method = method.name,
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
