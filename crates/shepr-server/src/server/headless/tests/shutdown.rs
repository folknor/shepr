use super::*;

#[test]
fn server_stop_interrupts_server_event_backlog() {
    let mut server = test_headless_server();
    for client_id in 1..=64 {
        server
            .server_event_tx
            .try_send(ServerEvent::Disconnected {
                client_id: client_id.into(),
            })
            .expect("test precondition");
    }

    server.lifecycle.stop_signal().request();

    assert!(!server.test_drain_server_events());
    assert!(server.server_event_rx.try_recv().is_ok());
    shutdown_test_runtimes(&mut server);
}

fn shutdown_test_request(
    id: &str,
) -> (
    shepr_api::ApiRequestMessage,
    std::sync::mpsc::Receiver<shepr_api::error::ApiResult>,
) {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    (
        shepr_api::ApiRequestMessage {
            request: shepr_api::schema::AppRequest {
                id: id.into(),
                method: shepr_api::schema::AppMethod::DetectCapture(
                    shepr_api::schema::PaneTarget {
                        pane_id: "w1:p1".into(),
                    },
                ),
            },
            respond_to,
        },
        response_rx,
    )
}

fn assert_server_unavailable(
    response_rx: &std::sync::mpsc::Receiver<shepr_api::error::ApiResult>,
    id: &str,
) {
    let response = response_rx
        .try_recv()
        .expect("shutdown must answer the request, not drop it");
    let error = response.expect_err("shutdown must reject the request");
    assert_eq!(
        error.code,
        shepr_api::error::ApiErrorCode::ServerUnavailable,
        "{id}"
    );
}

#[tokio::test]
async fn complete_shutdown_answers_queued_requests_and_closes_the_channel() {
    let mut server = test_headless_server();
    let (api_tx, api_rx) = tokio::sync::mpsc::channel(1);
    server.api_request_rx = api_rx;

    let (queued, queued_rx) = shutdown_test_request("queued");
    assert!(api_tx.try_send(queued).is_ok());

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown completes");

    assert_server_unavailable(&queued_rx, "queued");
    // A request dispatched after cleanup fails at the sender, which the API
    // thread turns into `server_unavailable` at once.
    let (late, _late_rx) = shutdown_test_request("late");
    assert!(api_tx.try_send(late).is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn an_endpoint_request_queued_at_shutdown_is_answered() {
    let mut server = test_headless_server();
    let at_stop = shepr_protocol::RequestId::allocate();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("stopping")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (control, _render) = connect_matching_test_shell(&mut server, 44);
    server
        .server_event_tx
        .try_send(ServerEvent::ShellEndpointRequest {
            client_id: ClientId::test_new(44),
            boot_id: server.client_shell_boot_id.clone(),
            request_id: at_stop.clone(),
            command: Box::new(EndpointCommand::PaneFocus(
                shepr_protocol::command::PaneTarget {
                    pane_id: shepr_test_fixtures::id("w1:p1"),
                },
            )),
        })
        .expect("test precondition");

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown completes");

    let mut messages = Vec::new();
    loop {
        let message = read_server_message(
            control
                .recv_timeout(Duration::from_millis(500))
                .expect("the queued refusal and shutdown notice are flushed"),
        );
        let shutdown = matches!(&message, ServerMessage::ServerShutdown { .. });
        messages.push(message);
        if shutdown {
            break;
        }
    }
    let refusal_index = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::ClientShellEndpointResponse {
                    request_id,
                    result: Err(shepr_protocol::command::EndpointError::ShuttingDown),
                    ..
                } if *request_id == at_stop
            )
        })
        .expect("the queued command is answered, not left to its timeout");
    let shutdown_index = messages
        .iter()
        .position(|message| matches!(message, ServerMessage::ServerShutdown { .. }))
        .expect("shutdown notice is delivered");
    assert!(
        refusal_index < shutdown_index,
        "refusal must precede shutdown"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_queued_new_client_gets_its_endpoint_refusal_before_shutdown() {
    let mut server = test_headless_server();
    let client_id = ClientId::test_new(45);
    let new_client_command = shepr_protocol::RequestId::allocate();
    let boot_id = server.client_shell_boot_id.clone();
    let (writer, control, _render) = test_client_writer();
    assert!(
        server
            .server_event_tx
            .try_send(ServerEvent::ShellConnected {
                client_id,
                geometry: shepr_core::geometry::HostGeometry::new(
                    shepr_core::geometry::GridSize::clamped(80, 23),
                    shepr_core::geometry::HostCell::Unknown
                ),
                mouse_capture: false,
                surface_active: true,
                outbox: writer,
            })
            .is_ok()
    );
    assert!(
        server
            .server_event_tx
            .try_send(ServerEvent::ShellEndpointRequest {
                client_id,
                boot_id,
                request_id: new_client_command.clone(),
                command: Box::new(EndpointCommand::PaneFocus(
                    shepr_protocol::command::PaneTarget {
                        pane_id: shepr_test_fixtures::id("w1:p1"),
                    },
                )),
            })
            .is_ok()
    );

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown settles the queued connection and command");

    let mut messages = Vec::new();
    loop {
        let message = read_server_message(
            control
                .recv_timeout(Duration::from_millis(500))
                .expect("the queued refusal and shutdown notice are flushed"),
        );
        let shutdown = matches!(&message, ServerMessage::ServerShutdown { .. });
        messages.push(message);
        if shutdown {
            break;
        }
    }
    let refusal_index = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::ClientShellEndpointResponse {
                    request_id,
                    result: Err(shepr_protocol::command::EndpointError::ShuttingDown),
                    ..
                } if *request_id == new_client_command
            )
        })
        .expect("the queued command is refused");
    let shutdown_index = messages
        .iter()
        .position(|message| matches!(message, ServerMessage::ServerShutdown { .. }))
        .expect("the late client receives shutdown");
    assert!(
        refusal_index < shutdown_index,
        "refusal must precede shutdown"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_dequeued_new_client_waits_for_queued_commands_before_shutdown() {
    let mut server = test_headless_server();
    let client_id = ClientId::test_new(46);
    let dequeued_command = shepr_protocol::RequestId::allocate();
    let (writer, control, _render) = test_client_writer();
    server
        .shutdown_unregistered_clients
        .insert(client_id, writer);
    assert!(
        server
            .server_event_tx
            .try_send(ServerEvent::ShellEndpointRequest {
                client_id,
                boot_id: server.client_shell_boot_id.clone(),
                request_id: dequeued_command.clone(),
                command: Box::new(EndpointCommand::PaneFocus(
                    shepr_protocol::command::PaneTarget {
                        pane_id: shepr_test_fixtures::id("w1:p1"),
                    },
                )),
            })
            .is_ok()
    );

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown settles a selected connection before notifying it");

    assert!(matches!(
        read_server_message(
            control
                .recv_timeout(Duration::from_millis(500))
                .expect("the endpoint refusal is flushed first")
        ),
        ServerMessage::ClientShellEndpointResponse {
            request_id,
            result: Err(shepr_protocol::command::EndpointError::ShuttingDown),
            ..
        } if request_id == dequeued_command
    ));
    assert!(matches!(
        read_server_message(
            control
                .recv_timeout(Duration::from_millis(500))
                .expect("the shutdown notice follows the refusal")
        ),
        ServerMessage::ServerShutdown { .. }
    ));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn api_request_selected_during_shutdown_is_answered() {
    let mut server = test_headless_server();
    server.initiate_shutdown();
    let (request, response_rx) = shutdown_test_request("selected");
    server.reject_api_request_for_shutdown(&request);
    assert_server_unavailable(&response_rx, "selected");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn host_shutdown_warning_freezes_saves_before_applying_events_and_thaws_on_cancel() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("host-shutdown");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(true, Ordering::Release);
    // The test policy never saves, so the checkpoint writes nothing and the
    // real session file is untouched.
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Frozen);
    assert!(!server.app.session_persists());
    assert!(server.app.test_saver().autosave_deadline().is_none());

    // The server keeps running and applies pane deaths; only the disk is frozen.
    server.app.insert_idle_test_runtime(pane_id);
    let died = server.app.from_pane_runtime(
        pane_id,
        shepr_mux::events::RuntimeEvent::PaneDied {
            ending: shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Exited),
            ended_at: std::time::Instant::now(),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(died));
    assert!(server.app.state().pane(pane_id).is_none());
    assert!(!server.app.session_persists());

    // Cancellation reported through the flag thaws and re-saves current state.
    server.app.test_state_mut().test_clear_session_dirty();
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(false, Ordering::Release);
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Running);
    assert!(!server.app.session_persists());
    assert!(server.app.state().session_dirty());
    // Not stopping: the warning alone never ends the server.
    assert!(!server.lifecycle.stop_requested());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_frozen_persisting_server_runs_the_final_save_and_writes_nothing() {
    let mut server = test_headless_server();
    server.persist_for_test();
    let workspace = shepr_mux::workspace::Workspace::test_new("frozen-final-save");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    server.app.insert_idle_test_runtime(pane_id);
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(true, Ordering::Release);

    // The warning requests its checkpoint and waits for the writer's result
    // before it freezes.
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    for _ in 0..5000 {
        if server.app.host_shutdown_checkpoint_result_ready() {
            break;
        }
        server.app.reap_finished_session_save();
        std::thread::sleep(Duration::from_millis(1));
    }
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Frozen);
    assert!(server.app.session_persists());

    let session_file = shepr_mux::persist::session_path(server.app.test_paths().data_dir());
    let written_by_the_warning = std::fs::read(&session_file).expect("the warning's checkpoint");

    server.app.test_state_mut().mark_session_dirty();
    server.app.save_session_for_exit(None).await;
    assert_eq!(
        std::fs::read(&session_file).expect("the checkpoint still stands"),
        written_by_the_warning,
        "a frozen saver's final save writes nothing"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn signal_quit_drain_keeps_dying_panes_in_the_layout() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("signal-quit");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    // From the pane's live runtime, so admission passes it and only the
    // signal quit keeps the pane.
    server.app.insert_idle_test_runtime(pane_id);
    let died = server.app.from_pane_runtime(
        pane_id,
        shepr_mux::events::RuntimeEvent::PaneDied {
            ending: shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Exited),
            ended_at: std::time::Instant::now(),
        },
    );
    server
        .outputs
        .event_sender()
        .try_send(died)
        .expect("test precondition");
    server
        .lifecycle
        .signal_quit_request_flag()
        .set(std::time::Instant::now())
        .expect("the first signal");
    server.lifecycle.stop_signal().request();

    // The quit-path drain still consumes the queue ...
    server.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_CHANNEL_CAPACITY);
    assert!(server.outputs.no_queued_events());
    // ... but the pane stays in the layout the final save captures.
    assert!(server.app.state().pane(pane_id).is_some());
    shutdown_test_runtimes(&mut server);
}
