use super::*;

fn window_title_test_server() -> (HeadlessServer, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("herd")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            1,
            client_tx,
        ),
    );
    server.promote_client_to_foreground(ClientId::test_new(1));
    drain_window_titles(&control_rx);
    (server, control_rx)
}

/// The test client writer drains its queue on a background thread, so
/// reading a pushed message needs a timeout rather than `try_recv`.
fn next_window_title(control_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Option<Option<String>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Ok(bytes) = control_rx.recv_timeout(remaining) else {
            return None;
        };
        if let ServerMessage::WindowTitle { title } = read_server_message(bytes) {
            return Some(title);
        }
    }
    None
}

fn drain_window_titles(control_rx: &std::sync::mpsc::Receiver<Vec<u8>>) {
    while control_rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
}

fn no_window_title(control_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> bool {
    while let Ok(bytes) = control_rx.recv_timeout(Duration::from_millis(200)) {
        if let ServerMessage::WindowTitle { .. } = read_server_message(bytes) {
            return false;
        }
    }
    true
}

#[test]
fn window_title_waits_for_a_client_to_exist() {
    let mut server = test_headless_server();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new("herd")]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    server.window_title = crate::ui::WindowTitleSettings::for_test("{workspace}");

    // The server renders before the first client attaches. Nothing was
    // delivered, so the first client is written to when it arrives.
    server.sync_window_title();

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            1,
            client_tx,
        ),
    );
    server.promote_client_to_foreground(ClientId::test_new(1));
    server.sync_window_title();

    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd".to_string()))
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn an_attaching_client_gets_the_title_even_when_it_has_not_changed() {
    let (mut server, first_control_rx) = window_title_test_server();
    server.window_title = crate::ui::WindowTitleSettings::for_test("{workspace}");
    server.sync_window_title();
    assert_eq!(
        next_window_title(&first_control_rx),
        Some(Some("herd".to_string()))
    );

    // Each client records what it was delivered, so a new client is written
    // to even though the title is the one the first client already has.
    let (client_tx, second_control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            2,
            client_tx,
        ),
    );
    server.sync_window_title();

    assert_eq!(
        next_window_title(&second_control_rx),
        Some(Some("herd".to_string()))
    );
    assert!(no_window_title(&first_control_rx));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn configured_window_title_reaches_each_client_once_per_change() {
    let (mut server, control_rx) = window_title_test_server();
    server.window_title = crate::ui::WindowTitleSettings::for_test("{workspace}");

    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd".to_string()))
    );

    // An unchanged title must not re-emit an OSC on every render.
    server.sync_window_title();
    assert!(no_window_title(&control_rx));

    server
        .app
        .test_state_mut()
        .ws_mut(0)
        .set_name("build".into());
    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("build".to_string()))
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn focused_terminal_title_syncs_and_invalidates_shell_metadata() {
    let (mut server, control_rx) = window_title_test_server();
    server.window_title = crate::ui::WindowTitleSettings::for_test("{terminal_title}");
    let pane_id = server.app.state().ws(0).tree().root();
    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes("\x1b]0;⠋ building\x07".as_bytes());
    server.app.test_runtimes_mut().insert(pane_id, runtime);

    assert_eq!(
        server.sync_terminal_title_sources(&HashSet::from([pane_id])),
        TitleSync {
            sidebar_changed: true,
            window_title_synced: true,
        }
    );
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("building".to_string()))
    );

    server
        .app
        .test_runtimes_mut()
        .get(&pane_id)
        .expect("runtime")
        .test_process_pty_bytes("\x1b]0;⠙ building\x07".as_bytes());
    assert_eq!(
        server.sync_terminal_title_sources(&HashSet::from([pane_id])),
        TitleSync {
            sidebar_changed: true,
            window_title_synced: true,
        }
    );
    assert!(no_window_title(&control_rx));

    shutdown_test_runtimes(&mut server);
}

#[test]
fn a_client_without_a_writer_does_not_cache_the_window_title() {
    let (mut server, _control_rx) = window_title_test_server();
    server.window_title = crate::ui::WindowTitleSettings::for_test("{workspace}");

    // A client whose writer has closed waits for the loop's reap; until then
    // the targeted send must report it as undelivered.
    if let Some(client) = server.clients.get_mut(&ClientId::test_new(1)) {
        client.outbox = ClientOutbox::detached();
    }
    assert!(!server.send_to_client(
        ClientId::test_new(1),
        &ServerMessage::WindowTitle {
            title: Some("probe".into()),
        }
    ));
    server.sync_window_title();
    assert!(
        server.clients[&ClientId::test_new(1)]
            .outbox
            .told_window_title()
            .is_none()
    );

    // Attaching again has to deliver the title rather than skip it as sent.
    let (client_tx, control_rx, _render_rx) = test_client_writer();
    if let Some(client) = server.clients.get_mut(&ClientId::test_new(1)) {
        client.outbox = client_tx;
    }
    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd".to_string()))
    );

    shutdown_test_runtimes(&mut server);
}

#[test]
fn empty_window_title_config_leaves_the_outer_title_alone() {
    let (mut server, control_rx) = window_title_test_server();
    server.window_title = crate::ui::WindowTitleSettings::for_test("");

    server.sync_window_title();

    assert!(no_window_title(&control_rx));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn a_newly_promoted_client_gets_the_window_title_again() {
    let (mut server, first_control_rx) = window_title_test_server();
    server.window_title = crate::ui::WindowTitleSettings::for_test("{workspace}");
    server.sync_window_title();
    assert_eq!(
        next_window_title(&first_control_rx),
        Some(Some("herd".to_string()))
    );

    // A second terminal starts on whatever its shell or ssh left behind.
    let (client_tx, second_control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            2,
            client_tx,
        ),
    );
    server.promote_client_to_foreground(ClientId::test_new(2));
    server.sync_window_title();

    assert_eq!(
        next_window_title(&second_control_rx),
        Some(Some("herd".to_string()))
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn promoted_client_window_title_uses_its_own_view() {
    let mut server = test_headless_server();
    let survivor = shepr_mux::workspace::Workspace::test_new("survivor");
    let survivor_pane = survivor.tree().focused();
    let disconnected = shepr_mux::workspace::Workspace::test_new("disconnected");
    let disconnected_pane = disconnected.tree().focused();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![survivor, disconnected]);
    server.app.test_state_mut().seed_bookmark_index(Some(1));
    let terminal = server.app.test_state_mut().terminal_mut(survivor_pane);
    terminal.set_manual_label("client-pane".into());
    terminal.set_terminal_title(Some("CLIENT OSC".into()));
    server.window_title =
        crate::ui::WindowTitleSettings::for_test("{workspace}/{pane}/{terminal_title}");

    let (survivor_control, _) = connect_matching_test_shell(&mut server, 1);
    let (disconnected_control, _) = connect_matching_test_shell(&mut server, 2);
    let survivor_workspace_id = server.app.state().ws(0).id();
    // Client 1 views the survivor while the session's bookmark stays on the
    // other workspace, so its title can only come from its own location.
    server
        .clients
        .get_mut(&ClientId::test_new(1))
        .expect("client 1")
        .shell_state_mut()
        .location
        .navigate(survivor_workspace_id, 0);
    server.promote_client_to_foreground(ClientId::test_new(2));
    drain_window_titles(&survivor_control);
    drain_window_titles(&disconnected_control);

    assert!(server.test_handle_server_event(ServerEvent::Disconnected {
        client_id: ClientId::test_new(2)
    }));
    server.sync_window_title();

    assert_eq!(
        next_window_title(&survivor_control),
        Some(Some("survivor/client-pane/CLIENT OSC".into()))
    );
    assert_eq!(server.app.state().bookmark_index(), Some(1));
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(1)),
        Some(survivor_workspace_id)
    );

    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes(b"\x1b]0;UPDATED OSC\x07");
    server
        .app
        .test_runtimes_mut()
        .insert(survivor_pane, runtime);
    assert!(
        server
            .sync_terminal_title_sources(&HashSet::from([survivor_pane]))
            .window_title_synced
    );
    assert_eq!(
        next_window_title(&survivor_control),
        Some(Some("survivor/client-pane/UPDATED OSC".into()))
    );
    assert!(
        !server
            .sync_terminal_title_sources(&HashSet::from([disconnected_pane]))
            .window_title_synced
    );
    assert!(no_window_title(&survivor_control));

    shutdown_test_runtimes(&mut server);
}
