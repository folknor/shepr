use super::*;

#[tokio::test]
async fn client_shell_attach_seeds_workspace() {
    let mut server = test_headless_server();
    server.app.test_state_mut().test_set_workspaces(Vec::new());
    server.app.test_state_mut().seed_bookmark_index(None);
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: ClientId::test_new(6),
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 23),
                shepr_core::geometry::HostCell::Unknown
            ),
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );

    assert_eq!(server.app.state().workspaces().len(), 1);
    // The connecting client is the automatic creation's trigger: the workspace
    // is sized for it and it controls it, and the client views it. No
    // navigation effect ran, so the session's bookmark is untouched.
    let created = server.app.state().ws(0).id();
    let client_id = ClientId::test_new(6);
    assert_eq!(
        server
            .app
            .state()
            .ws(0)
            .spawn_geometry()
            .map(|geometry| geometry.area),
        Some(Rect::new(0, 0, 80, 23))
    );
    assert_eq!(
        server.clients.geometry_controller(&created),
        Some(client_id)
    );
    assert_eq!(server.shell_target_for_client(client_id), Some(created));
    assert_eq!(server.app.state().workspaces().bookmark(), None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_snapshot_presents_unknown_agent_as_idle() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("endpoint");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    server
        .app
        .test_state_mut()
        .terminal_mut(pane_id)
        .set_detected_state(
            Some(shepr_agent::Agent::Pi),
            shepr_agent::AgentState::Unknown,
        );

    let (writer, control_rx, _render_rx) = test_client_writer();
    server.test_handle_server_event(ServerEvent::ShellConnected {
        client_id: ClientId::test_new(78),
        geometry: shepr_core::geometry::HostGeometry::new(
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_core::geometry::HostCell::Unknown,
        ),
        mouse_capture: false,
        surface_active: false,
        outbox: writer,
    });

    let snapshot = client_shell_snapshot(&control_rx);
    assert_eq!(
        snapshot.agents[0].agent_status,
        shepr_api::schema::AgentStatus::Idle
    );
    assert_eq!(
        snapshot.workspaces[0].agent_status,
        shepr_api::schema::AgentStatus::Idle
    );
    assert!(control_rx.try_recv().is_err());
}

#[tokio::test]
async fn client_shell_endpoint_request_uses_the_selected_connection() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("endpoint")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(41);
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 23),
                shepr_core::geometry::HostCell::Unknown
            ),
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let _initial_snapshot = client_shell_snapshot(&control_rx);
    let boot_id = server.client_shell_boot_id.clone();
    let workspace_id = server.app.state().ws(0).id();
    // A rename is a UI mutation, so each accepted request asks for a render.
    // The second arrives before the first was answered and simply runs
    // after it.
    let ids = [
        shepr_protocol::RequestId::allocate(),
        shepr_protocol::RequestId::allocate(),
    ];
    for (request_id, label) in ids.iter().cloned().zip(["first", "renamed"]) {
        let command = Box::new(EndpointCommand::WorkspaceRename(
            shepr_protocol::command::WorkspaceRenameParams {
                workspace_id,
                label: Some(label.into()),
            },
        ));
        assert!(
            server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
                client_id,
                boot_id: boot_id.clone(),
                request_id,
                command,
            })
        );
    }
    assert!(
        control_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "a reply waits for the render its command asked for"
    );

    // The loop's order: render, then flush the held replies.
    server.render_now();
    server.release_endpoint_replies(ReleaseMode::WithinBudget);

    let ServerMessage::EndpointSnapshot(snapshot) =
        read_server_message(control_rx.recv().expect("renamed projection"))
    else {
        panic!("the projection the commands changed goes out before their replies");
    };
    assert_eq!(snapshot.workspaces[0].label, "renamed");
    for expected in &ids {
        match read_server_message(control_rx.recv().expect("endpoint response")) {
            ServerMessage::ClientShellEndpointResponse {
                boot_id: response_boot_id,
                request_id,
                result,
            } => {
                assert_eq!(response_boot_id, boot_id);
                assert_eq!(&request_id, expected, "replies leave in command order");
                assert!(matches!(
                    result,
                    Ok(shepr_protocol::command::EndpointReply::WorkspaceInfo { .. })
                ));
            }
            other => panic!("expected client shell endpoint response, got {other:?}"),
        }
    }
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn immediate_endpoint_replies_stay_after_earlier_commands() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("ordered")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (control, _render) = connect_matching_test_shell(&mut server, 40);
    let _initial_snapshot = client_shell_snapshot(&control);
    let client_id = ClientId::test_new(40);
    let current_boot = server.client_shell_boot_id.clone();
    let workspace_id = server.app.state().ws(0).id();
    let [held, stale, deactivate, inactive, activate] =
        std::array::from_fn(|_| shepr_protocol::RequestId::allocate());

    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: current_boot.clone(),
        request_id: held.clone(),
        command: Box::new(EndpointCommand::WorkspaceRename(
            shepr_protocol::command::WorkspaceRenameParams {
                workspace_id,
                label: Some("updated".into()),
            },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: shepr_test_fixtures::fixed_boot_id(2),
        request_id: stale.clone(),
        command: Box::new(EndpointCommand::PaneClear(
            shepr_protocol::command::PaneTarget {
                pane_id: shepr_test_fixtures::id("w1:p1"),
            },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: current_boot.clone(),
        request_id: deactivate.clone(),
        command: Box::new(EndpointCommand::ClientShellSurfaceSet(
            shepr_protocol::command::ClientShellSurfaceSetParams { active: false },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: current_boot.clone(),
        request_id: inactive.clone(),
        command: Box::new(EndpointCommand::PaneClear(
            shepr_protocol::command::PaneTarget {
                pane_id: shepr_test_fixtures::id("w1:p1"),
            },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: current_boot.clone(),
        request_id: activate.clone(),
        command: Box::new(EndpointCommand::ClientShellSurfaceSet(
            shepr_protocol::command::ClientShellSurfaceSetParams { active: true },
        )),
    });

    server.render_now();
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let mut replies = Vec::new();
    while replies.len() < 5 {
        if let ServerMessage::ClientShellEndpointResponse {
            request_id, result, ..
        } = read_server_message(
            control
                .recv_timeout(Duration::from_secs(1))
                .expect("ordered endpoint replies"),
        ) {
            replies.push((request_id, result));
        }
    }
    assert_eq!(
        replies.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        [held, stale, deactivate, inactive, activate]
    );
    assert!(matches!(
        &replies[1].1,
        Err(shepr_protocol::command::EndpointError::StaleBoot)
    ));
    assert!(matches!(
        &replies[2].1,
        Ok(shepr_protocol::command::EndpointReply::ClientShellSurfaceSet { active: false, .. })
    ));
    assert!(matches!(
        &replies[3].1,
        Err(shepr_protocol::command::EndpointError::SurfaceInactive)
    ));
    assert!(matches!(
        &replies[4].1,
        Ok(shepr_protocol::command::EndpointReply::ClientShellSurfaceSet { active: true, .. })
    ));
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn endpoint_requests_dirty_immediate_sources_for_view_changes_only() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    server
        .app
        .test_state_mut()
        .test_push_workspace(shepr_mux::workspace::Workspace::test_new("second"));
    let (control, _render) = connect_matching_test_shell(&mut server, 45);
    let _initial_snapshot = client_shell_snapshot(&control);
    server.immediate_pty_sources_dirty = false;
    let boot_id = server.client_shell_boot_id.clone();
    let client_id = ClientId::test_new(45);

    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: boot_id.clone(),
        request_id: shepr_protocol::RequestId::allocate(),
        command: Box::new(EndpointCommand::PaneScroll(
            shepr_protocol::command::PaneScrollParams {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                offset_from_bottom: 0,
            },
        )),
    });
    assert!(
        !server.immediate_pty_sources_dirty,
        "pane scrolling changes its surface but not the set of immediate PTY sources"
    );

    let second_workspace = server.app.state().ws(1).id();
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id,
        request_id: shepr_protocol::RequestId::allocate(),
        command: Box::new(EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget {
                workspace_id: second_workspace,
            },
        )),
    });
    assert!(
        server.immediate_pty_sources_dirty,
        "moving a client to another workspace changes the immediate PTY sources"
    );
    server.immediate_pty_sources_dirty = false;
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: server.client_shell_boot_id.clone(),
        request_id: shepr_protocol::RequestId::allocate(),
        command: Box::new(EndpointCommand::ClientShellSurfaceSet(
            shepr_protocol::command::ClientShellSurfaceSetParams { active: false },
        )),
    });
    assert!(
        server.immediate_pty_sources_dirty,
        "removing an active surface changes which client views contribute sources"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn slow_checkout_root_worker_does_not_hold_other_clients() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("checkout")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (control_a, _render_a) = connect_matching_test_shell(&mut server, 51);
    let (control_b, _render_b) = connect_matching_test_shell(&mut server, 52);
    let _initial_a = client_shell_snapshot(&control_a);
    let _initial_b = client_shell_snapshot(&control_b);
    let client_a = ClientId::test_new(51);
    let client_b = ClientId::test_new(52);
    let boot_id = server.client_shell_boot_id.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
    server.workers.set_runner(std::sync::Arc::new(move |_| {
        started_tx.send(()).expect("checkout worker started signal");
        let released = release_rx
            .lock()
            .ok()
            .is_some_and(|receiver| receiver.recv_timeout(Duration::from_secs(3)).is_ok());
        assert!(released, "slow worker test gate released before timeout");
        Ok(Some("/checkout".into()))
    }));

    let [slow_checkout, after_slow, other_client] =
        std::array::from_fn(|_| shepr_protocol::RequestId::allocate());
    let started = std::time::Instant::now();
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id: client_a,
        boot_id: boot_id.clone(),
        request_id: slow_checkout.clone(),
        command: Box::new(EndpointCommand::WorkspaceCheckoutRoot(
            shepr_protocol::command::WorkspaceCheckoutRootParams { cwd: "/".into() },
        )),
    });
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the request handler must return while Git work is still pending"
    );
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("checkout worker should have started");

    let workspace_id = server.app.state().ws(0).id();
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id: client_a,
        boot_id: boot_id.clone(),
        request_id: after_slow.clone(),
        command: Box::new(EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget { workspace_id },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id: client_b,
        boot_id,
        request_id: other_client.clone(),
        command: Box::new(EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget { workspace_id },
        )),
    });
    server.render_now();
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let mut other_client_replied = false;
    while !other_client_replied {
        if let ServerMessage::ClientShellEndpointResponse { request_id, .. } = read_server_message(
            control_b
                .recv_timeout(Duration::from_secs(1))
                .expect("the other client's command should complete"),
        ) {
            other_client_replied = request_id == other_client;
        }
    }
    assert!(
        server.clients[&client_a]
            .outbox
            .held_reply_message(0)
            .is_none(),
        "the slow client's reserved reply should remain held"
    );
    assert!(
        server.clients[&client_a]
            .outbox
            .held_reply_message(1)
            .is_some()
    );
    release_tx
        .send(())
        .expect("checkout worker should still wait");

    let completion = tokio::time::timeout(Duration::from_secs(1), server.workers.recv())
        .await
        .expect("checkout completion should wake the loop")
        .expect("worker channel should stay open");
    assert!(!server.handle_worker_completion(completion));
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let mut client_a_replies = Vec::new();
    while client_a_replies.len() < 2 {
        if let ServerMessage::ClientShellEndpointResponse {
            request_id, result, ..
        } = read_server_message(
            control_a
                .recv_timeout(Duration::from_secs(1))
                .expect("slow client's ordered responses"),
        ) {
            client_a_replies.push((request_id, result));
        }
    }
    assert_eq!(
        client_a_replies
            .iter()
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>(),
        [slow_checkout, after_slow]
    );
    assert!(matches!(
        &client_a_replies[0].1,
        Ok(shepr_protocol::command::EndpointReply::WorkspaceCheckoutRoot {
            root: Some(root),
            ..
        }) if root.as_path() == std::path::Path::new("/checkout")
    ));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_endpoint_replies_leave_with_their_client_and_resolve_at_shutdown() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("pending")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (control_a, _render_a) = connect_matching_test_shell(&mut server, 61);
    let (control_b, _render_b) = connect_matching_test_shell(&mut server, 62);
    let _initial_a = client_shell_snapshot(&control_a);
    let _initial_b = client_shell_snapshot(&control_b);
    let client_a = ClientId::test_new(61);
    let client_b = ClientId::test_new(62);
    let boot_id = server.client_shell_boot_id.clone();
    let [gone_id, late_id, pending_id, after_id] =
        std::array::from_fn(|_| shepr_protocol::RequestId::allocate());
    let refusal = |request_id: &shepr_protocol::RequestId| {
        crate::server::client_commands::error_message(
            boot_id.clone(),
            request_id.clone(),
            shepr_protocol::command::EndpointError::ShuttingDown,
        )
    };

    // A client that leaves takes its pending slot with it, and the worker
    // result that arrives for it afterwards finds nothing to fill.
    let gone = server
        .reserve_endpoint_reply(client_a, &refusal(&gone_id))
        .expect("reserve");
    server.remove_client(client_a);
    assert!(!server.clients.contains_key(&client_a));
    server.complete_endpoint_reply(gone, &refusal(&late_id));
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );

    // At shutdown a pending slot is answered with its refusal, and a reply
    // queued behind it still leaves after it.
    server.reserve_endpoint_reply(client_b, &refusal(&pending_id));
    server.queue_endpoint_reply(
        client_b,
        &crate::server::client_commands::response_message(
            boot_id.clone(),
            after_id.clone(),
            Ok(shepr_protocol::command::EndpointReply::Done),
        ),
    );
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    assert_eq!(
        server.clients[&client_b].outbox.held_reply_count(),
        2,
        "the pending slot holds the reply behind it"
    );
    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown settles endpoint replies");
    let mut replies = Vec::new();
    while replies.len() < 2 {
        if let ServerMessage::ClientShellEndpointResponse {
            request_id, result, ..
        } = read_server_message(
            control_b
                .recv_timeout(Duration::from_secs(1))
                .expect("both replies reach the client"),
        ) {
            replies.push((request_id, result));
        }
    }
    assert_eq!(
        replies
            .iter()
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>(),
        [pending_id, after_id]
    );
    assert!(matches!(
        &replies[0].1,
        Err(shepr_protocol::command::EndpointError::ShuttingDown)
    ));
    assert!(matches!(
        read_server_message(
            control_b
                .recv_timeout(Duration::from_secs(1))
                .expect("shutdown notice follows held replies")
        ),
        ServerMessage::ServerShutdown { .. }
    ));
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn an_endpoint_error_reply_is_held_until_the_flush() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("no-render")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(42);
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 23),
                shepr_core::geometry::HostCell::Unknown
            ),
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let _initial_snapshot = client_shell_snapshot(&control_rx);

    // Focusing a pane that does not exist fails; its error is held like
    // any other reply until the loop flushes.
    let missing_pane = shepr_protocol::RequestId::allocate();
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: server.client_shell_boot_id.clone(),
        request_id: missing_pane.clone(),
        command: Box::new(EndpointCommand::PaneFocus(
            shepr_protocol::command::PaneTarget {
                pane_id: shepr_test_fixtures::id("w999:p999"),
            },
        )),
    });
    assert_eq!(server.clients[&client_id].outbox.held_reply_count(), 1);
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let ServerMessage::ClientShellEndpointResponse {
        request_id,
        result: Err(_),
        ..
    } = read_server_message(control_rx.recv().expect("error response"))
    else {
        panic!("expected an endpoint error response");
    };
    assert_eq!(request_id, missing_pane);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn an_endpoint_reply_for_a_departed_client_is_dropped() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("departed")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(43);
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 23),
                shepr_core::geometry::HostCell::Unknown
            ),
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let _initial_snapshot = client_shell_snapshot(&control_rx);
    let workspace_id = server.app.state().ws(0).id();
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: server.client_shell_boot_id.clone(),
        request_id: shepr_protocol::RequestId::allocate(),
        command: Box::new(EndpointCommand::WorkspaceRename(
            shepr_protocol::command::WorkspaceRenameParams {
                workspace_id,
                label: Some("gone".into()),
            },
        )),
    });
    assert!(server.test_handle_server_event(ServerEvent::Detached { client_id }));
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );
    assert!(!server.clients.contains_key(&client_id));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn a_completion_for_a_departed_client_is_dropped() {
    let mut server = test_headless_server();
    for id in [1, 2] {
        let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 16, 1 << 20);
        server.insert_test_client(
            id,
            ClientConnection::new(
                (80, 24),
                shepr_core::geometry::HostCell::Unknown,
                id,
                outbox,
            ),
        );
    }
    let refusal = ServerMessage::WindowTitle {
        title: Some("refusal".into()),
    };
    let departing = server
        .reserve_endpoint_reply(ClientId::test_new(1), &refusal)
        .expect("reserved for the departing client");
    let survivor = server
        .reserve_endpoint_reply(ClientId::test_new(2), &refusal)
        .expect("reserved for the survivor");
    // Sequences are per outbox, so only the client id tells the two apart.
    assert_eq!(departing.seq, survivor.seq);
    server.remove_client(ClientId::test_new(1));
    server.complete_endpoint_reply(
        departing,
        &ServerMessage::WindowTitle {
            title: Some("late".into()),
        },
    );
    assert!(!server.clients.contains_key(&ClientId::test_new(1)));
    assert_eq!(
        server.clients[&ClientId::test_new(2)]
            .outbox
            .held_reply_count(),
        1
    );
    assert_eq!(
        server.clients[&ClientId::test_new(2)]
            .outbox
            .held_reply_message(0),
        None,
        "the survivor's reply is still pending"
    );
}
