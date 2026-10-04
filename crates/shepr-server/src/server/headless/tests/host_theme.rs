use super::*;

#[test]
fn client_shell_host_theme_follows_foreground_client() {
    let mut server = test_headless_server();
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
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            2,
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    let _first_lanes = attach_test_writer(&mut server, 1);
    let _second_lanes = attach_test_writer(&mut server, 2);
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(1)));

    let dark = shepr_protocol::ClientHostColor {
        r: 20,
        g: 30,
        b: 40,
    };
    let blue = shepr_protocol::ClientHostColor {
        r: 10,
        g: 20,
        b: 200,
    };
    assert!(
        server.test_handle_server_event(ServerEvent::ShellHostTheme {
            client_id: ClientId::test_new(1),
            update: shepr_protocol::ClientHostThemeUpdate::DefaultColor {
                kind: shepr_protocol::ClientHostDefaultColorKind::Background,
                color: dark,
            },
        })
    );
    assert!(
        server.test_handle_server_event(ServerEvent::ShellHostTheme {
            client_id: ClientId::test_new(1),
            update: shepr_protocol::ClientHostThemeUpdate::PaletteColors(vec![(4, blue)]),
        })
    );
    server.test_handle_server_event(ServerEvent::ShellHostTheme {
        client_id: ClientId::test_new(1),
        update: shepr_protocol::ClientHostThemeUpdate::Appearance(
            shepr_protocol::ClientHostAppearance::Dark,
        ),
    });
    assert_eq!(
        server.app.state().host_terminal_theme().background,
        Some(dark)
    );
    assert_eq!(
        server.app.state().host_terminal_theme().palette[4],
        Some(blue)
    );
    assert_eq!(
        server.app.state().host_terminal_appearance(),
        Some(shepr_term::host::HostAppearance::Dark)
    );
    assert!(server.app.state().host_terminal_appearance_explicit());

    let light = shepr_protocol::ClientHostColor {
        r: 240,
        g: 230,
        b: 220,
    };
    assert!(
        !server.test_handle_server_event(ServerEvent::ShellHostTheme {
            client_id: ClientId::test_new(2),
            update: shepr_protocol::ClientHostThemeUpdate::DefaultColor {
                kind: shepr_protocol::ClientHostDefaultColorKind::Background,
                color: light,
            },
        })
    );
    assert_eq!(
        server.app.state().host_terminal_theme().background,
        Some(dark)
    );

    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(2)));
    assert!(server.sync_host_theme_from_foreground());
    assert_eq!(
        server.app.state().host_terminal_theme().background,
        Some(light)
    );
    assert_eq!(
        server.app.state().host_terminal_appearance(),
        Some(shepr_term::host::HostAppearance::Light)
    );
    assert!(!server.app.state().host_terminal_appearance_explicit());
}

#[test]
fn resizing_a_background_shell_does_not_change_foreground_or_host_theme() {
    let mut server = test_headless_server();
    let (first_writer, first_control, _first_render) = test_client_writer();
    let (second_writer, second_control, _second_render) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            1,
            first_writer,
        ),
    );
    server.insert_test_client(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            2,
            second_writer,
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(1)));

    let first_background = shepr_protocol::ClientHostColor {
        r: 20,
        g: 30,
        b: 40,
    };
    let second_background = shepr_protocol::ClientHostColor {
        r: 10,
        g: 20,
        b: 200,
    };
    for (client_id, color) in [
        (ClientId::test_new(1), first_background),
        (ClientId::test_new(2), second_background),
    ] {
        server.test_handle_server_event(ServerEvent::ShellHostTheme {
            client_id,
            update: shepr_protocol::ClientHostThemeUpdate::DefaultColor {
                kind: shepr_protocol::ClientHostDefaultColorKind::Background,
                color,
            },
        });
    }
    assert_eq!(
        server.app.state().host_terminal_theme().background,
        Some(first_background)
    );

    assert!(server.test_handle_server_event(ServerEvent::ShellResize {
        client_id: ClientId::test_new(2),
        geometry: shepr_core::geometry::HostGeometry::new(
            shepr_core::geometry::GridSize::clamped(100, 30),
            shepr_core::geometry::HostCell::Unknown
        ),
    }));
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(1))
    );
    assert_eq!(
        server.app.state().host_terminal_theme().background,
        Some(first_background)
    );

    let write = unviewed_clipboard_write(&mut server);
    assert!(!server.handle_internal_event_with_forwarding(write));
    assert!(matches!(
        read_server_message(
            first_control
                .recv_timeout(Duration::from_millis(100))
                .expect("foreground clipboard message")
        ),
        ServerMessage::Clipboard { data } if data == b"test"
    ));
    assert!(
        second_control
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
}

/// A foreground change that moves the host theme owes a full pass to every
/// client, with no render signal involved: the theme sync itself marks the
/// view changed, so callers that discard its result (input promotion, client
/// departure) still render.
#[test]
fn a_host_theme_change_from_input_promotion_renders() {
    let mut server = test_headless_server();
    for id in [1, 2] {
        server.insert_test_client(
            id,
            ClientConnection::new(
                (80, 24),
                shepr_core::geometry::HostCell::Unknown,
                id,
                crate::server::outbox::ClientOutbox::detached(),
            ),
        );
    }
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
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(1)));
    server.sync_host_theme_from_foreground();
    let _ = server.outputs.render().take();
    let epoch = server.view_epoch;

    // The other client becomes the foreground one, as pane input does.
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(2)));
    server.sync_host_theme_from_foreground();

    assert_eq!(
        server.app.state().host_terminal_theme().background,
        Some(colors[1])
    );
    assert_ne!(server.view_epoch, epoch);
    assert!(!server.outputs.render().is_pending());
}
