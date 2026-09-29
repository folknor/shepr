use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::limits::{CONNECTION_POLL_INTERVAL, MAX_WAIT_TIMEOUT_MS};
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
    let deadline = match timeout_deadline(clock(), params.timeout_ms) {
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

/// The wait's deadline, or `None` for a wait without a timeout. A timeout
/// above [`MAX_WAIT_TIMEOUT_MS`] is refused before the wait subscribes.
fn timeout_deadline(
    now: std::time::Instant,
    timeout_ms: Option<u64>,
) -> Result<Option<std::time::Instant>, crate::error::ApiError> {
    let Some(ms) = timeout_ms else {
        return Ok(None);
    };
    if ms > MAX_WAIT_TIMEOUT_MS {
        return Err(crate::error::ApiError::new(
            crate::error::ApiErrorCode::InvalidRequest,
            format!("timeout_ms must be at most {MAX_WAIT_TIMEOUT_MS}, got {ms}"),
        ));
    }
    // A monotonic Instant is i64 seconds since boot on Linux; one day on top
    // of it cannot overflow, so the addition needs no checked form.
    Ok(Some(now + std::time::Duration::from_millis(ms)))
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
    use crate::error::ApiLogOutcome;
    use crate::schema::{AgentStatus, Method, PaneInfo};
    use interprocess::local_socket::traits::Listener as _;
    use shepr_test_support::ScratchDir;
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    const PANE: &str = "w1:p1";
    const TIMEOUT_MS: u64 = 1_000;

    /// A clock that answers its nth read with `base + offsets_ms[n]`, after
    /// running `on_read(n)`. Reading past the script panics, so a deadline
    /// check that never fires fails the test instead of hanging it.
    struct ScriptedClock<'a> {
        base: Instant,
        offsets_ms: &'a [u64],
        reads: Cell<usize>,
        on_read: &'a dyn Fn(usize),
    }

    impl<'a> ScriptedClock<'a> {
        fn new(offsets_ms: &'a [u64], on_read: &'a dyn Fn(usize)) -> Self {
            Self {
                // The base is arbitrary: the wait only compares readings.
                base: Instant::now(),
                offsets_ms,
                reads: Cell::new(0),
                on_read,
            }
        }

        fn now(&self) -> Instant {
            let read = self.reads.get();
            self.reads.set(read + 1);
            (self.on_read)(read);
            let Some(&offset) = self.offsets_ms.get(read) else {
                panic!("the wait read the clock past its script (read {read})");
            };
            self.base + Duration::from_millis(offset)
        }
    }

    fn working_pane() -> PaneInfo {
        PaneInfo {
            pane_id: shepr_test_fixtures::id(PANE),
            terminal_id: shepr_test_fixtures::id("term_1_1"),
            workspace_id: shepr_test_fixtures::id("w1"),
            tab_id: shepr_test_fixtures::id("w1:t1"),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            restore_error: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Working,
            tokens: std::collections::HashMap::new(),
            agent_session: None,
            scroll: None,
            revision: 0,
        }
    }

    /// An app stand-in whose pane stays `Working`, so the wait only matches
    /// an event the test pushes into the hub.
    fn working_pane_app() -> ApiRequestSender {
        let (api_tx, mut api_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::ApiRequestMessage>();
        std::thread::spawn(move || {
            while let Some(message) = api_rx.blocking_recv() {
                let response = match message.request.method {
                    Method::PaneGet(_) => Ok(ResponseResult::PaneInfo {
                        pane: working_pane(),
                    }),
                    _ => Err(crate::error::ApiError::new(
                        crate::error::ApiErrorCode::InternalError,
                        "the wait stand-in only answers pane.get",
                    )),
                };
                // A requester that stopped waiting is not the stand-in's
                // failure; the test asserts on what the wait answered.
                drop(message.respond_to.send(response));
            }
        });
        api_tx
    }

    fn idle_event() -> EventEnvelope {
        EventEnvelope {
            data: EventData::PaneAgentStatusChanged {
                pane_id: shepr_test_fixtures::id(PANE),
                workspace_id: shepr_test_fixtures::id("w1"),
                agent_status: AgentStatus::Idle,
                agent: Some("pi".into()),
                title: Some("done".into()),
                display_agent: None,
            },
        }
    }

    /// Runs one `events.wait` for `PANE` going idle within `TIMEOUT_MS`,
    /// with `clock` as the wait's clock and an open peer on the socket.
    fn wait_with_clock(
        event_hub: &EventHub,
        clock: &ScriptedClock<'_>,
    ) -> crate::error::EncodedApiResponse {
        wait_with_timeout(event_hub, clock, Some(TIMEOUT_MS))
    }

    /// [`wait_with_clock`] with the request's `timeout_ms` given explicitly.
    fn wait_with_timeout(
        event_hub: &EventHub,
        clock: &ScriptedClock<'_>,
        timeout_ms: Option<u64>,
    ) -> crate::error::EncodedApiResponse {
        let dir = ScratchDir::new("wait-clock");
        let path = dir.join("s");
        let listener = shepr_platform::ipc::bind_local_listener(&path).expect("test precondition");
        let _client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        let mut server = listener.accept().expect("test precondition");
        let api_tx = working_pane_app();
        let running = Arc::new(AtomicBool::new(true));

        wait_for_event(
            "wait".into(),
            EventsWaitParams {
                match_event: EventMatch::PaneAgentStatusChanged {
                    pane_id: PANE.into(),
                    agent_status: AgentStatus::Idle,
                },
                timeout_ms,
            },
            &mut server,
            &api_tx,
            event_hub,
            &running,
            None,
            &|| clock.now(),
        )
        .expect("the wait's socket stays healthy")
        .expect("the client stays connected, so the wait answers")
    }

    #[test]
    fn wait_times_out_exactly_when_the_clock_reaches_the_deadline() {
        let event_hub = EventHub::default();
        // Read 0 sets the deadline; each later read follows one empty poll.
        // The last reading is past the deadline, for a check that fires late.
        let offsets = [0, 0, 500, TIMEOUT_MS - 1, TIMEOUT_MS, TIMEOUT_MS + 1];
        let clock = ScriptedClock::new(&offsets, &|_| {});

        let response = wait_with_clock(&event_hub, &clock);

        assert_eq!(
            clock.reads.get(),
            5,
            "the wait must end on the read that reaches the deadline, not before or after"
        );
        assert_eq!(response.outcome, ApiLogOutcome::Timeout);
        let response: ErrorResponse =
            serde_json::from_str(&response.body).expect("an error response");
        assert_eq!(response.id, "wait");
        assert_eq!(response.error.code, "timeout");
        assert_eq!(response.error.message, "timed out waiting for event match");
    }

    #[test]
    fn an_event_arriving_just_before_the_deadline_wins() {
        let event_hub = EventHub::default();
        let offsets = [0, 0, TIMEOUT_MS - 1, TIMEOUT_MS];
        // The event lands during the read one millisecond short of the
        // deadline; the next poll must deliver it rather than time out.
        let push_idle = |read| {
            if read == 2 {
                event_hub.push(idle_event());
            }
        };
        let clock = ScriptedClock::new(&offsets, &push_idle);

        let response = wait_with_clock(&event_hub, &clock);

        assert_eq!(clock.reads.get(), 3, "the matching poll ends the wait");
        assert_eq!(response.outcome, ApiLogOutcome::Ok);
        let response: serde_json::Value = serde_json::from_str(&response.body).expect("a response");
        assert_eq!(response["id"], "wait");
        assert_eq!(response["result"]["type"], "wait_matched");
        let data = &response["result"]["event"]["data"];
        assert_eq!(data["pane_id"], PANE);
        assert_eq!(data["agent_status"], "idle");
        assert_eq!(data["title"], "done");
    }

    #[test]
    fn a_timeout_above_the_cap_is_refused_before_the_wait_subscribes() {
        for timeout_ms in [MAX_WAIT_TIMEOUT_MS + 1, u64::MAX] {
            let event_hub = EventHub::default();
            // One read for the deadline. A wait that accepted the timeout
            // would poll and read again, panicking past the script.
            let offsets = [0];
            let clock = ScriptedClock::new(&offsets, &|_| {});

            let response = wait_with_timeout(&event_hub, &clock, Some(timeout_ms));

            assert_eq!(clock.reads.get(), 1, "timeout_ms {timeout_ms}");
            let response: ErrorResponse =
                serde_json::from_str(&response.body).expect("an error response");
            assert_eq!(response.id, "wait");
            assert_eq!(response.error.code, "invalid_request");
            assert_eq!(
                response.error.message,
                format!("timeout_ms must be at most {MAX_WAIT_TIMEOUT_MS}, got {timeout_ms}")
            );
        }
    }

    #[test]
    fn a_timeout_at_the_cap_or_absent_is_accepted() {
        let now = Instant::now();
        let at_cap = timeout_deadline(now, Some(MAX_WAIT_TIMEOUT_MS))
            .unwrap_or_else(|error| panic!("the cap itself must be accepted: {error:?}"));
        assert_eq!(
            at_cap,
            Some(now + Duration::from_millis(MAX_WAIT_TIMEOUT_MS))
        );
        assert!(
            timeout_deadline(now, Some(MAX_WAIT_TIMEOUT_MS + 1)).is_err(),
            "one past the cap must be refused"
        );
        assert_eq!(
            timeout_deadline(now, None).expect("no timeout is not an error"),
            None
        );
    }

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
                pane_id: shepr_test_fixtures::id("w1:p1"),
                workspace_id: shepr_test_fixtures::id("w1"),
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
                    pane_id: shepr_test_fixtures::id("w1:p1"),
                    workspace_id: shepr_test_fixtures::id("w1"),
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
