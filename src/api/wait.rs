use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use regex::Regex;

use crate::api::schema::{
    ErrorResponse, EventData, EventEnvelope, EventMatch, EventsWaitParams, Method, Request,
    ResponseResult, Subscription, SubscriptionEventData, SubscriptionEventEnvelope,
    SuccessResponse,
};
use crate::api::server::{
    APP_RESPONSE_TIMEOUT, CONNECTION_POLL_INTERVAL, dispatch_to_app_with_caller_timeout_result,
    dispatch_to_app_with_timeout, dispatch_to_app_with_timeout_result, error_response_json,
    should_stop_connection,
};
use crate::api::subscriptions::ActiveSubscription;
use crate::api::subscriptions::{
    match_output, output_match_read_source, subscription_events_after,
};
use crate::api::{ApiRequestSender, EventHub};
use shepr_platform::ipc::LocalStream;

const AGENT_PROMPT_EFFECT_TIMEOUT_MS: u64 = 5_000;
const AGENT_PROMPT_RESPONSE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

pub(super) fn wait_for_output(
    request_id: String,
    params: &crate::api::schema::PaneWaitForOutputParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    shepr_platform::logging::api_wait_started(&request_id, &params.pane_id, params.timeout_ms);
    let deadline = params
        .timeout_ms
        .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));

    let regex = match &params.r#match {
        crate::api::schema::OutputMatch::Regex { value } => match Regex::new(value) {
            Ok(regex) => Some(regex),
            Err(err) => {
                return Ok(Some(
                    serde_json::to_string(&ErrorResponse {
                        id: request_id,
                        error: crate::api::error::ApiError::new(
                            crate::api::error::ApiErrorCode::InvalidRegex,
                            err.to_string(),
                        )
                        .into_body(),
                    })
                    .map_err(std::io::Error::other)?,
                ));
            }
        },
        crate::api::schema::OutputMatch::Substring { .. } => None,
    };

    loop {
        if should_stop_connection(stream, running)? {
            shepr_platform::logging::api_wait_completed(
                &request_id,
                &params.pane_id,
                "client_disconnected",
            );
            return Ok(None);
        }

        let read_request = Request {
            id: format!("{request_id}:read"),
            method: Method::PaneRead(crate::api::schema::PaneReadParams {
                pane_id: params.pane_id.clone(),
                source: output_match_read_source(&params.source),
                lines: params.lines,
                // `strip_ansi: false` switches the read to the ANSI renderer.
                format: crate::api::schema::ReadFormat::Text,
                strip_ansi: params.strip_ansi,
                intent: crate::api::schema::ReadIntent::Passive,
            }),
        };
        let response =
            dispatch_to_app_with_timeout_result(read_request, api_tx, Some(APP_RESPONSE_TIMEOUT));
        let read = match response {
            Ok(ResponseResult::PaneRead { read }) => read,
            Err(error) => {
                return Ok(Some(crate::api::error::encode_result(
                    request_id,
                    Err(error),
                )));
            }
            Ok(_) => {
                return Ok(Some(crate::api::error::encode_result(
                    request_id,
                    Err(crate::api::error::ApiError::new(
                        crate::api::error::ApiErrorCode::InternalError,
                        "app returned an unexpected pane read result",
                    )),
                )));
            }
        };

        let matched_line = match_output(&read.text, &params.r#match, regex.as_ref());
        if matched_line.is_some() {
            let revision = read.revision;
            shepr_platform::logging::api_wait_completed(&request_id, &params.pane_id, "matched");
            return Ok(Some(
                serde_json::to_string(&SuccessResponse {
                    id: request_id,
                    result: ResponseResult::OutputMatched {
                        pane_id: read.pane_id.clone(),
                        revision,
                        matched_line,
                        read,
                    },
                })
                .map_err(std::io::Error::other)?,
            ));
        }

        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            shepr_platform::logging::api_wait_timed_out(&request_id, &params.pane_id);
            return Ok(Some(
                serde_json::to_string(&ErrorResponse {
                    id: request_id,
                    error: crate::api::error::ApiError::new(
                        crate::api::error::ApiErrorCode::Timeout,
                        "timed out waiting for output match",
                    )
                    .into_body(),
                })
                .map_err(std::io::Error::other)?,
            ));
        }

        std::thread::sleep(CONNECTION_POLL_INTERVAL);
    }
}

pub(super) fn wait_for_agent(
    request_id: String,
    params: crate::api::schema::AgentWaitParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    let last_event_sequence = event_hub.current_sequence();
    let initial = match agent_get(&request_id, &params.target, api_tx) {
        Ok(agent) => agent,
        Err(response) => {
            return serde_json::to_string(&response)
                .map(Some)
                .map_err(std::io::Error::other);
        }
    };
    let until = agent_wait_statuses(params.until);
    if agent_wait_matches(&initial, &until, None) {
        return agent_wait_success(request_id, initial).map(Some);
    }

    match wait_for_resolved_agent(
        request_id.clone(),
        &ResolvedAgentWait {
            target: params.target,
            until,
            timeout_ms: params.timeout_ms,
            initial,
            last_event_sequence,
            after_state_change_seq: None,
            accept_transient_status: true,
            timeout_kind: AgentWaitTimeoutKind::Status,
        },
        stream,
        api_tx,
        event_hub,
        running,
    )? {
        Some(AgentWaitOutcome::Matched(agent)) => agent_wait_success(request_id, *agent).map(Some),
        Some(AgentWaitOutcome::Response(response)) => Ok(Some(response)),
        None => Ok(None),
    }
}

pub(super) fn prompt_agent(
    request_id: String,
    mut params: crate::api::schema::AgentPromptParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    let Some(wait) = params.wait.clone() else {
        // Deliberately unbounded. The app answers a plain prompt only once the
        // PTY actor has written it, on a side thread, not on the main loop;
        // an agent that is busy and not reading stdin can legitimately hold
        // that for minutes. A timeout here would report failure for a prompt
        // that is still queued and will be typed later (nothing can cancel
        // it). Callers that need a bound pass `wait` with `timeout_ms`.
        return Ok(Some(dispatch_to_app_with_timeout(
            Request {
                id: request_id,
                method: Method::AgentPrompt(params),
            },
            api_tx,
            None,
        )));
    };

    let wait_started = std::time::Instant::now();
    let before_prompt = match agent_get_for_prompt(
        &request_id,
        &params.target,
        api_tx,
        wait.timeout_ms,
        wait_started,
    ) {
        Ok(agent) => agent,
        Err(response) => {
            return serde_json::to_string(&response)
                .map(Some)
                .map_err(std::io::Error::other);
        }
    };
    let prompt_started_working =
        before_prompt.agent_status == crate::api::schema::AgentStatus::Working;
    let target = params.target.clone();
    if let Some(prompt_wait) = params.wait.as_mut() {
        prompt_wait.submission_deadline = wait
            .timeout_ms
            .map(|timeout_ms| wait_started + std::time::Duration::from_millis(timeout_ms));
    }
    let last_event_sequence = event_hub.current_sequence();
    let prompt_request = Request {
        id: request_id.clone(),
        method: Method::AgentPrompt(params),
    };
    // The app side stops waiting for the PTY write at `submission_deadline`
    // and answers with its own timeout. This bound only catches an app that
    // never answers, so it trails the deadline by a grace period to let the
    // more specific app response win.
    let prompt_response = match remaining_timeout_ms(wait.timeout_ms, wait_started) {
        Some(timeout_ms) => dispatch_to_app_with_caller_timeout_result(
            prompt_request,
            api_tx,
            Some(std::time::Duration::from_millis(timeout_ms) + AGENT_PROMPT_RESPONSE_GRACE),
        ),
        None => dispatch_to_app_with_timeout_result(prompt_request, api_tx, None),
    };
    let Ok(prompted) = agent_from_response(&request_id, &prompt_response) else {
        return Ok(Some(crate::api::error::encode_result(
            request_id,
            prompt_response,
        )));
    };
    if !agent_wait_identity_matches(
        &prompted,
        &before_prompt.terminal_id,
        before_prompt.name.as_deref().filter(|name| *name == target),
        before_prompt.agent.as_deref(),
    ) {
        return agent_wait_not_running(request_id).map(Some);
    }

    let prompt_activity_observed = prompt_started_working
        || matches!(
            prompted.agent_status,
            crate::api::schema::AgentStatus::Working | crate::api::schema::AgentStatus::Blocked
        );
    let prompt_state_change_seq = prompted.state_change_seq;
    let until = agent_wait_statuses(wait.until);
    let mut initial = prompted;

    if !prompt_activity_observed {
        let remaining_timeout_ms = remaining_timeout_ms(wait.timeout_ms, wait_started);
        let (effect_timeout_ms, timeout_kind) = match remaining_timeout_ms {
            Some(timeout_ms) if timeout_ms <= AGENT_PROMPT_EFFECT_TIMEOUT_MS => {
                (timeout_ms, AgentWaitTimeoutKind::Status)
            }
            _ => (
                AGENT_PROMPT_EFFECT_TIMEOUT_MS,
                AgentWaitTimeoutKind::PromptStalled {
                    timeout_ms: AGENT_PROMPT_EFFECT_TIMEOUT_MS,
                },
            ),
        };
        let Some(outcome) = wait_for_resolved_agent(
            request_id.clone(),
            &ResolvedAgentWait {
                target: target.clone(),
                until: prompt_activity_statuses(),
                timeout_ms: Some(effect_timeout_ms),
                initial,
                last_event_sequence,
                after_state_change_seq: Some(prompt_state_change_seq),
                accept_transient_status: true,
                timeout_kind,
            },
            stream,
            api_tx,
            event_hub,
            running,
        )?
        else {
            return Ok(None);
        };
        initial = match outcome {
            AgentWaitOutcome::Matched(agent) => *agent,
            AgentWaitOutcome::Response(response) => return Ok(Some(response)),
        };
    }
    if agent_wait_matches(&initial, &until, None) {
        return agent_prompt_success(request_id, initial).map(Some);
    }

    let Some(outcome) = wait_for_resolved_agent(
        request_id.clone(),
        &ResolvedAgentWait {
            target,
            until,
            timeout_ms: remaining_timeout_ms(wait.timeout_ms, wait_started),
            initial,
            // Replay from before submission so terminal lifecycle events consumed by
            // the activity gate still terminate this settled-state wait.
            last_event_sequence,
            after_state_change_seq: None,
            accept_transient_status: false,
            timeout_kind: AgentWaitTimeoutKind::Status,
        },
        stream,
        api_tx,
        event_hub,
        running,
    )?
    else {
        return Ok(None);
    };
    let agent = match outcome {
        AgentWaitOutcome::Matched(agent) => *agent,
        AgentWaitOutcome::Response(response) => return Ok(Some(response)),
    };
    agent_prompt_success(request_id, agent).map(Some)
}

fn remaining_timeout_ms(total_ms: Option<u64>, started: std::time::Instant) -> Option<u64> {
    total_ms.map(|total_ms| {
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        total_ms.saturating_sub(elapsed_ms)
    })
}

fn agent_prompt_success(
    request_id: String,
    agent: crate::api::schema::AgentInfo,
) -> std::io::Result<String> {
    serde_json::to_string(&SuccessResponse {
        id: request_id,
        result: ResponseResult::AgentPrompted { agent },
    })
    .map_err(std::io::Error::other)
}

struct ResolvedAgentWait {
    target: String,
    until: Vec<crate::api::schema::AgentStatus>,
    timeout_ms: Option<u64>,
    initial: crate::api::schema::AgentInfo,
    last_event_sequence: u64,
    after_state_change_seq: Option<u64>,
    accept_transient_status: bool,
    timeout_kind: AgentWaitTimeoutKind,
}

#[derive(Clone, Copy)]
enum AgentWaitTimeoutKind {
    Status,
    PromptStalled { timeout_ms: u64 },
}

enum AgentWaitOutcome {
    Matched(Box<crate::api::schema::AgentInfo>),
    Response(String),
}

fn wait_for_resolved_agent(
    request_id: String,
    wait: &ResolvedAgentWait,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<AgentWaitOutcome>> {
    let deadline = wait
        .timeout_ms
        .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    let expected_terminal_id = wait.initial.terminal_id.clone();
    let expected_name = wait
        .initial
        .name
        .as_ref()
        .filter(|name| name.as_str() == wait.target)
        .cloned();
    let expected_agent = wait.initial.agent.clone();
    let pane_id = wait.initial.pane_id.clone();
    let mut last_event_sequence = wait.last_event_sequence;

    loop {
        if should_stop_connection(stream, running)? {
            return Ok(None);
        }

        let mut should_probe = false;
        let mut matched_event_status = None;
        // Checked: if the retained history rolled past our cursor, the pane's
        // close or exit event may be among the lost ones, and waiting on would
        // never end.
        let events = match subscription_events_after(event_hub, last_event_sequence) {
            Ok(events) => events,
            Err(error) => {
                return serde_json::to_string(&ErrorResponse {
                    id: request_id,
                    error,
                })
                .map(|response| Some(AgentWaitOutcome::Response(response)))
                .map_err(std::io::Error::other);
            }
        };
        for (sequence, event) in events {
            last_event_sequence = sequence;
            match event.data {
                EventData::PaneAgentDetected {
                    pane_id: event_pane,
                    agent,
                    released,
                    final_status,
                    ..
                } if event_pane == pane_id => {
                    if released {
                        if let Some(status) = final_status
                            .filter(|status| wait.until.contains(status))
                            .or(matched_event_status)
                        {
                            let mut matched = wait.initial.clone();
                            matched.agent_status = status;
                            return Ok(Some(AgentWaitOutcome::Matched(Box::new(matched))));
                        }
                        return agent_wait_not_running(request_id)
                            .map(AgentWaitOutcome::Response)
                            .map(Some);
                    }
                    if agent.is_some() && expected_agent.is_some() && agent != expected_agent {
                        return agent_wait_not_running(request_id)
                            .map(AgentWaitOutcome::Response)
                            .map(Some);
                    }
                    should_probe = true;
                }
                EventData::PaneAgentStatusChanged {
                    pane_id: event_pane,
                    agent_status,
                    ..
                } if event_pane == pane_id => {
                    if wait.accept_transient_status && wait.until.contains(&agent_status) {
                        matched_event_status = Some(agent_status);
                    }
                    should_probe = true;
                }
                EventData::PaneUpdated { pane } if pane.pane_id == pane_id => should_probe = true,
                EventData::PaneMoved {
                    previous_pane_id, ..
                } if previous_pane_id == pane_id => {
                    return agent_wait_not_running(request_id)
                        .map(AgentWaitOutcome::Response)
                        .map(Some);
                }
                EventData::PaneClosed {
                    pane_id: event_pane,
                    ..
                }
                | EventData::PaneExited {
                    pane_id: event_pane,
                    ..
                } if event_pane == pane_id => {
                    return agent_wait_not_running(request_id)
                        .map(AgentWaitOutcome::Response)
                        .map(Some);
                }
                _ => {}
            }
        }

        if should_probe {
            let current = match agent_get(&request_id, &wait.target, api_tx) {
                Ok(agent) => agent,
                Err(response) => {
                    return agent_wait_probe_error(response)
                        .map(AgentWaitOutcome::Response)
                        .map(Some);
                }
            };
            if !agent_wait_identity_matches(
                &current,
                &expected_terminal_id,
                expected_name.as_deref(),
                expected_agent.as_deref(),
            ) {
                return agent_wait_not_running(request_id)
                    .map(AgentWaitOutcome::Response)
                    .map(Some);
            }
            if let Some(status) = matched_event_status {
                let mut matched = current;
                matched.agent_status = status;
                return Ok(Some(AgentWaitOutcome::Matched(Box::new(matched))));
            }
            if agent_wait_matches(&current, &wait.until, wait.after_state_change_seq) {
                return Ok(Some(AgentWaitOutcome::Matched(Box::new(current))));
            }
        }

        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            let current = match agent_get(&request_id, &wait.target, api_tx) {
                Ok(agent) => agent,
                Err(response) => {
                    return agent_wait_probe_error(response)
                        .map(AgentWaitOutcome::Response)
                        .map(Some);
                }
            };
            if !agent_wait_identity_matches(
                &current,
                &expected_terminal_id,
                expected_name.as_deref(),
                expected_agent.as_deref(),
            ) {
                return agent_wait_not_running(request_id)
                    .map(AgentWaitOutcome::Response)
                    .map(Some);
            }
            if agent_wait_matches(&current, &wait.until, wait.after_state_change_seq) {
                return Ok(Some(AgentWaitOutcome::Matched(Box::new(current))));
            }
            return agent_wait_timeout(request_id, wait.timeout_kind, &current)
                .map(AgentWaitOutcome::Response)
                .map(Some);
        }
        std::thread::sleep(CONNECTION_POLL_INTERVAL);
    }
}

fn prompt_activity_statuses() -> Vec<crate::api::schema::AgentStatus> {
    vec![
        crate::api::schema::AgentStatus::Working,
        crate::api::schema::AgentStatus::Blocked,
    ]
}

fn agent_wait_statuses(
    until: Vec<crate::api::schema::AgentStatus>,
) -> Vec<crate::api::schema::AgentStatus> {
    if until.is_empty() {
        vec![
            crate::api::schema::AgentStatus::Idle,
            crate::api::schema::AgentStatus::Blocked,
        ]
    } else {
        until
    }
}

fn agent_wait_identity_matches(
    agent: &crate::api::schema::AgentInfo,
    expected_terminal_id: &str,
    expected_name: Option<&str>,
    expected_agent: Option<&str>,
) -> bool {
    agent.terminal_id == expected_terminal_id
        && expected_name.is_none_or(|name| agent.name.as_deref() == Some(name))
        && match (expected_agent, agent.agent.as_deref()) {
            (Some(expected), Some(current)) => expected == current,
            (Some(_), None) => agent.name.is_some(),
            (None, _) => true,
        }
}

fn agent_wait_matches(
    agent: &crate::api::schema::AgentInfo,
    until: &[crate::api::schema::AgentStatus],
    after_state_change_seq: Option<u64>,
) -> bool {
    until.contains(&agent.agent_status)
        && after_state_change_seq.is_none_or(|baseline| agent.state_change_seq > baseline)
}

fn agent_get(
    request_id: &str,
    target: &str,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::AgentInfo, ErrorResponse> {
    let response = dispatch_to_app_with_timeout_result(
        Request {
            id: format!("{request_id}:agent"),
            method: Method::AgentGet(crate::api::schema::AgentTarget {
                target: target.to_string(),
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    agent_from_response(request_id, &response)
}

fn agent_get_for_prompt(
    request_id: &str,
    target: &str,
    api_tx: &ApiRequestSender,
    total_timeout_ms: Option<u64>,
    started: std::time::Instant,
) -> Result<crate::api::schema::AgentInfo, ErrorResponse> {
    let request = Request {
        id: format!("{request_id}:agent"),
        method: Method::AgentGet(crate::api::schema::AgentTarget {
            target: target.to_string(),
        }),
    };
    let remaining_ms = remaining_timeout_ms(total_timeout_ms, started);
    let response = match remaining_ms {
        Some(timeout_ms)
            if timeout_ms
                <= u64::try_from(APP_RESPONSE_TIMEOUT.as_millis()).unwrap_or(u64::MAX) =>
        {
            dispatch_to_app_with_caller_timeout_result(
                request,
                api_tx,
                Some(std::time::Duration::from_millis(timeout_ms)),
            )
        }
        _ => dispatch_to_app_with_timeout_result(request, api_tx, Some(APP_RESPONSE_TIMEOUT)),
    };
    agent_from_response(request_id, &response)
}

fn agent_from_response(
    request_id: &str,
    response: &crate::api::error::ApiResult,
) -> Result<crate::api::schema::AgentInfo, ErrorResponse> {
    match response {
        Ok(ResponseResult::AgentInfo { agent }) | Ok(ResponseResult::AgentPrompted { agent }) => {
            Ok(agent.clone())
        }
        Err(error) => Err(ErrorResponse {
            id: request_id.into(),
            error: error.clone().into_body(),
        }),
        Ok(_) => Err(ErrorResponse {
            id: request_id.into(),
            error: crate::api::error::ApiError::new(
                crate::api::error::ApiErrorCode::InternalError,
                "app returned an unexpected agent result",
            )
            .into_body(),
        }),
    }
}

fn agent_wait_success(
    request_id: String,
    agent: crate::api::schema::AgentInfo,
) -> std::io::Result<String> {
    serde_json::to_string(&SuccessResponse {
        id: request_id,
        result: ResponseResult::AgentInfo { agent },
    })
    .map_err(std::io::Error::other)
}

fn agent_wait_timeout(
    request_id: String,
    kind: AgentWaitTimeoutKind,
    current: &crate::api::schema::AgentInfo,
) -> std::io::Result<String> {
    let (code, message) = match kind {
        AgentWaitTimeoutKind::Status => (
            crate::api::error::ApiErrorCode::Timeout,
            "timed out waiting for agent status".to_string(),
        ),
        AgentWaitTimeoutKind::PromptStalled { timeout_ms } => {
            let status = format!("{:?}", current.agent_status).to_ascii_lowercase();
            (
                crate::api::error::ApiErrorCode::AgentPromptStalled,
                format!(
                    "agent prompt produced no observed working or blocked state within {timeout_ms} ms; current status is {status}"
                ),
            )
        }
    };
    serde_json::to_string(&ErrorResponse {
        id: request_id,
        error: crate::api::error::ApiError::new(code, message).into_body(),
    })
    .map_err(std::io::Error::other)
}

fn agent_wait_not_running(request_id: String) -> std::io::Result<String> {
    serde_json::to_string(&ErrorResponse {
        id: request_id,
        error: crate::api::error::ApiError::new(
            crate::api::error::ApiErrorCode::AgentNotRunning,
            "agent is no longer running in the target pane",
        )
        .into_body(),
    })
    .map_err(std::io::Error::other)
}

fn agent_wait_probe_error(response: ErrorResponse) -> std::io::Result<String> {
    if crate::api::error::ApiErrorCode::from(response.error.code.as_str())
        == crate::api::error::ApiErrorCode::AgentNotFound
    {
        return agent_wait_not_running(response.id);
    }
    serde_json::to_string(&response).map_err(std::io::Error::other)
}

pub(super) fn wait_for_event(
    request_id: String,
    params: EventsWaitParams,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    let deadline = params
        .timeout_ms
        .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));

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
            return Ok(Some(
                serde_json::to_string(&response).map_err(std::io::Error::other)?,
            ));
        }
    };

    loop {
        if should_stop_connection(stream, running)? {
            return Ok(None);
        }

        // Every error is final (pane gone, history lost, app unavailable):
        // retrying would spin until the deadline, or forever without one.
        match active.poll_for_wait(api_tx, event_hub) {
            Ok(Some(event)) => return Ok(Some(wait_matched_response(&request_id, event))),
            Ok(None) => {}
            Err(error) => {
                return serde_json::to_string(&ErrorResponse {
                    id: request_id,
                    error,
                })
                .map(Some)
                .map_err(std::io::Error::other);
            }
        }

        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Ok(Some(
                serde_json::to_string(&ErrorResponse {
                    id: request_id,
                    error: crate::api::error::ApiError::new(
                        crate::api::error::ApiErrorCode::Timeout,
                        "timed out waiting for event match",
                    )
                    .into_body(),
                })
                .map_err(std::io::Error::other)?,
            ));
        }

        std::thread::sleep(CONNECTION_POLL_INTERVAL);
    }
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

fn wait_matched_response(request_id: &str, event: serde_json::Value) -> String {
    let Ok(event) = serde_json::from_value::<SubscriptionEventEnvelope>(event) else {
        return error_response_json(
            request_id,
            crate::api::error::ApiErrorCode::InternalError,
            "failed to decode matched event".into(),
        );
    };

    let SubscriptionEventData::PaneAgentStatusChanged(data) = event.data else {
        return error_response_json(
            request_id,
            crate::api::error::ApiErrorCode::UnsupportedEventWaitMatch,
            "events.wait currently supports pane agent status matches".into(),
        );
    };

    serde_json::to_string(&SuccessResponse {
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
                    state_labels: data.state_labels,
                },
            },
        },
    })
    .unwrap_or_else(|err| {
        error_response_json(
            request_id,
            crate::api::error::ApiErrorCode::InternalError,
            format!("failed to encode matched event: {err}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::ErrorBody;

    #[test]
    fn agent_wait_probe_only_translates_agent_disappearance() {
        let disappeared = agent_wait_probe_error(ErrorResponse {
            id: "wait".into(),
            error: ErrorBody {
                code: "agent_not_found".into(),
                message: "missing".into(),
            },
        })
        .expect("test precondition");
        let disappeared: ErrorResponse =
            serde_json::from_str(&disappeared).expect("test precondition");
        assert_eq!(disappeared.id, "wait");
        assert_eq!(disappeared.error.code, "agent_not_running");

        let unavailable = agent_wait_probe_error(ErrorResponse {
            id: "wait".into(),
            error: ErrorBody {
                code: "server_unavailable".into(),
                message: "timed out waiting for app response".into(),
            },
        })
        .expect("test precondition");
        let unavailable: ErrorResponse =
            serde_json::from_str(&unavailable).expect("test precondition");
        assert_eq!(unavailable.id, "wait");
        assert_eq!(unavailable.error.code, "server_unavailable");
    }

    #[test]
    fn wait_matched_response_reports_undecodable_and_unsupported_events() {
        let garbage = wait_matched_response("wait", serde_json::json!({"nope": true}));
        let garbage: ErrorResponse = serde_json::from_str(&garbage).expect("test precondition");
        assert_eq!(garbage.id, "wait");
        assert_eq!(garbage.error.code, "internal_error");

        let scroll = serde_json::to_value(SubscriptionEventEnvelope {
            event: crate::api::schema::SubscriptionEventKind::ScrollChanged,
            data: SubscriptionEventData::ScrollChanged(
                crate::api::schema::PaneScrollChangedEvent {
                    pane_id: "pane_1".into(),
                    workspace_id: "workspace_1".into(),
                    scroll: crate::api::schema::PaneScrollInfo {
                        offset_from_bottom: 1,
                        max_offset_from_bottom: 2,
                        viewport_rows: 3,
                    },
                },
            ),
        })
        .expect("test precondition");
        let unsupported = wait_matched_response("wait", scroll);
        let unsupported: ErrorResponse =
            serde_json::from_str(&unsupported).expect("test precondition");
        assert_eq!(unsupported.error.code, "unsupported_event_wait_match");

        let status = serde_json::to_value(SubscriptionEventEnvelope {
            event: crate::api::schema::SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(
                crate::api::schema::PaneAgentStatusChangedEvent {
                    pane_id: "pane_1".into(),
                    workspace_id: "workspace_1".into(),
                    agent_status: crate::api::schema::AgentStatus::Idle,
                    agent: None,
                    title: None,
                    display_agent: None,
                    state_labels: Default::default(),
                },
            ),
        })
        .expect("test precondition");
        let matched: serde_json::Value =
            serde_json::from_str(&wait_matched_response("wait", status))
                .expect("test precondition");
        assert_eq!(matched["result"]["type"], "wait_matched");
    }
}
