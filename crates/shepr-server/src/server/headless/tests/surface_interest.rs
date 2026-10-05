use super::*;
use shepr_protocol::command::{EndpointCommand, EndpointReply};

fn surface_set(active: bool) -> Box<EndpointCommand> {
    Box::new(EndpointCommand::ClientShellSurfaceSet(
        shepr_protocol::command::ClientShellSurfaceSetParams { active },
    ))
}

fn request_active_surface(server: &mut HeadlessServer, client_id: u64) {
    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
            client_id: client_id.into(),
            boot_id,
            request_id: shepr_protocol::RequestId::allocate(),
            command: surface_set(true),
        })
    );
    // The acknowledgement waits in the client's ordered reply queue, as every
    // endpoint reply does; the loop flushes it once the pass has rendered.
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
}

#[tokio::test]
async fn metadata_only_shell_is_isolated_until_surface_activation() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    let pane_id = focused_test_pane(&server);
    let workspace_id = server.app.state().ws(0).id();
    let (writer, control_rx, render_rx) = test_client_writer();
    let client_id = ClientId::test_new(52);

    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(101, 37),
                shepr_core::geometry::HostCell::from_host(9, 18, true)
            ),
            mouse_capture: true,
            surface_active: false,
            outbox: writer,
        })
    );
    let _ = client_shell_snapshot(&control_rx);
    assert_eq!(server.clients.foreground_client_id(), None);
    // A metadata-only connection sizes no workspace.
    assert_eq!(server.app.state().ws(0).spawn_geometry(), None);

    server.render_now();
    assert!(render_rx.try_recv().is_err());
    assert!(
        server.clients[&client_id]
            .render_state
            .last_pane_surface()
            .is_none()
    );

    assert!(
        !server.test_handle_server_event(ServerEvent::ShellPaneInput {
            client_id,
            pane_id,
            events: vec![shepr_protocol::ClientPaneInputEvent::Paste(
                "blocked".into()
            )],
        })
    );
    assert!(input_rx.try_recv().is_err());

    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        !server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request_id: shepr_protocol::RequestId::allocate(),
            command: Box::new(EndpointCommand::WorkspaceFocus(
                shepr_protocol::command::WorkspaceTarget { workspace_id },
            )),
        })
    );
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let ServerMessage::ClientShellEndpointResponse {
        result: Err(error), ..
    } = read_server_message(control_rx.recv().expect("inactive mutation response"))
    else {
        panic!("expected endpoint error response");
    };
    assert_eq!(
        error,
        shepr_protocol::command::EndpointError::SurfaceInactive
    );

    assert!(server.send_to_client(
        client_id,
        &ServerMessage::ClientShellError {
            kind: shepr_protocol::NoticeKind::PaneInputDropped { pane_id, events: 1 },
        }
    ));
    assert!(matches!(
        read_server_message(control_rx.recv().expect("metadata notification")),
        ServerMessage::ClientShellError { .. }
    ));

    assert!(
        server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request_id: shepr_protocol::RequestId::allocate(),
            command: surface_set(true),
        })
    );
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let ServerMessage::ClientShellEndpointResponse {
        result:
            Ok(EndpointReply::ClientShellSurfaceSet {
                active: true,
                projection_revision: activation_floor,
            }),
        ..
    } = read_server_message(control_rx.recv().expect("surface activation response"))
    else {
        panic!("expected typed surface activation response");
    };
    assert_eq!(server.clients.foreground_client_id(), Some(client_id));
    assert_eq!(
        server
            .app
            .state()
            .ws(0)
            .spawn_geometry()
            .map(|geometry| geometry.area),
        Some(Rect::new(0, 0, 101, 37))
    );

    server.render_now();
    let ServerMessage::PaneSurface(surface) =
        read_server_message(render_rx.recv().expect("activated surface"))
    else {
        panic!("expected pane surface");
    };
    assert_eq!((surface.frame.width(), surface.frame.height()), (101, 37));
    assert!(surface.projection_revision >= activation_floor);
    assert_eq!(
        surface.surface_revision,
        shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(1)
    );

    assert!(
        server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request_id: shepr_protocol::RequestId::allocate(),
            command: surface_set(false),
        })
    );
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let _ = control_rx.recv().expect("surface deactivation response");
    let plan = server.render_plan(false);
    server.render_pass(&plan, &HashSet::new());
    assert!(server.clients.contains_key(&client_id));
    let runtime_pane_id = server
        .app
        .state()
        .resolve_pane(&surface.panes[0].pane_id)
        .expect("test precondition")
        .id();
    server
        .app
        .pane_runtime(runtime_pane_id)
        .expect("test precondition")
        .test_process_pty_bytes(b"REACTIVATED");
    assert!(server.try_render_patches(&std::collections::HashSet::from([runtime_pane_id])));
    assert!(render_rx.try_recv().is_err());

    let reactivate = shepr_protocol::RequestId::allocate();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
            client_id,
            boot_id,
            request_id: reactivate.clone(),
            command: surface_set(true),
        })
    );
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let result = loop {
        let message =
            read_server_message(control_rx.recv().expect("surface reactivation response"));
        match message {
            ServerMessage::ClientShellEndpointResponse {
                request_id, result, ..
            } if request_id == reactivate => break result,
            ServerMessage::EndpointSnapshot(_)
            | ServerMessage::ClientShellEndpointResponse { .. } => continue,
            other => panic!("unexpected surface reactivation message: {other:?}"),
        }
    };
    let Ok(EndpointReply::ClientShellSurfaceSet {
        active: true,
        projection_revision: reactivation_floor,
    }) = result
    else {
        panic!("expected typed surface reactivation result");
    };
    assert!(reactivation_floor > activation_floor);
    server.render_now();
    let ServerMessage::PaneSurface(surface) =
        read_server_message(render_rx.recv().expect("reactivated surface"))
    else {
        panic!("expected replacement pane surface");
    };
    assert!(frame_text(&surface.frame).contains("REACTIVATED"));
    assert!(surface.projection_revision >= reactivation_floor);
    assert_eq!(
        surface.surface_revision,
        shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(2)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn background_surface_activation_preserves_focused_viewer_geometry() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (focused_control, _) = connect_test_shell(&mut server, 7, 68, 17);
    let _ = focused_control.recv().expect("focused client snapshot");
    assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
        client_id: ClientId::test_new(7),
        focused: true,
    }));
    let focused_size = server.app.test_runtime(pane_id).current_size();
    assert_eq!(focused_size, (15, 65));
    let shared_workspace_id = server
        .shell_target_for_client(ClientId::test_new(7))
        .expect("focused workspace");
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(7))
    );

    let (writer, background_control, _background_render) = test_client_writer();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: ClientId::test_new(8),
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(100, 35),
                shepr_core::geometry::HostCell::Unknown
            ),
            mouse_capture: false,
            surface_active: false,
            outbox: writer,
        })
    );
    let _ = background_control
        .recv()
        .expect("background client snapshot");

    request_active_surface(&mut server, 8);
    let _ = background_control
        .recv()
        .expect("background surface activation response");
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(8)),
        Some(shared_workspace_id)
    );
    assert_eq!(
        server.clients[&ClientId::test_new(7)]
            .shell_state()
            .outer_terminal_focus,
        crate::server::clients::OuterFocus::Focused
    );
    assert_eq!(
        server.clients[&ClientId::test_new(8)]
            .shell_state()
            .outer_terminal_focus,
        crate::server::clients::OuterFocus::Unreported
    );
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        focused_size,
        "surface activation must not transiently resize a focused viewer's workspace"
    );
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(7))
    );

    assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
        client_id: ClientId::test_new(8),
        focused: false,
    }));
    assert_eq!(
        server.clients[&ClientId::test_new(7)]
            .shell_state()
            .outer_terminal_focus,
        crate::server::clients::OuterFocus::Focused
    );
    assert_eq!(
        server.clients[&ClientId::test_new(8)]
            .shell_state()
            .outer_terminal_focus,
        crate::server::clients::OuterFocus::Unfocused
    );
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        focused_size
    );
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(7))
    );

    request_active_surface(&mut server, 8);
    let _ = background_control
        .recv()
        .expect("background view reassertion response");
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        focused_size
    );
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(7))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn focused_surface_reassertion_reclaims_workspace_geometry() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (focused_control, _) = connect_test_shell(&mut server, 8, 100, 35);
    let _ = focused_control.recv().expect("focused client snapshot");
    assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
        client_id: ClientId::test_new(8),
        focused: true,
    }));
    let shared_workspace_id = server
        .shell_target_for_client(ClientId::test_new(8))
        .expect("focused workspace");

    let (other_control, _) = connect_test_shell(&mut server, 7, 68, 17);
    let _ = other_control.recv().expect("other client snapshot");
    assert!(server.claim_shell_workspace_geometry(
        ClientId::test_new(7),
        client_views::PendingResumes::Defer
    ));
    assert_eq!(server.app.test_runtime(pane_id).current_size(), (15, 65));

    request_active_surface(&mut server, 8);
    let _ = focused_control
        .recv()
        .expect("focused surface reassertion response");

    assert_eq!(
        server.clients[&ClientId::test_new(8)]
            .shell_state()
            .outer_terminal_focus,
        crate::server::clients::OuterFocus::Focused
    );
    assert_eq!(server.app.test_runtime(pane_id).current_size(), (33, 97));
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(8))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn navigation_reapplies_geometry_for_the_workspace_left_behind() {
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
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let first_id = server.app.state().ws(0).id();
    let second_id = server.app.state().ws(1).id();

    let (first_control, _first_render) = connect_test_shell(&mut server, 7, 200, 60);
    let _ = client_shell_snapshot(&first_control);
    let (second_control, _second_render) = connect_test_shell(&mut server, 8, 100, 30);
    let _ = client_shell_snapshot(&second_control);
    assert_eq!(
        server
            .app
            .state()
            .ws(0)
            .spawn_geometry()
            .map(|geometry| geometry.area),
        Some(Rect::new(0, 0, 200, 60))
    );

    // The destination already remembers client 7 as its controller by the
    // time it returns to it, so the final move cannot rely on a new claim to
    // trigger geometry settlement.
    for workspace_id in [&second_id, &first_id, &second_id] {
        let result = server.handle_client_shell_command(
            ClientId::test_new(7),
            EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
                workspace_id: *workspace_id,
            }),
        );
        assert!(result.is_ok());
    }

    assert_eq!(
        server.workspace_geometry_source(&first_id),
        Some(super::super::client_views::GeometrySource::Client(
            ClientId::test_new(8)
        ))
    );
    assert_eq!(
        server
            .app
            .state()
            .ws(0)
            .spawn_geometry()
            .map(|geometry| geometry.area),
        Some(Rect::new(0, 0, 100, 30))
    );
    assert_eq!(server.app.test_runtime(first_pane).current_size(), (28, 97));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn unchanged_geometry_application_does_not_force_surface_recompute() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control, _render) = connect_test_shell(&mut server, 7, 80, 23);
    let _ = client_shell_snapshot(&control);
    server.render_now();
    assert!(
        !server.clients[&ClientId::test_new(7)]
            .render_state
            .requires_recompute()
    );
    let applied_size = server.app.test_runtime(pane_id).current_size();

    assert!(
        !server.reapply_controlled_shell_workspace_geometry(client_views::PendingResumes::Defer)
    );
    assert!(
        !server.clients[&ClientId::test_new(7)]
            .render_state
            .requires_recompute()
    );
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        applied_size
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn replay_host_effects_replays_modes() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(63);
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(80, 24),
                shepr_core::geometry::HostCell::from_host(8, 16, false)
            ),
            mouse_capture: true,
            surface_active: true,
            outbox: writer,
        })
    );
    let _ = client_shell_snapshot(&control_rx);
    {
        let client = server
            .clients
            .get_mut(&client_id)
            .expect("test precondition");
        client
            .outbox
            .tell_mouse_capture(shepr_term::mouse::HostMouseCapture::Off);
        client.outbox.tell_keyboard_report_all(false);
    }
    for _ in 0..2 {
        control_rx.recv().expect("setup presentation effect");
    }

    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
            client_id,
            boot_id,
            request_id: shepr_protocol::RequestId::allocate(),
            command: surface_set(true),
        })
    );
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let _ = control_rx
        .recv()
        .expect("typed surface reassertion acknowledgement");
    server.test_handle_server_event(ServerEvent::ShellReplayHostEffects { client_id });
    assert!(
        matches!(
            server.clients[&client_id].outbox.told_mouse_capture(),
            Some(
                shepr_term::mouse::HostMouseCapture::Cells
                    | shepr_term::mouse::HostMouseCapture::Pixels
            )
        ),
        "the committed target replays its host mode"
    );
    assert_eq!(
        server.clients[&client_id].outbox.told_keyboard_report_all(),
        Some(false)
    );
    let messages = (0..2)
        .map(|_| read_server_message(control_rx.recv().expect("reassertion effect")))
        .collect::<Vec<_>>();
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::MouseCapture {
            mode: shepr_term::mouse::HostMouseCapture::Cells
                | shepr_term::mouse::HostMouseCapture::Pixels
        }
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::ClientShellKeyboardReportAll { enabled: false }
    )));
    assert!(
        control_rx.try_recv().is_err(),
        "no readiness message follows replay"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn replay_host_effects_is_ignored_by_a_non_viewed_connection() {
    let mut server = test_headless_server();
    let (writer, control, _render) = test_client_writer();
    let client_id = ClientId::test_new(64);
    server.test_handle_server_event(ServerEvent::ShellConnected {
        client_id,
        geometry: shepr_core::geometry::HostGeometry::new(
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_core::geometry::HostCell::from_host(8, 16, false),
        ),
        mouse_capture: true,
        surface_active: false,
        outbox: writer,
    });
    let _ = client_shell_snapshot(&control);
    server.test_handle_server_event(ServerEvent::ShellReplayHostEffects { client_id });
    assert!(control.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}
