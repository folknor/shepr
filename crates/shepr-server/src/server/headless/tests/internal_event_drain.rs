use super::*;
use shepr_agent::{Agent, AgentState};
use shepr_mux::events::AppEvent;

#[test]
fn headless_internal_event_drain_is_bounded_per_tick() {
    let mut server = test_headless_server();
    for _ in 0..=crate::app::APP_EVENT_DRAIN_LIMIT {
        server
            .app
            .event_tx
            .try_send(AppEvent::GitStatusRefreshed {
                outcome: shepr_git::RefreshOutcome::empty(),
            })
            .expect("test precondition");
    }

    assert!(!server.drain_internal_events_with_forwarding());
    assert_eq!(server.app.event_rx.len(), 1);
    assert!(!server.drain_internal_events_with_forwarding());
    assert!(server.app.event_rx.is_empty());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn unchanged_git_status_drain_clears_in_flight_without_rendering() {
    let mut server = test_headless_server();
    server.app.git_refresh.git_refresh_in_flight = true;
    server
        .app
        .event_tx
        .try_send(AppEvent::GitStatusRefreshed {
            outcome: shepr_git::RefreshOutcome::empty(),
        })
        .expect("test precondition");

    assert!(!server.drain_internal_events_with_forwarding());
    assert!(!server.app.git_refresh.git_refresh_in_flight);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn full_internal_event_queue_eventually_applies_working_to_idle_transition() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("test");
    let pane_id = workspace.root_pane();
    server.app.state.test_set_workspaces(vec![workspace]);
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));

    let terminal_id = server.app.state.workspaces[0]
        .pane_state(pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    server.app.insert_idle_test_runtime(pane_id);
    let now = server.app.clock.now;
    let working = server.app.from_pane_runtime(
        pane_id,
        AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            detection: shepr_detect::Detection::new(AgentState::Working, false),
            process_exited: false,
            observed_at: now,
        },
    );
    server.app.handle_internal_event(working);
    assert_eq!(
        server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .ownership()
            .state(),
        AgentState::Working
    );

    for _ in 0..crate::app::APP_EVENT_CHANNEL_CAPACITY {
        server
            .app
            .event_tx
            .try_send(AppEvent::GitStatusRefreshed {
                outcome: shepr_git::RefreshOutcome::empty(),
            })
            .expect("test precondition");
    }

    let tx = server.app.event_tx.clone();
    let now = server.app.clock.now;
    let send = tx.send(server.app.from_pane_runtime(
        pane_id,
        AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            detection: shepr_detect::Detection::new(AgentState::Idle, false),
            process_exited: false,
            observed_at: now,
        },
    ));
    tokio::pin!(send);

    let blocked =
        tokio::time::timeout(Duration::from_millis(20), async { (&mut send).await }).await;
    assert!(
        blocked.is_err(),
        "state change sender should wait for queue space instead of failing"
    );

    server.drain_internal_events_with_forwarding();

    tokio::time::timeout(Duration::from_millis(50), async { (&mut send).await })
        .await
        .expect("state change should enqueue once queue space is available")
        .expect("app event receiver should still be alive");

    let max_drains =
        (crate::app::APP_EVENT_CHANNEL_CAPACITY / crate::app::APP_EVENT_DRAIN_LIMIT) + 2;
    for _ in 0..max_drains {
        if server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .ownership()
            .state()
            == AgentState::Idle
        {
            break;
        }
        server.drain_internal_events_with_forwarding();
    }

    assert_eq!(
        server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .ownership()
            .state(),
        AgentState::Idle,
        "Working to Idle should still apply after temporary queue pressure"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn checkout_root_requests_are_limited_by_running_workers() {
    let mut server = test_headless_server();
    server
        .app
        .state
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new(
            "checkout-limit",
        )]);
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (control, _render) = connect_matching_test_shell(&mut server, 811);
    let _snapshot = client_shell_snapshot(&control);
    let client_id = ClientId::test_new(811);
    let boot_id = server.client_shell_boot_id.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
    server.workers.set_runner(std::sync::Arc::new(move |_| {
        started_tx.send(()).expect("checkout worker started signal");
        release_rx
            .lock()
            .expect("checkout worker gate was not poisoned")
            .recv_timeout(Duration::from_secs(5))
            .expect("checkout worker gate released before timeout");
        Ok(Some("/checkout".into()))
    }));

    for index in 0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG {
        server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request_id: format!("checkout-{index}").into(),
            command: Box::new(EndpointCommand::WorkspaceCheckoutRoot(
                shepr_protocol::command::WorkspaceCheckoutRootParams { cwd: "/".into() },
            )),
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("checkout worker should start within the limit");
    }

    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id,
        request_id: "checkout-over-limit".into(),
        command: Box::new(EndpointCommand::WorkspaceCheckoutRoot(
            shepr_protocol::command::WorkspaceCheckoutRootParams { cwd: "/".into() },
        )),
    });
    let replies = &server.clients[&client_id].outbox;
    assert_eq!(
        replies.held_reply_count(),
        crate::limits::MAX_WORKER_COMPLETION_BACKLOG + 1
    );
    assert!(matches!(
        replies.held_reply_message(replies.held_reply_count() - 1),
        Some(ServerMessage::ClientShellEndpointResponse {
            request_id,
            result: Err(shepr_protocol::command::EndpointError::Busy(message)),
            ..
        }) if request_id.as_str() == "checkout-over-limit" && message.contains("limit")
    ));

    for _ in 0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG {
        release_tx
            .send(())
            .expect("checkout workers should be waiting");
    }
    for _ in 0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG {
        let completion = tokio::time::timeout(Duration::from_secs(1), server.workers.recv())
            .await
            .expect("checkout worker should complete after release")
            .expect("worker channel should stay open");
        assert!(matches!(
            &completion,
            worker::WorkerCompletion::CheckoutRoot { .. }
        ));
        server.handle_worker_completion(completion);
    }
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    shutdown_test_runtimes(&mut server);
}

#[test]
fn checkout_root_requests_count_completions_waiting_in_the_worker_channel() {
    let mut server = test_headless_server();
    server
        .app
        .state
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new(
            "checkout-backlog",
        )]);
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (control, _render) = connect_matching_test_shell(&mut server, 812);
    let _snapshot = client_shell_snapshot(&control);
    let client_id = ClientId::test_new(812);
    let boot_id = server.client_shell_boot_id.clone();

    for index in 0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG {
        server
            .workers
            .enqueue(worker::WorkerCompletion::CheckoutRoot {
                ticket: super::super::ReplyTicket {
                    client_id,
                    seq: crate::server::outbox::ReplySeq::test_new(index as u64),
                },
                boot_id: boot_id.clone(),
                request_id: format!("queued-{index}").into(),
                home: None,
                result: Ok(None),
            });
    }

    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id,
        request_id: "checkout-queued-limit".into(),
        command: Box::new(EndpointCommand::WorkspaceCheckoutRoot(
            shepr_protocol::command::WorkspaceCheckoutRootParams { cwd: "/".into() },
        )),
    });
    let replies = &server.clients[&client_id].outbox;
    assert_eq!(replies.held_reply_count(), 1);
    assert!(matches!(
        replies.held_reply_message(0),
        Some(ServerMessage::ClientShellEndpointResponse {
            request_id,
            result: Err(shepr_protocol::command::EndpointError::Busy(message)),
            ..
        }) if request_id.as_str() == "checkout-queued-limit" && message.contains("limit")
    ));
    shutdown_test_runtimes(&mut server);
}
