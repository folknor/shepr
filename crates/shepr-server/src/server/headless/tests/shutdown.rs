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
    // Every test app uses the production writer on a scratch data directory.
    // Wait for the warning's checkpoint before applying pane deaths.
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::HostShutdownWarning);
    tokio::time::timeout(
        Duration::from_secs(5),
        server.outputs.save_finished_signal().notified(),
    )
    .await
    .expect("checkpoint writer completed");
    server.handle_scheduled_tasks_headless(server.app.clock().now);
    let session_file = shepr_mux::persist::session_path(server.app.test_paths().data_dir());
    let checkpoint = std::fs::read(&session_file).expect("warning checkpoint");
    assert_eq!(
        server.lifecycle.phase(),
        ShutdownPhase::Frozen { generation: None }
    );
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

    assert_eq!(
        std::fs::read(&session_file).expect("frozen checkpoint"),
        checkpoint
    );

    // Cancellation reported through the flag thaws and re-saves current state.
    server.app.test_state_mut().test_clear_session_dirty();
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(false, Ordering::Release);
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Running);
    assert!(server.app.state().session_dirty());
    server
        .app
        .save_session_before_teardown_async()
        .await
        .expect("save after thaw");
    assert!(
        !session_file.try_exists().expect("stat the session file"),
        "the empty live session replaces the checkpoint"
    );
    // Not stopping: the warning alone never ends the server.
    assert!(!server.lifecycle.stop_requested());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_frozen_persisting_server_runs_the_final_save_and_writes_nothing() {
    let mut server = test_headless_server();

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
    tokio::time::timeout(
        Duration::from_secs(5),
        server.outputs.save_finished_signal().notified(),
    )
    .await
    .expect("checkpoint writer completed");
    server.handle_scheduled_tasks_headless(server.app.clock().now);
    assert_eq!(
        server.lifecycle.phase(),
        ShutdownPhase::Frozen { generation: None }
    );

    let session_file = shepr_mux::persist::session_path(server.app.test_paths().data_dir());
    let written_by_the_warning = std::fs::read(&session_file).expect("the warning's checkpoint");

    server.app.test_state_mut().mark_session_dirty();
    server
        .app
        .save_session_for_exit(None)
        .await
        .expect("frozen save is skipped");
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
    server.drain_all_internal_events_with_forwarding();
    assert!(server.outputs.no_queued_events());
    // ... but the pane stays in the layout the final save captures.
    assert!(server.app.state().pane(pane_id).is_some());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn refreshed_warning_requires_a_new_checkpoint_before_releasing_the_lock() {
    let mut server = test_headless_server();

    let mut checkpoints = server.lifecycle.test_warning_monitor();
    let finished = server.outputs.save_finished_signal();
    let old_checkpoint = server.app.test_saver().hold_test_host_checkpoint();
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::HostShutdownWarning);

    // Refresh while the first checkpoint is held. Its completion must not
    // answer the refreshed warning.
    server.lifecycle.test_refresh_warning();
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    old_checkpoint.complete(Ok(()));
    // Reaping the voided save starts the refreshed checkpoint on the real
    // persister; until that one is reaped the warning stays unanswered.
    server.app.service_session_saves(server.app.clock().now);
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::HostShutdownWarning);
    assert!(checkpoints.borrow().is_none());

    tokio::time::timeout(Duration::from_secs(5), finished.notified())
        .await
        .expect("refreshed checkpoint completed");
    server.handle_scheduled_tasks_headless(server.app.clock().now);
    checkpoints.changed().await.expect("delay lock released");
    assert!(matches!(
        server.lifecycle.phase(),
        ShutdownPhase::Frozen { .. }
    ));
    assert_eq!(
        *checkpoints.borrow(),
        server.lifecycle.frozen_warning_generation()
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn idle_host_checkpoint_freezes_without_an_unrelated_loop_event() {
    let output = shepr_test_support::command_in_scratch(
        std::env::current_exe().expect("test executable"),
        "idle-host-checkpoint",
    )
    .args([
        "--exact",
        "server::headless::tests::shutdown::idle_host_checkpoint_subprocess_entry_point",
        "--ignored",
        "--nocapture",
    ])
    .env("SHEPR_TEST_HOST_CHECKPOINT_CHILD", "1")
    .output()
    .expect("run isolated server loop");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "child ran no test: {stdout}\n{stderr}"
    );
}

#[test]
#[ignore = "re-exec entry point for idle_host_checkpoint_freezes_without_an_unrelated_loop_event"]
fn idle_host_checkpoint_subprocess_entry_point() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the marker belongs to this test's re-exec harness"
    )]
    if std::env::var_os("SHEPR_TEST_HOST_CHECKPOINT_CHILD").is_none() {
        return;
    }
    let _env = IsolatedEnv::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let mut server = test_headless_server();

            let workspace = shepr_mux::workspace::Workspace::test_new("idle-host-warning");
            let pane = workspace.tree().root();
            server
                .app
                .test_state_mut()
                .test_set_workspaces(vec![workspace]);
            server.app.test_state_mut().seed_bookmark_index(Some(0));
            server.app.insert_idle_test_runtime(pane);
            let mut checkpoints = server.lifecycle.test_warning_monitor();
            let completion = server.app.test_saver().hold_test_host_checkpoint();
            let finished = server.outputs.save_finished_signal();
            let stop = Arc::clone(server.lifecycle.stop_signal());
            let observe = async move {
                // Let run() reach its idle wait with the checkpoint unfinished.
                tokio::task::yield_now().await;
                completion.complete(Ok(()));
                finished.notify_one();
                checkpoints
                    .changed()
                    .await
                    .expect("checkpoint acknowledgement");
                stop.request();
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                let (result, ()) = tokio::join!(server.run(), observe);
                result.expect("server exited cleanly");
            })
            .await
            .expect("idle server failed to freeze after its checkpoint");
            // The acknowledgement is only sent on the transition to Frozen;
            // stopping retains that freeze, and the final save writes nothing.
            assert_eq!(server.lifecycle.phase(), ShutdownPhase::Stopping);
            assert!(!server.app.test_saver().save_in_flight());
        });
}

#[test]
fn the_run_loop_saves_mutations_before_releasing_its_socket() {
    let output = shepr_test_support::command_in_scratch(
        std::env::current_exe().expect("test executable"),
        "final-save-run-loop",
    )
    .args([
        "--exact",
        "server::headless::tests::shutdown::final_save_run_loop_subprocess_entry_point",
        "--ignored",
        "--nocapture",
    ])
    .env("SHEPR_TEST_FINAL_SAVE_CHILD", "1")
    .output()
    .expect("run isolated server loop");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "child ran no test: {stdout}\n{stderr}"
    );
}

#[test]
#[ignore = "re-exec entry point for the_run_loop_saves_mutations_before_releasing_its_socket"]
fn final_save_run_loop_subprocess_entry_point() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the marker belongs to this test's re-exec harness"
    )]
    if std::env::var_os("SHEPR_TEST_FINAL_SAVE_CHILD").is_none() {
        return;
    }
    let _env = IsolatedEnv::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let mut server = test_headless_server();
            let paths = server.app.test_paths().clone();
            let (tx, _rx) = tokio::sync::mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
            server.api_server = Some(
                shepr_api::start_server(
                    tx,
                    Arc::clone(server.lifecycle.stop_signal()),
                    &paths,
                    server.client_shell_boot_id.clone(),
                )
                .expect("real server socket"),
            );
            let socket = paths.server_address().socket().to_path_buf();
            assert!(
                socket.try_exists().expect("stat socket"),
                "the socket must start live"
            );
            let mut workspace = shepr_mux::workspace::Workspace::test_new("saved-by-run-loop");
            let retained_pane = workspace.tree().root();
            let exited_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);
            server
                .app
                .test_state_mut()
                .test_set_workspaces(vec![workspace]);
            server.app.test_state_mut().seed_bookmark_index(Some(0));
            server.app.insert_idle_test_runtime(retained_pane);
            server.app.insert_idle_test_runtime(exited_pane);
            server.app.test_state_mut().mark_session_dirty();
            let reported_cwd = ScratchDir::new("shutdown-cwd");
            let cwd_report = server.app.from_pane_runtime(
                retained_pane,
                shepr_mux::events::RuntimeEvent::TerminalCwdReported {
                    cwd: shepr_mux::UsableCwd::new(reported_cwd.to_path_buf())
                        .expect("scratch cwd is usable"),
                },
            );
            server
                .outputs
                .event_sender()
                .try_send(cwd_report)
                .expect("queue cwd report before stop");
            let pane_exit = server.app.from_pane_runtime(
                exited_pane,
                shepr_mux::events::RuntimeEvent::PaneDied {
                    ending: shepr_mux::pane::PaneEnding::new(
                        shepr_mux::pane::PaneEndReason::Exited,
                    ),
                    ended_at: std::time::Instant::now(),
                },
            );
            server
                .outputs
                .event_sender()
                .try_send(pane_exit)
                .expect("queue pane exit before stop");
            let session = shepr_mux::persist::session_path(paths.data_dir());
            assert!(
                !session.try_exists().expect("stat session"),
                "the mutation has not been autosaved"
            );
            server.initiate_shutdown();
            tokio::time::timeout(crate::test_support::SESSION_WRITE_TEST_BOUND, server.run())
                .await
                .expect("server shutdown completed")
                .expect("clean shutdown with final save");
            assert!(
                !socket.try_exists().expect("stat socket"),
                "run released the socket"
            );
            let saved = std::fs::read_to_string(session).expect("final session exists after stop");
            assert!(
                saved.contains("saved-by-run-loop"),
                "the final save contains the mutation"
            );
            assert!(server.app.state().pane(exited_pane).is_none());
            let snapshot = shepr_mux::persist::schema::parse_session_file(&saved)
                .expect("valid saved session");
            assert_eq!(snapshot.workspaces.len(), 1);
            let panes = snapshot.workspaces[0].layout.panes();
            assert_eq!(panes.len(), 1, "the queued pane exit is in the final save");
            assert_eq!(panes[0].cwd.as_path(), reported_cwd.path());
            assert!(shepr_mux::persist::DataDirLease::acquire(paths.data_dir()).is_ok());
        });
}

#[test]
fn an_exit_before_the_final_save_answers_a_waiting_stop_with_an_error() {
    let mut server = test_headless_server();
    let signal = Arc::clone(server.lifecycle.stop_signal());

    server.release_socket_after_save();

    // The exit published the result itself, so a later unfinished completion
    // finds one already there. (The result's wording and the waiting
    // request's answer are covered in shepr-api.)
    assert!(
        !signal.complete_unfinished_final_save("again"),
        "the exit left no result published"
    );
    shutdown_test_runtimes(&mut server);
}
