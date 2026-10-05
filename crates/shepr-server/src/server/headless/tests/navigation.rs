use super::*;

#[tokio::test]
async fn a_client_command_neither_drags_the_bookmark_nor_moves_the_clients_location() {
    use shepr_protocol::command::{PaneSelectionReadParams, PaneTextPoint};

    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("bookmark-focus-cache");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("bookmark-focus-second");
    let second_pane = second.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let first_workspace_id = server.app.state().ws(0).id();
    let second_workspace_id = server.app.state().ws(1).id();
    let first_pane_id = server
        .app
        .state()
        .pane(first_pane)
        .expect("pane exists")
        .public_id();

    let (control, _render) = connect_matching_test_shell(&mut server, 70);
    let initial = client_shell_snapshot(&control);
    assert_eq!(
        initial.focused_workspace_id.as_ref(),
        Some(&first_workspace_id)
    );

    // The bookmark moving on (another client's navigation, say) leaves this
    // connection's location behind: it is the client's own.
    server.app.test_state_mut().seed_bookmark_index(Some(1));
    server.app.test_state_mut().mark_shell_projection_dirty();
    server.render_now();
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(70)),
        Some(first_workspace_id)
    );
    assert_eq!(
        server.app.state().workspaces().bookmark().as_ref(),
        Some(&second_workspace_id)
    );

    let epoch_before = server.view_epoch;
    let result = server.handle_client_shell_command(
        ClientId::test_new(70),
        EndpointCommand::PaneSelectionRead(PaneSelectionReadParams {
            pane_id: first_pane_id,
            anchor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
        }),
    );
    let changed = server.view_epoch != epoch_before;
    // The pane holds no text, so the read itself is refused. No request acts
    // on the bookmark, so nothing about it or the client's view moves.
    assert_eq!(
        result,
        Err(shepr_protocol::command::EndpointError::Unavailable(
            "selection text is unavailable".into()
        ))
    );
    assert!(!changed, "a refused read changes nothing to render");
    assert_eq!(
        server.app.state().workspaces().bookmark().as_ref(),
        Some(&second_workspace_id)
    );
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(70)),
        Some(first_workspace_id)
    );

    server.render_now();
    let shell = server.clients[&ClientId::test_new(70)].shell_state();
    assert_eq!(shell.session_generation, server.shell_session_generation);
    assert_eq!(
        shell
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.focused_workspace_id.as_ref()),
        Some(&first_workspace_id),
        "the per-client projection remains on its own location"
    );
    assert!(
        control.try_recv().is_err(),
        "unchanged projection needs no replacement"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_local_navigation_does_not_emit_global_focus_transitions() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("independent-focus");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("independent-focus-second");
    let second_pane = second.tree().root();
    let (first_runtime, mut first_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    let (second_runtime, mut second_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.insert_test_runtime(first_pane, first_runtime);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_workspace_id = server.app.state().ws(1).id();

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(62), &second_workspace_id));
    server
        .clients
        .get_mut(&ClientId::test_new(61))
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = crate::server::clients::OuterFocus::Focused;
    server
        .clients
        .get_mut(&ClientId::test_new(62))
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = crate::server::clients::OuterFocus::Focused;
    // Each focused viewer's pane gains focus once.
    server.sync_pane_focus();
    assert_eq!(
        first_input.try_recv().expect("first viewer focus gained"),
        Bytes::from_static(b"\x1b[I")
    );
    assert_eq!(
        second_input.try_recv().expect("second viewer focus gained"),
        Bytes::from_static(b"\x1b[I")
    );

    let result = server.handle_client_shell_command(
        ClientId::test_new(62),
        shepr_protocol::command::EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget {
                workspace_id: second_workspace_id,
            },
        ),
    );
    assert!(result.is_ok());
    assert!(first_input.try_recv().is_err());
    assert!(second_input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_local_navigation_leaves_the_other_clients_focus_alone() {
    use shepr_protocol::command::{PaneTarget, WorkspaceCloseParams, WorkspaceTarget};

    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("focus-events");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("focus-events-second");
    let second_pane = second.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let first_workspace_id = server.app.state().ws(0).id();
    let second_workspace_id = server.app.state().ws(1).id();
    let first_pane_id = server
        .app
        .state()
        .pane(first_pane)
        .expect("pane exists")
        .public_id();
    let second_pane_id = server
        .app
        .state()
        .pane(second_pane)
        .expect("pane exists")
        .public_id();

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    let first_target = WorkspaceTarget {
        workspace_id: first_workspace_id,
    };
    let second_target = WorkspaceTarget {
        workspace_id: second_workspace_id,
    };
    let cases = [
        (61, EndpointCommand::WorkspaceFocus(second_target.clone())),
        (62, EndpointCommand::WorkspaceFocus(second_target.clone())),
        (61, EndpointCommand::WorkspaceFocus(second_target.clone())),
        (61, EndpointCommand::WorkspaceFocus(first_target)),
        (
            62,
            EndpointCommand::PaneFocus(PaneTarget {
                pane_id: second_pane_id,
            }),
        ),
        (
            62,
            EndpointCommand::PaneFocus(PaneTarget {
                pane_id: first_pane_id,
            }),
        ),
        (61, EndpointCommand::WorkspaceFocus(second_target.clone())),
        (
            61,
            EndpointCommand::WorkspaceClose(WorkspaceCloseParams {
                workspace_id: second_target.workspace_id,
            }),
        ),
    ];
    for (client_id, command) in cases {
        let other_client = ClientId::test_new(if client_id == 61 { 62 } else { 61 });
        let other_focus = server.shell_focus_target(other_client);
        let result = server.handle_client_shell_command(client_id.into(), command);
        assert!(result.is_ok(), "client {client_id}");

        assert_eq!(
            server.shell_focus_target(other_client),
            other_focus,
            "client {client_id}"
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn navigation_moves_pane_focus_between_workspaces_once() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("navigation-focus-events");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("navigation-focus-second");
    let second_pane = second.tree().root();
    let (first_runtime, mut first_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    let (second_runtime, mut second_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.insert_test_runtime(first_pane, first_runtime);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_workspace_id = server.app.state().ws(1).id();

    let (first_control, _) = connect_matching_test_shell(&mut server, 63);
    let (second_control, _) = connect_matching_test_shell(&mut server, 64);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(64), &second_workspace_id));
    server
        .clients
        .get_mut(&ClientId::test_new(63))
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = crate::server::clients::OuterFocus::Focused;
    server
        .clients
        .get_mut(&ClientId::test_new(64))
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = crate::server::clients::OuterFocus::Focused;
    server.sync_pane_focus();
    for input in [&mut first_input, &mut second_input] {
        assert_eq!(
            input.try_recv().expect("focused viewer gained focus"),
            Bytes::from_static(b"\x1b[I")
        );
    }

    // The first viewer joins the second one's workspace: its old pane loses
    // focus, and the pane both now view, already focused, gains nothing again.
    let result = server.handle_client_shell_command(
        ClientId::test_new(63),
        shepr_protocol::command::EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget {
                workspace_id: second_workspace_id,
            },
        ),
    );
    assert!(result.is_ok());

    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(63)),
        Some(second_workspace_id)
    );
    assert_eq!(
        first_input
            .try_recv()
            .expect("previous workspace focus lost"),
        Bytes::from_static(b"\x1b[O")
    );
    assert!(
        second_input.try_recv().is_err(),
        "focus gain was duplicated"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn workspace_focus_moves_only_its_client() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("first");
    let second = shepr_mux::workspace::Workspace::test_new("second");
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let first_workspace_id = server.app.state().ws(0).id();
    let second_workspace_id = server.app.state().ws(1).id();

    let (first_control, _) = connect_test_shell(&mut server, 41, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 42, 80, 24);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    assert!(command_through_server(
        &mut server,
        41,
        EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
            workspace_id: second_workspace_id,
        }),
    ));

    let first_location = &server.clients[&ClientId::test_new(41)]
        .shell_state()
        .location;
    let second_location = &server.clients[&ClientId::test_new(42)]
        .shell_state()
        .location;
    assert_eq!(
        first_location.focused_workspace_id(),
        Some(&second_workspace_id)
    );
    assert_eq!(
        second_location.focused_workspace_id(),
        Some(&first_workspace_id)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_focus_replaces_a_diverged_client_shell_projection() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("first");
    let first_pane = first.tree().root();

    let second = shepr_mux::workspace::Workspace::test_new("second");
    let second_pane = second.tree().root();

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"FIRST_AGENT"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"SECOND_WORKSPACE"),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let first_workspace_id = server.app.state().ws(0).id();
    let first_pane_id = server
        .app
        .state()
        .pane(first_pane)
        .expect("pane exists")
        .public_id();
    let second_workspace_id = server.app.state().ws(1).id();

    let (control_rx, render_rx) = connect_test_shell(&mut server, 9, 80, 23);
    let mut render_rx = PaneSurfaceReceiver::new(render_rx);
    let _ = client_shell_snapshot(&control_rx);
    assert!(server.place_test_client_on_workspace(ClientId::test_new(9), &second_workspace_id));
    // The sole shell already sized every workspace, so the claim only records
    // the controller.
    let _ = server
        .claim_shell_workspace_geometry(ClientId::test_new(9), client_views::PendingResumes::Defer);
    assert_eq!(
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(9))
    );
    server.render_now();
    let diverged = client_shell_snapshot(&control_rx);
    assert_eq!(
        diverged.focused_workspace_id.as_ref(),
        Some(&server.app.state().ws(1).id())
    );
    let diverged_surface = recv_pane_surface(&mut render_rx, "diverged surface");
    assert!(frame_text(&diverged_surface.frame).contains("SECOND_WORKSPACE"));

    let result = server.handle_client_shell_command(
        ClientId::test_new(9),
        EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget {
            pane_id: first_pane_id,
        }),
    );
    let Ok(shepr_protocol::command::EndpointReply::PaneInfo { pane }) = result else {
        panic!("expected pane info");
    };
    assert_eq!(pane.pane_id, first_pane_id);
    assert_eq!(server.app.state().bookmark_index(), Some(0));
    let location = &server.clients[&ClientId::test_new(9)]
        .shell_state()
        .location;
    assert_eq!(location.focused_workspace_id(), Some(&first_workspace_id));

    server.render_now();
    let replacement = client_shell_snapshot(&control_rx);
    assert_eq!(
        replacement.focused_workspace_id.as_ref(),
        Some(&first_workspace_id)
    );
    let replacement_surface = recv_pane_surface(&mut render_rx, "pane focus replacement surface");
    assert!(frame_text(&replacement_surface.frame).contains("FIRST_AGENT"));
    assert!(!frame_text(&replacement_surface.frame).contains("SECOND_WORKSPACE"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn workspace_focus_replaces_the_client_shell_projection() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("first");
    let second = shepr_mux::workspace::Workspace::test_new("second");
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_id = server.app.projection_input().workspaces[1].workspace_id;

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: ClientId::test_new(9),
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 23),
                shepr_core::geometry::HostCell::Unknown
            ),
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let initial_revision = client_shell_snapshot(&control_rx).revision;

    assert!(command_through_server(
        &mut server,
        9,
        EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
            workspace_id: second_id,
        }),
    ));
    assert_eq!(server.app.state().bookmark_index(), Some(1));
    server.render_now();

    let replacement = client_shell_snapshot(&control_rx);
    assert!(replacement.revision > initial_revision);
    assert_eq!(replacement.focused_workspace_id.as_ref(), Some(&second_id));
    match read_server_message(render_rx.recv().expect("replacement pane surface")) {
        ServerMessage::PaneSurface(surface) => {
            assert_eq!(surface.projection_revision, replacement.revision);
        }
        other => panic!("expected replacement pane surface, got {other:?}"),
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_death_reconciles_each_client_view_and_focus() {
    let mut server = test_headless_server();
    let doomed = shepr_mux::workspace::Workspace::test_new("pane-death-views");
    let dead_pane = doomed.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("pane-death-views-second");
    let second_pane = second.tree().root();
    let (second_runtime, mut second_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![doomed, second]);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_workspace_id = server.app.state().ws(1).id();

    let (first_control, _) = connect_test_shell(&mut server, 71, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 72, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(72), &second_workspace_id));
    server
        .clients
        .get_mut(&ClientId::test_new(71))
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = crate::server::clients::OuterFocus::Focused;
    server
        .clients
        .get_mut(&ClientId::test_new(72))
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = crate::server::clients::OuterFocus::Unfocused;

    server.app.insert_idle_test_runtime(dead_pane);
    let died = server.app.from_pane_runtime(
        dead_pane,
        shepr_mux::events::RuntimeEvent::PaneDied {
            ending: shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Exited),
            ended_at: std::time::Instant::now(),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(died));

    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(71)),
        Some(second_workspace_id)
    );
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(72)),
        Some(second_workspace_id)
    );
    assert_eq!(
        second_input
            .try_recv()
            .expect("fallback focus gained input"),
        Bytes::from_static(b"\x1b[I")
    );
    assert!(
        second_input.try_recv().is_err(),
        "focus gain was duplicated"
    );
    assert_eq!(
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(71))
    );
    let before_resize = server.app.test_runtime(second_pane).current_size();
    assert!(server.test_handle_server_event(ServerEvent::ShellResize {
        client_id: ClientId::test_new(71),
        geometry: shepr_core::geometry::HostGeometry::new(
            shepr_core::geometry::GridSize::clamped(90, 25),
            shepr_core::geometry::HostCell::Unknown
        ),
    }));
    assert!(settle_pane_resizes(&mut server));
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        before_resize
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn client_shell_focus_promotes_and_reaches_reporting_pane() {
    with_terminal_session_test_server(|server, terminal_id, _other_terminal_id, _pane_id| {
        let (runtime, mut input_rx) =
            shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                b"\x1b[?1004h",
                4,
            );
        server.app.test_runtimes_mut().insert(terminal_id, runtime);
        server.app.test_state_mut().seed_bookmark_index(Some(0));
        server.insert_test_client(
            1,
            ClientConnection::new(
                (80, 24),
                shepr_core::geometry::HostCell::Unknown,
                1,
                crate::server::outbox::ClientOutbox::detached(),
            ),
        );
        server.insert_test_client(
            2,
            ClientConnection::new(
                (100, 30),
                shepr_core::geometry::HostCell::Unknown,
                2,
                crate::server::outbox::ClientOutbox::detached(),
            ),
        );
        let _first_lanes = attach_test_writer(server, 1);
        let _second_lanes = attach_test_writer(server, 2);
        server
            .clients
            .set_foreground_client_id(Some(ClientId::test_new(2)));
        assert!(server.claim_unowned_shell_workspace_geometry(
            ClientId::test_new(2),
            client_views::PendingResumes::Start
        ));
        assert_eq!(
            server
                .app
                .test_runtimes_mut()
                .get(&terminal_id)
                .expect("focused runtime")
                .current_size(),
            (28, 97)
        );

        assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
            client_id: ClientId::test_new(1),
            focused: true,
        }));
        assert_eq!(
            server.clients.foreground_client_id(),
            Some(ClientId::test_new(1))
        );
        assert_eq!(
            outer_terminal_focus(server, ClientId::test_new(1)),
            Some(true)
        );
        assert_eq!(
            server
                .app
                .test_runtimes_mut()
                .get(&terminal_id)
                .expect("focused runtime")
                .current_size(),
            (22, 77)
        );
        assert_eq!(
            input_rx.try_recv().expect("focus gained input"),
            Bytes::from_static(b"\x1b[I")
        );

        assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
            client_id: ClientId::test_new(2),
            focused: true,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "second viewer duplicated focus gain"
        );
        assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
            client_id: ClientId::test_new(1),
            focused: false,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "remaining viewer lost pane focus"
        );
        assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
            client_id: ClientId::test_new(2),
            focused: false,
        }));
        assert_eq!(
            outer_terminal_focus(server, ClientId::test_new(2)),
            Some(false)
        );
        assert_eq!(
            input_rx.try_recv().expect("last viewer focus lost input"),
            Bytes::from_static(b"\x1b[O")
        );
    });
}
