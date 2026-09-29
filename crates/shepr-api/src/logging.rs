/// Request params are deliberately not logged, here or in the other request
/// events: `pane.send_text` and `agent.prompt` payloads carry user content.
/// Keep them out when adding fields.
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

pub(crate) fn api_request_failed(request_id: &str, method_name: &str, err: &str) {
    tracing::error!(
        event = "api.request.fail",
        subsystem = "api",
        outcome = "error",
        request_id,
        method = method_name,
        err,
        "api request failed"
    );
}

pub(crate) fn api_wait_started(request_id: &str, pane_id: &str, timeout_ms: Option<u64>) {
    tracing::info!(
        event = "api.wait.start",
        subsystem = "api",
        outcome = "started",
        request_id,
        pane_id,
        timeout_ms,
        "api output wait started"
    );
}

pub(crate) fn api_wait_completed(request_id: &str, pane_id: &str, outcome: &'static str) {
    tracing::info!(
        event = "api.wait.complete",
        subsystem = "api",
        outcome,
        request_id,
        pane_id,
        "api output wait finished"
    );
}

pub(crate) fn api_wait_timed_out(request_id: &str, pane_id: &str) {
    tracing::warn!(
        event = "api.wait.timeout",
        subsystem = "api",
        outcome = "timeout",
        request_id,
        pane_id,
        "api output wait timed out"
    );
}
