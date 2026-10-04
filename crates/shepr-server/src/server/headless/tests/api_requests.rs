use super::*;

#[tokio::test]
async fn headless_api_reads_latest_title() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("one")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let pane_id = server.app.state().ws(0).tree().root();
    server
        .app
        .test_state_mut()
        .terminal_mut(pane_id)
        .ownership_mut()
        .set_detected_agent_process_at(shepr_agent::Agent::Claude, std::time::Instant::now());
    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes(b"\x1b]0;\xe2\xa0\x8b task\x07");
    server.app.test_runtimes_mut().insert(pane_id, runtime);
    server.outputs.render().request_terminal_title(pane_id);

    let first = headless_agent_list(&mut server)
        .pop()
        .expect("test precondition");
    assert_eq!(first.terminal_title.as_deref(), Some("⠋ task"));
    assert_eq!(first.terminal_title_stripped.as_deref(), Some("task"));
    server
        .app
        .test_runtimes_mut()
        .get(&pane_id)
        .expect("test precondition")
        .test_process_pty_bytes(b"\x1b]2;\xe2\xa0\x99 task\x1b\\");
    server.outputs.render().request_terminal_title(pane_id);
    let second = headless_agent_list(&mut server)
        .pop()
        .expect("test precondition");
    assert_eq!(second.terminal_title.as_deref(), Some("⠙ task"));
    assert_eq!(second.terminal_title_stripped.as_deref(), Some("task"));
}

fn headless_agent_list(server: &mut HeadlessServer) -> Vec<crate::app::SnapshotAgent> {
    server.sync_pending_terminal_titles();
    server.app.projection_input().agents
}

#[tokio::test]
async fn a_closed_api_channel_stops_being_selected() {
    let mut server = test_headless_server();
    let (api_tx, api_request_rx) = mpsc::channel(1);
    drop(api_tx);
    server.api_request_rx = api_request_rx;

    // Wakeups left from setup are consumed first; a closed channel still
    // selected would answer every wait at once and never let one go idle.
    let mut went_idle = false;
    for _ in 0..16 {
        let wait = server.next_loop_event(None);
        match tokio::time::timeout(Duration::from_millis(50), wait).await {
            Ok(_) => {}
            Err(_) => {
                went_idle = true;
                break;
            }
        }
    }
    assert!(!server.api_request_open, "the closed channel was noticed");
    assert!(
        went_idle,
        "the loop waits instead of spinning on the closed channel"
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn server_event_drain_is_bounded_and_keeps_remaining_events_in_order() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        42,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            42,
            writer,
        ),
    );

    let event_count = crate::server::headless::SERVER_EVENT_DRAIN_LIMIT + 2;
    let (server_event_tx, server_event_rx) =
        tokio::sync::mpsc::channel(crate::server::headless::SERVER_EVENT_DRAIN_LIMIT + 2);
    server.server_event_tx = server_event_tx;
    server.server_event_rx = server_event_rx;
    for index in 0..event_count {
        server
            .server_event_tx
            .try_send(ServerEvent::PasteRejected {
                client_id: ClientId::test_new(42),
                size: index + 1,
            })
            .expect("test precondition");
    }

    assert!(!server.test_drain_server_events());
    assert_eq!(server.server_event_rx.len(), 2);
    for expected_size in 1..=crate::server::headless::SERVER_EVENT_DRAIN_LIMIT {
        let ServerMessage::ClientShellError {
            kind: shepr_protocol::NoticeKind::LimitExceeded(error),
        } = read_server_message(
            control_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("first server event batch is reported"),
        )
        else {
            panic!("expected paste rejection notice");
        };
        assert_eq!(error.actual, expected_size);
        assert_eq!(error.limit.max(), shepr_protocol::MAX_INPUT_PAYLOAD);
    }

    assert!(!server.test_drain_server_events());
    for expected_size in (crate::server::headless::SERVER_EVENT_DRAIN_LIMIT + 1)..=event_count {
        let ServerMessage::ClientShellError {
            kind: shepr_protocol::NoticeKind::LimitExceeded(error),
        } = read_server_message(
            control_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("remaining server events are reported on the next pass"),
        )
        else {
            panic!("expected paste rejection notice");
        };
        assert_eq!(error.actual, expected_size);
        assert_eq!(error.limit.max(), shepr_protocol::MAX_INPUT_PAYLOAD);
    }
    assert_eq!(server.server_event_rx.len(), 0);
    shutdown_test_runtimes(&mut server);
}

#[test]
fn headless_api_request_drains_all_pending_internal_events_before_reading_state() {
    let mut server = test_headless_server();
    for _ in 0..=crate::app::APP_EVENT_DRAIN_LIMIT {
        server
            .outputs
            .event_sender()
            .try_send(AppEvent::GitStatusRefreshed {
                outcome: shepr_git::RefreshOutcome::empty(),
            })
            .expect("test precondition");
    }

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    // An empty git refresh has no render impact, so the returned `changed` flag is
    // not asserted; this test only covers draining past the per-batch limit.
    server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::AppRequest {
            id: "headless_capture_after_events".into(),
            method: shepr_api::schema::AppMethod::DetectCapture(shepr_api::schema::PaneTarget {
                pane_id: "w9:p9".into(),
            }),
        },
        respond_to,
    });
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("test precondition");
    let response: serde_json::Value =
        serde_json::from_str(&crate::test_support::test_json(&response))
            .expect("test precondition");

    assert_eq!(response["error"]["code"], "pane_not_found");
    assert!(server.outputs.no_queued_events());
}

#[test]
fn api_request_drain_is_bounded_and_keeps_remaining_requests_in_order() {
    let mut server = test_headless_server();
    server.app.test_state_mut().test_set_workspaces(vec![
        shepr_mux::workspace::Workspace::test_new("bounded-api"),
    ]);
    let request_count = crate::server::headless::api_dispatcher::API_REQUEST_DRAIN_LIMIT + 2;
    let (api_tx, api_rx) = tokio::sync::mpsc::channel(request_count);
    server.api_request_rx = api_rx;
    let mut responses = Vec::with_capacity(request_count);
    for index in 0..request_count {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            api_tx
                .try_send(shepr_api::ApiRequestMessage {
                    request: shepr_api::schema::AppRequest {
                        id: format!("bounded-{index}"),
                        method: shepr_api::schema::AppMethod::DetectCapture(
                            shepr_api::schema::PaneTarget {
                                pane_id: format!("w999:p{index}"),
                            },
                        ),
                    },
                    respond_to,
                })
                .is_ok()
        );
        responses.push(response_rx);
    }

    server.drain_api_requests_with_shutdown_check();
    assert_eq!(
        server.api_request_rx.len(),
        request_count - crate::server::headless::api_dispatcher::API_REQUEST_DRAIN_LIMIT
    );
    for (index, response_rx) in responses
        .iter()
        .take(crate::server::headless::api_dispatcher::API_REQUEST_DRAIN_LIMIT)
        .enumerate()
    {
        let error = response_rx
            .try_recv()
            .expect("first API request batch is answered")
            .expect_err("test pane does not exist");
        assert_eq!(
            error.into_message(),
            format!("pane w999:p{index} not found")
        );
    }
    for response_rx in responses
        .iter()
        .skip(crate::server::headless::api_dispatcher::API_REQUEST_DRAIN_LIMIT)
    {
        assert!(response_rx.try_recv().is_err());
    }

    server.drain_api_requests_with_shutdown_check();
    for (index, response_rx) in responses
        .iter()
        .enumerate()
        .skip(crate::server::headless::api_dispatcher::API_REQUEST_DRAIN_LIMIT)
    {
        let error = response_rx
            .try_recv()
            .expect("remaining API request is answered on the next pass")
            .expect_err("test pane does not exist");
        assert_eq!(
            error.into_message(),
            format!("pane w999:p{index} not found")
        );
    }
    assert_eq!(server.api_request_rx.len(), 0);
    shutdown_test_runtimes(&mut server);
}
