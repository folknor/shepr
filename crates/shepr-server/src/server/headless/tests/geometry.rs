use super::*;

#[tokio::test]
async fn a_workspace_appearing_resets_the_creation_retry() {
    let mut server = test_headless_server();
    let now = server.app.clock().now;
    server.schedule.creation.failed(now);
    server.schedule.creation.failed(now);
    assert!(server.schedule.creation.deadline(now).is_some());

    // A workspace the loop did not create (restore, an API request) appears.
    install_shared_view_test_runtime(&mut server);
    assert!(!server.create_automatic_workspace(None));

    assert_eq!(server.schedule.creation.deadline(now), None);
    server.schedule.creation.failed(now);
    assert_eq!(
        server.schedule.creation.deadline(now),
        Some(now + crate::limits::DEFAULT_WORKSPACE_RETRY_MIN)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn default_headless_size_lays_out_workspaces_without_clients() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);

    assert_eq!(
        server.app.state().settings().headless_size,
        shepr_core::geometry::GridSize::clamped(
            shepr_config::DEFAULT_HEADLESS_COLS,
            shepr_config::DEFAULT_HEADLESS_ROWS
        )
    );
    assert_eq!(server.app.state().ws(0).spawn_geometry(), None);
    server.render_now();
    let headless = server.app.state().settings().headless_rect();
    assert_eq!(
        server
            .app
            .state()
            .ws(0)
            .spawn_geometry()
            .map(|geometry| geometry.area),
        Some(headless)
    );
    let view = server.app.render_view();
    let layout = crate::ui::compute_surface_for(
        view.state,
        view.runtimes,
        Some(crate::ui::SurfaceTarget {
            index: 0,
            id: view.state.ws(0).id(),
        }),
        crate::ui::ratatui_rect(headless),
    );
    let pane = layout.panes.first().expect("test pane geometry");
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        (pane.inner_rect.height, pane.inner_rect.width)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn last_shell_disconnect_restores_headless_pane_size() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    server.app.test_state_mut().settings_mut().headless_size =
        shepr_core::geometry::GridSize::clamped(72, 18);
    let (_control, _render) = connect_test_shell(&mut server, 7, 112, 36);
    let client_size = server.app.test_runtime(pane_id).current_size();

    assert!(server.test_handle_server_event(ServerEvent::Disconnected {
        client_id: ClientId::test_new(7),
    }));

    let target = crate::ui::SurfaceTarget {
        index: 0,
        id: server.app.state().ws(0).id(),
    };
    let view = server.app.render_view();
    let layout = crate::ui::compute_surface_for(
        view.state,
        view.runtimes,
        Some(target),
        crate::ui::ratatui_rect(view.state.settings().headless_rect()),
    );
    let pane = layout.panes.first().expect("test pane geometry");
    let headless_pane_size = (pane.inner_rect.height, pane.inner_rect.width);

    assert_eq!(
        server
            .app
            .state()
            .ws(0)
            .spawn_geometry()
            .map(|geometry| geometry.area),
        Some(server.app.state().settings().headless_rect())
    );
    assert_ne!(client_size, headless_pane_size);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        headless_pane_size
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn first_shell_surface_resizes_a_pane_that_entered_alternate_screen() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (_control, render) = connect_test_shell(&mut server, 7, 80, 23);
    let mut render = PaneSurfaceReceiver::new(render);
    let initial_size = server.app.test_runtime(pane_id).current_size();

    write_shared_test_pane(&mut server, pane_id, b"\x1b[?1049hALT");
    // Output that flips the screen also raises the render signal, and the flip
    // flag is only read on a plan made with that signal pending.
    server.mark_view_changed();
    let plan = server.render_plan(true);
    server.render_pass(&plan, &HashSet::new());

    let surface = recv_pane_surface(&mut render, "first alternate-screen surface");
    assert!(surface.panes[0].alternate_screen_active);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        (initial_size.0, initial_size.1 + 1)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn repeated_layout_action_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("layout-geometry");
    let first_pane = workspace.tree().root();
    let second_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let workspace_id = server.app.state().ws(0).id();
    let epoch = server.app.state().ws(0).tree().layout_epoch();

    let (control, _) = connect_test_shell(&mut server, 65, 100, 30);
    let _ = control.recv().expect("snapshot");
    let before = server.app.test_runtime(first_pane).current_size();

    let epoch_before = server.view_epoch;
    let result = server.handle_client_shell_command(
        ClientId::test_new(65),
        shepr_protocol::command::EndpointCommand::LayoutSetSplitRatio(
            shepr_protocol::command::LayoutSetSplitRatioParams {
                workspace_id,
                path: Vec::new(),
                epoch,
                ratio: shepr_core::layout::SplitRatio::new(0.8).expect("test split ratio is valid"),
            },
        ),
    );
    let changed = server.view_epoch != epoch_before;
    assert!(result.is_ok());
    assert!(changed);

    let after = server.app.test_runtime(first_pane).current_size();
    assert_ne!(after, before);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_close_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("public-close-geometry");
    let first_pane = workspace.tree().root();
    let second_pane = workspace.test_split(shepr_core::layout::Direction::Vertical);

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_pane_id = server
        .app
        .state()
        .pane(second_pane)
        .expect("test precondition")
        .public_id();

    let (control, _) = connect_test_shell(&mut server, 66, 100, 30);
    let _ = control.recv().expect("snapshot");
    let shrunk = server.app.test_runtime(first_pane).current_size();
    assert!(shrunk.0 < 30);

    assert!(command_through_server(
        &mut server,
        66,
        EndpointCommand::PaneClose(shepr_protocol::command::PaneTarget {
            pane_id: second_pane_id,
        }),
    ));

    let runtime = &server.app.test_runtime(first_pane);
    let grown = runtime.current_size();
    assert!(grown.0 > shrunk.0);
    assert_eq!(
        runtime.read().terminal_dimensions(),
        Some(shepr_core::geometry::GridSize::clamped(grown.1, grown.0))
    );
    assert_eq!(
        runtime
            .read()
            .scroll_metrics()
            .expect("test precondition")
            .viewport_rows,
        grown.0 as usize
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn geometry_reapply_replaces_a_controller_that_left_the_workspace() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("geometry-controller-viewer");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("geometry-controller-second");
    let second_pane = second.tree().root();
    let third = shepr_mux::workspace::Workspace::test_new("geometry-controller-third");
    let third_pane = third.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second, third]);
    for pane_id in [first_pane, second_pane, third_pane] {
        server.app.insert_test_runtime(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_workspace_id = server.app.state().ws(1).id();
    let third_workspace_id = server.app.state().ws(2).id();

    let (first_control, _) = connect_test_shell(&mut server, 67, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 68, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    assert!(server.place_test_client_on_workspace(ClientId::test_new(67), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(
        ClientId::test_new(67),
        client_views::PendingResumes::Defer
    ));
    assert!(server.place_test_client_on_workspace(ClientId::test_new(67), &third_workspace_id));
    // The workspace is already sized for this client, so only the controller
    // moves and no geometry changes.
    let _ = server.claim_shell_workspace_geometry(
        ClientId::test_new(67),
        client_views::PendingResumes::Defer,
    );
    assert_eq!(
        server.clients.geometry_controller(&third_workspace_id),
        Some(ClientId::test_new(67))
    );
    assert!(server.place_test_client_on_workspace(ClientId::test_new(68), &second_workspace_id));
    assert_eq!(
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(67))
    );
    let (lower_control, _) = connect_test_shell(&mut server, 66, 90, 25);
    let _ = lower_control.recv().expect("lower-id viewer snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(66), &second_workspace_id));
    server
        .clients
        .get_mut(&ClientId::test_new(68))
        .expect("focused viewer")
        .shell_state_mut()
        .outer_terminal_focus = crate::server::clients::OuterFocus::Focused;
    let stale_size = server.app.test_runtime(second_pane).current_size();

    assert!(
        server.reapply_controlled_shell_workspace_geometry(client_views::PendingResumes::Defer)
    );

    assert_eq!(
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(68))
    );
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        stale_size
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn controller_disconnect_hands_geometry_to_a_remaining_viewer() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("controller-disconnect");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("controller-disconnect-second");
    let second_pane = second.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    for pane_id in [first_pane, second_pane] {
        server.app.insert_test_runtime(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_workspace_id = server.app.state().ws(1).id();

    let (first_control, _) = connect_test_shell(&mut server, 31, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 32, 70, 20);
    let (third_control, _) = connect_test_shell(&mut server, 33, 60, 16);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    let _ = third_control.recv().expect("third snapshot");

    assert!(server.place_test_client_on_workspace(ClientId::test_new(32), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(
        ClientId::test_new(32),
        client_views::PendingResumes::Defer
    ));
    let remaining_viewer_size = server.app.test_runtime(second_pane).current_size();
    assert!(server.place_test_client_on_workspace(ClientId::test_new(31), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(
        ClientId::test_new(31),
        client_views::PendingResumes::Defer
    ));
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        remaining_viewer_size
    );

    // Two shells remain, so this is not the single-shell resize path.
    server.remove_client_if_present(ClientId::test_new(31));

    assert_eq!(
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(32))
    );
    assert_eq!(
        server.app.test_runtime(second_pane).current_size(),
        remaining_viewer_size
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_workspaces_render_accept_input_and_resize_independently() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("independent-geometry");
    let first_pane = first.tree().root();
    let second = shepr_mux::workspace::Workspace::test_new("independent-geometry-second");
    let second_pane = second.tree().root();

    let (second_runtime, mut second_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"SECOND_WORKSPACE",
            4,
        );

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"FIRST_WORKSPACE"),
    );
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_workspace_id = server.app.state().ws(1).id();
    let second_pane_id = server
        .app
        .state()
        .pane(second_pane)
        .expect("test precondition")
        .public_id();
    let initial_second_size = server.app.test_runtime(second_pane).current_size();

    let (first_control, first_render) = connect_test_shell(&mut server, 21, 100, 30);
    let _ = first_control.recv().expect("first snapshot");
    let first_size = server.app.test_runtime(first_pane).current_size();
    let singleton_second_size = server.app.test_runtime(second_pane).current_size();
    assert_ne!(singleton_second_size, initial_second_size);
    assert_eq!(singleton_second_size, first_size);

    let (second_control, second_render) = connect_test_shell(&mut server, 22, 70, 20);
    let _ = second_control.recv().expect("second snapshot");

    assert!(server.place_test_client_on_workspace(ClientId::test_new(22), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(
        ClientId::test_new(22),
        client_views::PendingResumes::Defer
    ));
    let second_size = server.app.test_runtime(second_pane).current_size();
    assert_ne!(first_size, second_size);
    assert_eq!(
        server.app.test_runtime(first_pane).current_size(),
        first_size
    );

    server.test_handle_server_event(ServerEvent::ShellPaneInput {
        client_id: ClientId::test_new(22),
        pane_id: second_pane_id,
        events: vec![shepr_protocol::ClientPaneInputEvent::TextCommit(
            "typed".into(),
        )],
    });
    assert_eq!(
        second_input.try_recv().expect("second workspace input"),
        Bytes::from_static(b"typed")
    );

    server.render_now();
    let first_surface = match read_server_message(first_render.recv().expect("first surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected first pane surface, got {other:?}"),
    };
    let second_surface = match read_server_message(second_render.recv().expect("second surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected second pane surface, got {other:?}"),
    };
    assert!(frame_text(&first_surface.frame).contains("FIRST_WORKSPACE"));
    assert!(frame_text(&second_surface.frame).contains("SECOND_WORKSPACE"));

    assert!(server.test_handle_server_event(ServerEvent::ShellResize {
        client_id: ClientId::test_new(22),
        geometry: shepr_core::geometry::HostGeometry::new(
            shepr_core::geometry::GridSize::clamped(60, 16),
            shepr_core::geometry::HostCell::Unknown
        ),
    }));
    let resized_second = server.app.test_runtime(second_pane).current_size();
    assert_ne!(resized_second, second_size);
    assert_eq!(
        server.app.test_runtime(first_pane).current_size(),
        first_size
    );

    assert!(server.place_test_client_on_workspace(ClientId::test_new(21), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(
        ClientId::test_new(21),
        client_views::PendingResumes::Defer
    ));
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        resized_second
    );

    server.remove_client_if_present(ClientId::test_new(21));
    let singleton_first = server.app.test_runtime(first_pane).current_size();
    let singleton_second = server.app.test_runtime(second_pane).current_size();
    assert_ne!(singleton_first, first_size);
    assert_eq!(singleton_first, singleton_second);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_death_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("pane-death-geometry");
    let first_pane = workspace.tree().root();
    let dead_pane = workspace.test_split(shepr_core::layout::Direction::Vertical);

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        dead_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));

    let (control, _) = connect_test_shell(&mut server, 73, 185, 46);
    let _ = control.recv().expect("snapshot");
    let shrunk = server.app.test_runtime(first_pane).current_size();
    assert!(shrunk.0 < 46);

    let died = server.app.from_pane_runtime(
        dead_pane,
        shepr_mux::events::RuntimeEvent::PaneDied {
            ending: shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Exited),
            ended_at: std::time::Instant::now(),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(died));

    let runtime = &server.app.test_runtime(first_pane);
    let grown = runtime.current_size();
    assert!(grown.0 > shrunk.0);
    assert_eq!(
        runtime.read().terminal_dimensions(),
        Some(shepr_core::geometry::GridSize::clamped(grown.1, grown.0))
    );
    assert_eq!(
        runtime
            .read()
            .scroll_metrics()
            .expect("test precondition")
            .viewport_rows,
        grown.0 as usize
    );
    shutdown_test_runtimes(&mut server);
}
