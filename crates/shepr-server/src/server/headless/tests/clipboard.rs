use super::*;

#[tokio::test]
async fn clipboard_write_goes_to_the_clients_viewing_the_writing_pane() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("clipboard-viewers");
    let second = shepr_mux::workspace::Workspace::test_new("clipboard-viewers-second");
    let second_pane = second.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![first, second]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let second_workspace_id = server.app.state().ws(1).id();

    let (first_control, _first_render) = connect_test_shell(&mut server, 7, 100, 30);
    let (second_control, _second_render) = connect_test_shell(&mut server, 8, 80, 24);
    let _ = client_shell_snapshot(&first_control);
    let _ = client_shell_snapshot(&second_control);
    assert!(server.place_test_client_on_workspace(ClientId::test_new(8), &second_workspace_id));
    // The client on the first workspace is the foreground one, so a delivery to it
    // would mean the write fell back instead of reaching the viewer.
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(7)));

    server.app.insert_idle_test_runtime(second_pane);
    let write = server.app.from_pane_runtime(
        second_pane,
        shepr_mux::events::RuntimeEvent::ClipboardWrite {
            content: b"test".to_vec(),
        },
    );
    let changed = server.handle_internal_event_with_forwarding(write);

    assert!(!changed);
    let clipboard = loop {
        match read_server_message(
            second_control
                .recv_timeout(Duration::from_millis(100))
                .expect("viewer clipboard message"),
        ) {
            ServerMessage::Clipboard { data } => break data,
            ServerMessage::EndpointSnapshot(_) => {}
            other => panic!("expected clipboard message, got {other:?}"),
        }
    };
    assert_eq!(clipboard, b"test");
    while let Ok(bytes) = first_control.recv_timeout(Duration::from_millis(50)) {
        assert!(
            !matches!(read_server_message(bytes), ServerMessage::Clipboard { .. }),
            "a client on another workspace does not receive the write"
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[test]
fn clipboard_write_from_an_unviewed_pane_targets_foreground_client_only() {
    let mut server = test_headless_server();
    let (background_tx, background_control_rx, _background_rx) = test_client_writer();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

    server.insert_test_client(
        1,
        ClientConnection::new(
            (120, 40),
            shepr_core::geometry::HostCell::Unknown,
            1,
            background_tx,
        ),
    );
    server.insert_test_client(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            2,
            foreground_tx,
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(2)));

    let write = unviewed_clipboard_write(&mut server);
    let changed = server.handle_internal_event_with_forwarding(write);

    assert!(!changed);
    match read_server_message(
        foreground_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("foreground clipboard message"),
    ) {
        ServerMessage::Clipboard { data } => assert_eq!(data, b"test"),
        other => panic!("expected clipboard message, got {other:?}"),
    }
    assert!(
        background_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "background client should not receive clipboard writes"
    );
}

#[test]
fn clipboard_write_without_foreground_client_does_not_change_visual_state() {
    let mut server = test_headless_server();
    server.clients.set_foreground_client_id(None);

    let write = unviewed_clipboard_write(&mut server);
    let changed = server.handle_internal_event_with_forwarding(write);

    assert!(!changed);
}

#[test]
fn clipboard_write_failed_foreground_send_is_removed_at_the_reap() {
    let mut server = test_headless_server();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();
    drop(foreground_control_rx);
    foreground_tx.close();

    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            1,
            foreground_tx,
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(1)));

    let write = unviewed_clipboard_write(&mut server);
    let changed = server.handle_internal_event_with_forwarding(write);

    assert!(!changed);
    assert!(
        server.clients.contains_key(&ClientId::test_new(1)),
        "closure is latched at the reap"
    );
    assert!(server.reap_closed_clients());
    assert!(!server.clients.contains_key(&ClientId::test_new(1)));
}
