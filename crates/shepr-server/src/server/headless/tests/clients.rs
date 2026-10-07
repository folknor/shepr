use super::*;

#[test]
fn disconnect_after_detach_has_no_render_impact() {
    let mut server = test_headless_server();
    let _pane_id = install_shared_view_test_runtime(&mut server);
    let (control, _render) = connect_test_shell(&mut server, 8, 112, 36);
    let _snapshot = client_shell_snapshot(&control);
    let client_id = ClientId::test_new(8);

    assert!(server.test_handle_server_event(ServerEvent::Detached { client_id }));
    assert!(!server.test_handle_server_event(ServerEvent::Disconnected { client_id }));
    assert!(!server.clients.contains_key(&client_id));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_failed_health_pong_leaves_no_ghost_client() {
    let mut server = test_headless_server();
    let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 1, 1);
    let reader = outbox.control_sender();
    let client_id = ClientId::test_new(1);
    let workspace = shepr_mux::workspace::Workspace::test_new("health");
    let workspace_id = workspace.id();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.insert_test_client(
        client_id,
        ClientConnection::new((80, 24), shepr_core::geometry::HostCell::Unknown, 1, outbox),
    );
    server.clients.set_foreground_client_id(Some(client_id));
    server
        .clients
        .set_geometry_controller(workspace_id, client_id);
    assert_eq!(reader.send(&ServerMessage::HealthPong), Delivery::Closed);
    tokio::time::timeout(Duration::from_millis(100), server.outbox_wake.notified())
        .await
        .expect("close wakes idle loop");
    assert!(server.clients.contains_key(&client_id));
    assert!(server.reap_closed_clients());
    assert!(!server.clients.contains_key(&client_id));
    assert_eq!(server.clients.foreground_client_id(), None);
    assert_eq!(server.clients.geometry_controller(&workspace_id), None);
    assert_eq!(server.clients.app_client_count(), 0);
}

#[test]
fn closing_the_foreground_client_hands_foreground_over_at_the_reap() {
    let mut server = test_headless_server();
    for id in [1, 2] {
        let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 16, 1024);
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
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(2)));
    let colors = [
        shepr_protocol::ClientHostColor {
            r: 240,
            g: 240,
            b: 240,
        },
        shepr_protocol::ClientHostColor {
            r: 10,
            g: 10,
            b: 10,
        },
    ];
    for (id, color) in [1, 2].into_iter().zip(colors) {
        server
            .clients
            .get_mut(&ClientId::test_new(id))
            .expect("client")
            .update_host_theme(&shepr_protocol::ClientHostThemeUpdate::DefaultColor {
                kind: shepr_protocol::ClientHostDefaultColorKind::Background,
                color,
            });
    }
    server.sync_host_theme_from_foreground();
    assert_eq!(
        server.app.state().host_terminal_theme().background,
        Some(colors[1])
    );
    let epoch = server.view_epoch;
    server.clients[&ClientId::test_new(2)].outbox.close();
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(2))
    );
    assert!(server.reap_closed_clients());
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(1))
    );
    assert_eq!(
        server.app.state().host_terminal_theme().background,
        Some(colors[0])
    );
    assert_ne!(server.view_epoch, epoch);
    assert!(!server.reap_closed_clients());
}

#[test]
fn a_stopping_server_reaps_closed_clients_without_reapplying_geometry() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("stopping")]);
    let area = Rect::new(0, 0, 17, 9);
    server
        .app
        .test_state_mut()
        .test_record_all_workspace_areas(crate::ui::ratatui_rect(area));
    let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 16, 1024);
    server.insert_test_client(
        1,
        ClientConnection::new((80, 24), shepr_core::geometry::HostCell::Unknown, 1, outbox),
    );
    server.lifecycle.begin_stopping();
    server.clients[&ClientId::test_new(1)].outbox.close();
    assert!(server.reap_closed_clients());
    assert_eq!(
        server
            .app
            .state()
            .ws(0)
            .spawn_geometry()
            .map(|geometry| geometry.area),
        Some(area)
    );
}

#[test]
fn a_reaped_client_marks_the_view_changed() {
    let mut server = test_headless_server();
    let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 16, 1024);
    server.insert_test_client(
        1,
        ClientConnection::new((80, 24), shepr_core::geometry::HostCell::Unknown, 1, outbox),
    );
    let epoch = server.view_epoch;
    server.clients[&ClientId::test_new(1)].outbox.close();
    assert_eq!(server.view_epoch, epoch);
    assert!(server.reap_closed_clients());
    assert_ne!(server.view_epoch, epoch);
    let settled = server.view_epoch;
    assert!(!server.reap_closed_clients());
    assert_eq!(server.view_epoch, settled);
}
