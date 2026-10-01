use super::*;
use shepr_agent::detect::{Agent, AgentState};
use shepr_mux::events::AppEvent;

#[test]
fn headless_internal_event_drain_is_bounded_per_tick() {
    let mut server = test_headless_server();
    for _ in 0..=crate::app::APP_EVENT_DRAIN_LIMIT {
        server
            .app
            .event_tx
            .try_send(AppEvent::GitStatusRefreshed {
                results: Vec::new(),
                cache_updates: Vec::new(),
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
            results: Vec::new(),
            cache_updates: Vec::new(),
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
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));

    let terminal_id = server.app.state.workspaces[0]
        .pane_state(pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    let now = server.app.clock.now;
    server.app.handle_internal_event(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Pi),
        state: AgentState::Working,
        visible_blocker: false,
        process_exited: false,
        observed_at: now,
    });
    assert_eq!(
        server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .state,
        AgentState::Working
    );

    for _ in 0..crate::app::APP_EVENT_CHANNEL_CAPACITY {
        server
            .app
            .event_tx
            .try_send(AppEvent::GitStatusRefreshed {
                results: Vec::new(),
                cache_updates: Vec::new(),
            })
            .expect("test precondition");
    }

    let tx = server.app.event_tx.clone();
    let now = server.app.clock.now;
    let send = tx.send(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Pi),
        state: AgentState::Idle,
        visible_blocker: false,
        process_exited: false,
        observed_at: now,
    });
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
            .state
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
            .state,
        AgentState::Idle,
        "Working to Idle should still apply after temporary queue pressure"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn checkout_root_requests_are_limited_by_running_workers() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("checkout-limit")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (control, _render) = connect_matching_test_shell(&mut server, 811);
    let _snapshot = client_shell_snapshot(&control);
    let client_id = ClientId::test_new(811);
    let boot_id = server.client_shell_boot_id.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
    server.checkout_root_runner = std::sync::Arc::new(move |_| {
        started_tx.send(()).map_err(|error| error.to_string())?;
        release_rx
            .lock()
            .map_err(|_| "checkout worker gate was poisoned".to_owned())?
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "checkout worker gate timed out".to_owned())?;
        Ok(Some("/checkout".to_owned()))
    });

    for index in 0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG {
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
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

    server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
        client_id,
        boot_id,
        request_id: "checkout-over-limit".into(),
        command: Box::new(EndpointCommand::WorkspaceCheckoutRoot(
            shepr_protocol::command::WorkspaceCheckoutRootParams { cwd: "/".into() },
        )),
    });
    let replies = server
        .endpoint_replies
        .get(&client_id)
        .expect("checkout replies should remain ordered behind pending workers");
    assert_eq!(
        replies.len(),
        crate::limits::MAX_WORKER_COMPLETION_BACKLOG + 1
    );
    assert!(matches!(
        replies.back().and_then(|reply| reply.message.as_ref()),
        Some(ServerMessage::ClientShellEndpointResponse {
            request_id,
            result: Err(shepr_protocol::command::EndpointError::Rejected(message)),
            ..
        }) if request_id.as_str() == "checkout-over-limit" && message.contains("limit")
    ));

    for _ in 0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG {
        release_tx
            .send(())
            .expect("checkout workers should be waiting");
    }
    for _ in 0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG {
        let completion = tokio::time::timeout(Duration::from_secs(1), server.worker_rx.recv())
            .await
            .expect("checkout worker should complete after release")
            .expect("worker channel should stay open");
        assert!(matches!(
            &completion,
            worker::WorkerCompletion::CheckoutRoot { .. }
        ));
        let now = server.app.clock.now;
        server.handle_worker_completion(completion, now);
    }
    server.flush_endpoint_replies();
    shutdown_test_runtimes(&mut server);
}

#[test]
fn checkout_root_requests_count_completions_waiting_in_the_worker_channel() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new(
        "checkout-backlog",
    )];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (control, _render) = connect_matching_test_shell(&mut server, 812);
    let _snapshot = client_shell_snapshot(&control);
    let client_id = ClientId::test_new(812);
    let boot_id = server.client_shell_boot_id.clone();

    for index in 0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG {
        server
            .worker_tx
            .send(worker::WorkerCompletion::CheckoutRoot {
                ticket: super::super::EndpointReplyTicket(index as u64),
                boot_id: boot_id.clone(),
                request_id: format!("queued-{index}").into(),
                home: None,
                result: Ok(None),
            })
            .expect("worker completion channel should be open");
    }

    server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
        client_id,
        boot_id,
        request_id: "checkout-queued-limit".into(),
        command: Box::new(EndpointCommand::WorkspaceCheckoutRoot(
            shepr_protocol::command::WorkspaceCheckoutRootParams { cwd: "/".into() },
        )),
    });
    let replies = server
        .endpoint_replies
        .get(&client_id)
        .expect("the limited request should receive a reply");
    assert_eq!(replies.len(), 1);
    assert!(matches!(
        replies.front().and_then(|reply| reply.message.as_ref()),
        Some(ServerMessage::ClientShellEndpointResponse {
            request_id,
            result: Err(shepr_protocol::command::EndpointError::Rejected(message)),
            ..
        }) if request_id.as_str() == "checkout-queued-limit" && message.contains("limit")
    ));
    shutdown_test_runtimes(&mut server);
}
