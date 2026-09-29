use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::limits::CONNECTION_POLL_INTERVAL;
use crate::schema::{
    ErrorResponse, EventData, EventEnvelope, EventMatch, EventsWaitParams, ResponseResult,
    Subscription, SubscriptionEventData, SubscriptionEventEnvelope, SuccessResponse,
};
use crate::server::{
    error_response_json, server_is_stopping, should_stop_connection, shutdown_wait_error,
};
use crate::subscriptions::ActiveSubscription;
use crate::{ApiRequestSender, EventHub};
use shepr_platform::ipc::LocalStream;

/// The answer for a socket-thread wait cut short by server shutdown: every
/// wait polls the stop flag, so none outlives the start of shutdown.
fn shutdown_response(request_id: String) -> crate::error::EncodedApiResponse {
    crate::error::encode_result_with_outcome(request_id, Err(shutdown_wait_error()))
}

/// Serves one `events.wait`. `clock` decides the request's own timeout; the
/// socket thread hands in the real one.
pub(super) fn wait_for_event(
    request_id: String,
    params: EventsWaitParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    server_stop: Option<&Arc<AtomicBool>>,
    clock: &dyn Fn() -> std::time::Instant,
) -> std::io::Result<Option<crate::error::EncodedApiResponse>> {
    let deadline = match checked_timeout_deadline(clock(), params.timeout_ms) {
        Ok(deadline) => deadline,
        Err(error) => {
            return Ok(Some(crate::error::encode_result_with_outcome(
                request_id,
                Err(error),
            )));
        }
    };

    let subscription = event_match_subscription(params.match_event);
    let mut active = match ActiveSubscription::new(
        subscription,
        &request_id,
        0,
        api_tx,
        event_hub,
        event_hub.current_sequence(),
    ) {
        Ok(active) => active,
        Err(response) => {
            return Ok(Some(crate::error::encode_error_response_with_outcome(
                &response,
            )));
        }
    };

    loop {
        if server_is_stopping(server_stop) {
            return Ok(Some(shutdown_response(request_id)));
        }
        if should_stop_connection(stream, running)? {
            return Ok(None);
        }

        // Every error is final (pane gone, history lost, app unavailable):
        // retrying would spin until the deadline, or forever without one.
        match active.poll_for_wait(api_tx, event_hub) {
            Ok(Some(event)) => return Ok(Some(wait_matched_response(&request_id, event))),
            Ok(None) => {}
            Err(error) => {
                let response = ErrorResponse {
                    id: request_id,
                    error,
                };
                return Ok(Some(crate::error::encode_error_response_with_outcome(
                    &response,
                )));
            }
        }

        if deadline.is_some_and(|deadline| clock() >= deadline) {
            let response = ErrorResponse {
                id: request_id,
                error: crate::error::ApiError::new(
                    crate::error::ApiErrorCode::Timeout,
                    "timed out waiting for event match",
                )
                .into_body(),
            };
            return Ok(Some(crate::error::encode_error_response_with_outcome(
                &response,
            )));
        }

        std::thread::sleep(CONNECTION_POLL_INTERVAL);
    }
}

fn checked_timeout_deadline(
    now: std::time::Instant,
    timeout_ms: Option<u64>,
) -> Result<Option<std::time::Instant>, crate::error::ApiError> {
    timeout_ms
        .map(|ms| {
            now.checked_add(std::time::Duration::from_millis(ms))
                .ok_or_else(|| {
                    crate::error::ApiError::new(
                        crate::error::ApiErrorCode::InvalidRequest,
                        "timeout_ms exceeds the supported deadline range",
                    )
                })
        })
        .transpose()
}

/// `EventMatch` only has variants this function can serve, so an unsupported
/// match is rejected when the request is parsed, not here.
fn event_match_subscription(match_event: EventMatch) -> Subscription {
    match match_event {
        EventMatch::PaneAgentStatusChanged {
            pane_id,
            agent_status,
        } => Subscription::PaneAgentStatusChanged {
            pane_id,
            agent_status: Some(agent_status),
        },
    }
}

fn wait_matched_response(
    request_id: &str,
    event: serde_json::Value,
) -> crate::error::EncodedApiResponse {
    let Ok(event) = serde_json::from_value::<SubscriptionEventEnvelope>(event) else {
        return error_response_json(
            request_id,
            crate::error::ApiErrorCode::InternalError,
            "failed to decode matched event".into(),
        );
    };

    let SubscriptionEventData::PaneAgentStatusChanged(data) = event.data else {
        return error_response_json(
            request_id,
            crate::error::ApiErrorCode::UnsupportedEventWaitMatch,
            "events.wait currently supports pane agent status matches".into(),
        );
    };

    let response = SuccessResponse {
        id: request_id.into(),
        result: ResponseResult::WaitMatched {
            event: EventEnvelope {
                data: EventData::PaneAgentStatusChanged {
                    pane_id: data.pane_id,
                    workspace_id: data.workspace_id,
                    agent_status: data.agent_status,
                    agent: data.agent,
                    title: data.title,
                    display_agent: data.display_agent,
                },
            },
        },
    };
    crate::serialize_response_or_error_with_outcome(request_id, &response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_matched_response_reports_undecodable_and_unsupported_events() {
        let garbage = wait_matched_response("wait", serde_json::json!({"nope": true}));
        let garbage: ErrorResponse =
            serde_json::from_str(&garbage.body).expect("test precondition");
        assert_eq!(garbage.id, "wait");
        assert_eq!(garbage.error.code, "internal_error");

        let scroll = serde_json::to_value(SubscriptionEventEnvelope {
            event: crate::schema::SubscriptionEventKind::ScrollChanged,
            data: SubscriptionEventData::ScrollChanged(crate::schema::PaneScrollChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                scroll: crate::schema::PaneScrollInfo {
                    offset_from_bottom: 1,
                    max_offset_from_bottom: 2,
                    viewport_rows: 3,
                },
            }),
        })
        .expect("test precondition");
        let unsupported = wait_matched_response("wait", scroll);
        let unsupported: ErrorResponse =
            serde_json::from_str(&unsupported.body).expect("test precondition");
        assert_eq!(unsupported.error.code, "unsupported_event_wait_match");

        let status = serde_json::to_value(SubscriptionEventEnvelope {
            event: crate::schema::SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(
                crate::schema::PaneAgentStatusChangedEvent {
                    pane_id: "pane_1".into(),
                    workspace_id: "workspace_1".into(),
                    agent_status: crate::schema::AgentStatus::Idle,
                    agent: None,
                    title: None,
                    display_agent: None,
                },
            ),
        })
        .expect("test precondition");
        let matched: serde_json::Value =
            serde_json::from_str(&wait_matched_response("wait", status).body)
                .expect("test precondition");
        assert_eq!(matched["result"]["type"], "wait_matched");
    }
}
