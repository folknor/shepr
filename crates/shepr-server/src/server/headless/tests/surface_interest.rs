use super::*;
use shepr_protocol::command::{EndpointCommand, EndpointReply};

fn surface_set(active: bool) -> Box<EndpointCommand> {
    Box::new(EndpointCommand::ClientShellSurfaceSet(
        shepr_api::schema::ClientShellSurfaceSetParams { active },
    ))
}

fn request_active_surface(server: &mut HeadlessServer, client_id: u64, request_id: &str) {
    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: client_id.into(),
            boot_id,
            request_id: request_id.into(),
            command: surface_set(true),
        })
    );
}

#[tokio::test]
async fn metadata_only_shell_is_isolated_until_surface_activation() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    let pane_id = server
        .app
        .session_snapshot()
        .focused_pane_id
        .expect("test precondition");
    let workspace_id = server
        .app
        .session_snapshot()
        .focused_workspace_id
        .expect("test precondition");
    let (writer, control_rx, render_rx) = test_client_writer();
    let client_id = ClientId::test_new(52);

    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id,
            surface_cols: 101,
            surface_rows: 37,
            cell_width_px: 9,
            cell_height_px: 18,
            pixel_mouse: true,
            mouse_capture: true,
            surface_active: false,
            writer,
        })
    );
    let _ = client_shell_snapshot(&control_rx);
    assert_eq!(server.clients.foreground_client_id(), None);
    // A metadata-only connection sizes no workspace.
    assert_eq!(server.app.state.workspace_area(0), None);

    server.render_and_stream();
    assert!(render_rx.try_recv().is_err());
    assert!(
        server.clients[&client_id]
            .render_state
            .last_pane_surface()
            .is_none()
    );

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id,
            pane_id: pane_id.parse().expect("test precondition"),
            events: vec![shepr_protocol::ClientPaneInputEvent::Paste(
                "blocked".into()
            )],
        })
    );
    assert!(input_rx.try_recv().is_err());

    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request_id: "inactive-mutation".into(),
            command: Box::new(EndpointCommand::WorkspaceFocus(
                shepr_api::schema::WorkspaceTarget {
                    workspace_id: workspace_id.to_string(),
                },
            )),
        })
    );
    let ServerMessage::ClientShellEndpointResponse {
        result: Err(error), ..
    } = read_server_message(control_rx.recv().expect("inactive mutation response"))
    else {
        panic!("expected endpoint error response");
    };
    assert_eq!(error.code, "surface_inactive");

    assert!(server.send_to_client(
        client_id,
        &ServerMessage::ClientShellError {
            kind: shepr_protocol::NoticeKind::PaneInputDropped {
                pane_id: pane_id.parse().expect("test precondition"),
                events: 1
            },
        }
    ));
    assert!(matches!(
        read_server_message(control_rx.recv().expect("metadata notification")),
        ServerMessage::ClientShellError { .. }
    ));

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request_id: "activate-surface".into(),
            command: surface_set(true),
        })
    );
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
        server.app.state.workspace_area(0),
        Some(ratatui::layout::Rect::new(0, 0, 101, 37))
    );

    server.render_and_stream();
    let ServerMessage::PaneSurface(surface) =
        read_server_message(render_rx.recv().expect("activated surface"))
    else {
        panic!("expected pane surface");
    };
    assert_eq!((surface.frame.width, surface.frame.height), (101, 37));
    assert!(surface.projection_revision >= activation_floor);
    assert_eq!(surface.surface_revision, 1);

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request_id: "deactivate-surface".into(),
            command: surface_set(false),
        })
    );
    let _ = control_rx.recv().expect("surface deactivation response");
    assert!(server.clients.contains_key(&client_id));
    let (_, runtime_pane_id) = server
        .app
        .parse_pane_id(&surface.panes[0].pane_id)
        .expect("test precondition");
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, runtime_pane_id)
        .expect("test precondition")
        .test_process_pty_bytes(b"REACTIVATED");
    assert!(
        server.render_retained_pane_surface_and_stream(&std::collections::HashSet::from([
            runtime_pane_id
        ]))
    );
    assert!(render_rx.try_recv().is_err());

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id,
            request_id: "reactivate-surface".into(),
            command: surface_set(true),
        })
    );
    let result = loop {
        let message =
            read_server_message(control_rx.recv().expect("surface reactivation response"));
        match message {
            ServerMessage::ClientShellEndpointResponse {
                request_id, result, ..
            } if request_id == "reactivate-surface" => break result,
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
    server.render_and_stream();
    let ServerMessage::PaneSurface(surface) =
        read_server_message(render_rx.recv().expect("reactivated surface"))
    else {
        panic!("expected replacement pane surface");
    };
    assert!(frame_text(&surface.frame).contains("REACTIVATED"));
    assert!(surface.projection_revision >= reactivation_floor);
    assert_eq!(surface.surface_revision, 2);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn background_surface_activation_preserves_focused_viewer_geometry() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (focused_control, _) = connect_test_shell(&mut server, 7, 68, 17);
    let _ = focused_control.recv().expect("focused client snapshot");
    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: ClientId::test_new(7),
        focused: true,
    }));
    let focused_size = server.app.test_runtime(pane_id).current_size();
    assert_eq!(focused_size, (17, 67));
    let shared_workspace_id = server
        .shell_target_for_client(ClientId::test_new(7))
        .expect("focused workspace");
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(7))
    );

    let (writer, background_control, _background_render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: ClientId::test_new(8),
            surface_cols: 100,
            surface_rows: 35,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: false,
            writer,
        })
    );
    let _ = background_control
        .recv()
        .expect("background client snapshot");

    request_active_surface(&mut server, 8, "activate-background-surface");
    let _ = background_control
        .recv()
        .expect("background surface activation response");
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(8)),
        Some(shared_workspace_id.clone())
    );
    assert_eq!(
        server.clients[&7].shell_state().outer_terminal_focus,
        Some(true)
    );
    assert_eq!(server.clients[&8].shell_state().outer_terminal_focus, None);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        focused_size,
        "surface activation must not transiently resize a focused viewer's workspace"
    );
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(7))
    );

    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: ClientId::test_new(8),
        focused: false,
    }));
    assert_eq!(
        server.clients[&7].shell_state().outer_terminal_focus,
        Some(true)
    );
    assert_eq!(
        server.clients[&8].shell_state().outer_terminal_focus,
        Some(false)
    );
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        focused_size
    );
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(7))
    );

    request_active_surface(&mut server, 8, "synchronize-background-surface");
    let _ = background_control
        .recv()
        .expect("background presentation synchronization response");
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
    assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: ClientId::test_new(8),
        focused: true,
    }));
    let shared_workspace_id = server
        .shell_target_for_client(ClientId::test_new(8))
        .expect("focused workspace");

    let (other_control, _) = connect_test_shell(&mut server, 7, 68, 17);
    let _ = other_control.recv().expect("other client snapshot");
    assert!(server.claim_shell_workspace_geometry(ClientId::test_new(7), false));
    assert_eq!(server.app.test_runtime(pane_id).current_size(), (17, 67));

    request_active_surface(&mut server, 8, "reassert-focused-surface");
    let _ = focused_control
        .recv()
        .expect("focused surface reassertion response");

    assert_eq!(
        server.clients[&8].shell_state().outer_terminal_focus,
        Some(true)
    );
    assert_eq!(server.app.test_runtime(pane_id).current_size(), (35, 99));
    assert_eq!(
        server.clients.geometry_controller(&shared_workspace_id),
        Some(ClientId::test_new(8))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn presentation_sync_epoch_replays_modes_and_title() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(63);
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
            mouse_capture: true,
            surface_active: true,
            writer,
        })
    );
    let _ = client_shell_snapshot(&control_rx);
    server.app.configure_window_title("target title");
    {
        let client = server
            .clients
            .get_mut(&client_id)
            .expect("test precondition");
        client.host_mouse_capture_active = Some(false);
        client.host_sgr_pixels_active = Some(false);
        client.shell_state_mut().host_keyboard_report_all_active = Some(false);
    }

    let boot_id = server.client_shell_boot_id.clone();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id,
            request_id: "post-commit-reassert".into(),
            command: surface_set(true),
        })
    );
    let _ = control_rx
        .recv()
        .expect("typed surface reassertion acknowledgement");
    server.stream_host_mouse_capture_mode();
    server.stream_shell_keyboard_mode();
    server.sync_window_title();
    assert_eq!(
        server.clients[&client_id].host_mouse_capture_active,
        Some(true),
        "the target mode is sent after, not during, the frozen handoff"
    );
    assert_eq!(
        server.clients[&client_id]
            .shell_state()
            .host_keyboard_report_all_active,
        Some(false)
    );
    let messages = (0..3)
        .map(|_| read_server_message(control_rx.recv().expect("reassertion effect")))
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, ServerMessage::MouseCapture { enabled: true, .. }))
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::ClientShellKeyboardReportAll { enabled: false }
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::WindowTitle { title: Some(title) } if title == "target title"
    )));
    shutdown_test_runtimes(&mut server);
}
