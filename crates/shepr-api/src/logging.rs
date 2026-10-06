use crate::error::ApiLogOutcome;
use crate::schema::MethodTraits;

/// Request params are deliberately not logged, here or in the other request
/// events: request params can carry user content. Keep them out when adding
/// fields.
pub(crate) fn api_request_started(request_id: &str, method: MethodTraits) {
    let message = "api request received";
    if method.mutates_ui && !method.routine {
        shepr_platform::structured_log!(
            INFO,
            event = api.request,
            outcome = "started",
            request_id,
            method = method.name,
            changes_ui = method.mutates_ui,
            "{message}"
        );
    } else {
        shepr_platform::structured_log!(
            DEBUG,
            event = api.request,
            outcome = "started",
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
    let message = "api request completed";
    let outcome_value = outcome.as_str();
    if outcome != ApiLogOutcome::Ok || (method.mutates_ui && !method.routine) {
        shepr_platform::structured_log!(
            INFO,
            event = api.request,
            outcome = outcome_value,
            request_id,
            method = method.name,
            "{message}"
        );
    } else {
        shepr_platform::structured_log!(
            DEBUG,
            event = api.request,
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
    shepr_platform::structured_log!(
        ERROR,
        event = api.request,
        outcome = "delivery_error",
        request_id,
        method = method_name,
        error = err,
        "api request failed"
    );
}
