use super::*;

#[tokio::test]
async fn client_shell_input_targets_runtime_without_server_shell_classification() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1000h\x1b[?1006h");
    let pane_id = focused_test_pane(&server);
    server.insert_test_client(
        11,
        ClientConnection::with_shell(
            ClientShellState::with_surface_active(true),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_core::geometry::HostCell::Unknown,
            crate::server::clients::ActivityStamp::from(1),
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    let _writer_lanes = attach_test_writer(&mut server, 11);

    assert!(
        server.test_handle_server_event(ServerEvent::ShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id,
            events: vec![
                shepr_protocol::ClientPaneInputEvent::Key {
                    code: shepr_protocol::ClientKeyCode::Char('c'),
                    modifiers: shepr_protocol::WireModifiers::CONTROL,
                    kind: shepr_protocol::ClientKeyKind::Press,
                    shifted_codepoint: None,
                    generated_text: None,
                },
                shepr_protocol::ClientPaneInputEvent::Key {
                    code: shepr_protocol::ClientKeyCode::Char('c'),
                    modifiers: shepr_protocol::WireModifiers::CONTROL,
                    kind: shepr_protocol::ClientKeyKind::Release,
                    shifted_codepoint: None,
                    generated_text: None,
                },
                shepr_protocol::ClientPaneInputEvent::Key {
                    code: shepr_protocol::ClientKeyCode::Char('x'),
                    modifiers: shepr_protocol::WireModifiers::ALT,
                    kind: shepr_protocol::ClientKeyKind::Press,
                    shifted_codepoint: None,
                    generated_text: None,
                },
                shepr_protocol::ClientPaneInputEvent::Mouse {
                    kind: shepr_protocol::ClientMouseKind::Down(
                        shepr_protocol::ClientMouseButton::Left,
                    ),
                    position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                    modifiers: shepr_protocol::WireModifiers::NONE,
                    lines: 3,
                },
            ],
        })
    );
    assert_eq!(
        input_rx.try_recv().expect("targeted pane interrupt"),
        Bytes::from_static(&[0x03])
    );
    assert_eq!(
        input_rx.try_recv().expect("targeted pane alt key"),
        Bytes::from_static(b"\x1bx")
    );
    assert_eq!(
        input_rx.try_recv().expect("targeted pane mouse click"),
        Bytes::from_static(b"\x1b[<0;3;2M")
    );
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(11))
    );
    let pane_id = focused_test_pane(&server);

    let runtime_pane_id = server
        .app
        .state()
        .resolve_pane(&pane_id)
        .expect("runtime pane target")
        .id();
    let runtime = server
        .app
        .pane_runtime(runtime_pane_id)
        .expect("focused runtime");
    assert_eq!(runtime.current_size(), (24, 79));
    assert!(input_rx.try_recv().is_err(), "legacy release emitted bytes");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_hidden_pane_rejects_presses_but_accepts_releases() {
    let mut server = test_headless_server();
    let visible = shepr_mux::workspace::Workspace::test_new("visible-input");
    let hidden = shepr_mux::workspace::Workspace::test_new("hidden-input");
    let hidden_pane = hidden.tree().root();
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[>3u",
            4,
        );

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![visible, hidden]);
    server.app.insert_test_runtime(hidden_pane, runtime);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let pane_id = server
        .app
        .state()
        .pane(hidden_pane)
        .expect("pane exists")
        .public_id();
    server.insert_test_client(
        11,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            1,
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    let _writer_lanes = attach_test_writer(&mut server, 11);
    let key = |kind| shepr_protocol::ClientPaneInputEvent::Key {
        code: shepr_protocol::ClientKeyCode::Char('x'),
        modifiers: shepr_protocol::WireModifiers::NONE,
        kind,
        shifted_codepoint: None,
        generated_text: None,
    };

    assert!(
        !server.test_handle_server_event(ServerEvent::ShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id,
            events: vec![key(shepr_protocol::ClientKeyKind::Press)],
        })
    );
    assert!(input_rx.try_recv().is_err());
    assert!(
        !server.test_handle_server_event(ServerEvent::ShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id,
            events: vec![key(shepr_protocol::ClientKeyKind::Release)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded release").is_empty());
    assert_eq!(server.clients.foreground_client_id(), None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_text_input_renders_only_when_resetting_scrollback() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("scrolled-input");
    let pane_id = workspace.tree().root();
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            2,
            10_000,
            b"one\r\ntwo\r\nthree\r\n",
            4,
        );
    runtime.scroll_up(1);
    assert!(
        runtime
            .read()
            .scroll_metrics()
            .is_some_and(|metrics| metrics.offset_from_bottom > 0)
    );

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
    let public_pane_id = server
        .app
        .state()
        .pane(pane_id)
        .expect("pane exists")
        .public_id();
    server.insert_test_client(
        11,
        ClientConnection::with_shell(
            ClientShellState::with_surface_active(true),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_core::geometry::HostCell::Unknown,
            crate::server::clients::ActivityStamp::from(1),
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    let _writer_lanes = attach_test_writer(&mut server, 11);
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(11)));

    let render_impact = server.test_handle_server_event(ServerEvent::ShellPaneInput {
        client_id: ClientId::test_new(11),
        pane_id: public_pane_id,
        events: vec![shepr_protocol::ClientPaneInputEvent::TextCommit(
            "x".to_owned(),
        )],
    });

    assert!(render_impact);
    assert_eq!(
        input_rx.try_recv().expect("text must reach the PTY"),
        Bytes::from_static(b"x")
    );
    assert_eq!(
        server
            .app
            .pane_runtime(pane_id)
            .and_then(|runtime| runtime.read().scroll_metrics())
            .map(|metrics| metrics.offset_from_bottom),
        Some(0)
    );

    let render_impact = server.test_handle_server_event(ServerEvent::ShellPaneInput {
        client_id: ClientId::test_new(11),
        pane_id: public_pane_id,
        events: vec![shepr_protocol::ClientPaneInputEvent::TextCommit(
            "y".to_owned(),
        )],
    });
    assert!(!render_impact);
    assert_eq!(
        input_rx.try_recv().expect("second text must reach the PTY"),
        Bytes::from_static(b"y")
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_mouse_motion_delivers_without_render_when_foreground() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1003h\x1b[?1006h");
    let pane_id = focused_test_pane(&server);
    server.insert_test_client(
        11,
        ClientConnection::with_shell(
            ClientShellState::with_surface_active(true),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_core::geometry::HostCell::Unknown,
            crate::server::clients::ActivityStamp::from(1),
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    let _writer_lanes = attach_test_writer(&mut server, 11);
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(11)));
    assert!(server.claim_unowned_shell_workspace_geometry(
        ClientId::test_new(11),
        client_views::PendingResumes::Defer
    ));

    let render_impact = server.test_handle_server_event(ServerEvent::ShellPaneInput {
        client_id: ClientId::test_new(11),
        pane_id,
        events: vec![shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 0,
        }],
    });

    assert!(!render_impact);
    assert!(
        input_rx.try_recv().is_ok(),
        "motion must still reach the PTY"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_mouse_motion_promotes_and_requests_render() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1003h\x1b[?1006h");
    let pane_id = focused_test_pane(&server);
    server.insert_test_client(
        11,
        ClientConnection::with_shell(
            ClientShellState::with_surface_active(true),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_core::geometry::HostCell::Unknown,
            crate::server::clients::ActivityStamp::from(1),
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    let _writer_lanes = attach_test_writer(&mut server, 11);

    let render_impact = server.test_handle_server_event(ServerEvent::ShellPaneInput {
        client_id: ClientId::test_new(11),
        pane_id,
        events: vec![shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 0,
        }],
    });

    assert!(render_impact);
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(11))
    );
    assert!(
        input_rx.try_recv().is_ok(),
        "motion must still reach the PTY"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_input_dropped_on_a_full_pty_queue_is_reported_to_the_client() {
    let mut server = test_headless_server();
    // The focused test runtime's input queue holds four writes.
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    let pane_id = focused_test_pane(&server);
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        11,
        ClientConnection::new((80, 24), shepr_core::geometry::HostCell::Unknown, 1, writer),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(11)));

    let events = ["a", "b", "c", "d", "e", "f"]
        .into_iter()
        .map(|text| shepr_protocol::ClientPaneInputEvent::TextCommit(text.to_owned()))
        .collect();
    server.test_handle_server_event(ServerEvent::ShellPaneInput {
        client_id: ClientId::test_new(11),
        pane_id,
        events,
    });

    for expected in ["a", "b", "c", "d"] {
        assert_eq!(
            input_rx.try_recv().expect("queued input"),
            Bytes::from(expected)
        );
    }
    let message = loop {
        if let ServerMessage::ClientShellError { kind } = read_server_message(
            control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("dropped-input error"),
        ) {
            break kind.to_string();
        }
    };
    assert!(message.contains(&pane_id.to_string()), "message: {message}");
    assert!(message.contains("2 events"), "message: {message}");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn every_rejected_paste_is_reported_to_the_client_shell() {
    let mut server = test_headless_server();
    let (control_rx, _render_rx) = connect_test_shell(&mut server, 7, 80, 23);
    let notices = || {
        std::iter::from_fn(|| {
            control_rx
                .recv_timeout(std::time::Duration::from_millis(300))
                .ok()
        })
        .map(read_server_message)
        .filter_map(|message| match message {
            ServerMessage::ClientShellError { kind } => Some(kind.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
    };

    // Every rejected paste is its own user action and is reported.
    for _ in 0..2 {
        server.test_handle_server_event(ServerEvent::PasteRejected {
            client_id: ClientId::test_new(7),
            size: 2_000_000,
        });
    }
    let pastes = notices();
    assert_eq!(pastes.len(), 2);
    assert!(pastes[0].starts_with("Paste rejected"));
    assert!(
        server.clients.contains_key(&ClientId::test_new(7)),
        "notices never end the connection"
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn client_pane_pixel_mouse_uses_runtime_pixel_encoding() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let (mut runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            20,
            5,
            0,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016h",
            4,
        );
    runtime.resize(shepr_core::geometry::PaneGeometry::with_cell(
        20,
        5,
        shepr_core::geometry::CellPx::new(10, 20),
    ));

    apply_client_pane_input_events(
        &runtime,
        &[shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Pixels {
                column: 2,
                row: 1,
                report: shepr_term::mouse::PixelReport::new(
                    21,
                    22,
                    runtime.read().pixel_mouse().extent().expect("pane extent"),
                ),
            },
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 3,
        }],
    )
    .expect("pixel mouse input");
    assert_eq!(
        input_rx.try_recv().expect("encoded pixel mouse"),
        Bytes::from_static(b"\x1b[<35;21;22M")
    );
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn client_pane_pixel_mouse_stays_pixel_scaled_when_sgr_is_reasserted() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let (mut runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?1006h",
            4,
        );
    runtime.resize(shepr_core::geometry::PaneGeometry::with_cell(
        80,
        24,
        shepr_core::geometry::CellPx::new(10, 20),
    ));

    apply_client_pane_input_events(
        &runtime,
        &[shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Down(shepr_protocol::ClientMouseButton::Left),
            position: shepr_protocol::ClientMousePosition::Pixels {
                column: 40,
                row: 12,
                report: shepr_term::mouse::PixelReport::new(
                    403,
                    240,
                    runtime.read().pixel_mouse().extent().expect("pane extent"),
                ),
            },
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 1,
        }],
    )
    .expect("pixel mouse input");
    assert_eq!(
        input_rx.try_recv().expect("encoded pixel mouse"),
        Bytes::from_static(b"\x1b[<0;403;240M")
    );
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn client_pane_pixel_mouse_falls_back_to_canonical_cell_position() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let (mut runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            20,
            5,
            0,
            b"\x1b[?1003h\x1b[?1006h",
            4,
        );
    runtime.resize(shepr_core::geometry::PaneGeometry::with_cell(
        20,
        5,
        shepr_core::geometry::CellPx::new(10, 20),
    ));

    apply_client_pane_input_events(
        &runtime,
        &[shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Pixels {
                column: 2,
                row: 1,
                report: shepr_term::mouse::PixelReport::new(
                    21,
                    22,
                    runtime.read().pixel_mouse().extent().expect("pane extent"),
                ),
            },
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 3,
        }],
    )
    .expect("cell mouse fallback");
    assert_eq!(
        input_rx.try_recv().expect("encoded cell mouse"),
        Bytes::from_static(b"\x1b[<35;3;2M")
    );
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn client_pane_wheel_input_accumulates_scrollback_offset() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut bytes = Vec::new();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            20, 5, 4096, &bytes, 4,
        );
    let scroll = |kind| shepr_protocol::ClientPaneInputEvent::Mouse {
        kind,
        position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
        modifiers: shepr_protocol::WireModifiers::NONE,
        lines: 3,
    };

    apply_client_pane_input_events(
        &runtime,
        &[scroll(shepr_protocol::ClientMouseKind::ScrollUp)],
    )
    .expect("first scroll up");
    apply_client_pane_input_events(
        &runtime,
        &[scroll(shepr_protocol::ClientMouseKind::ScrollUp)],
    )
    .expect("second scroll up");
    assert_eq!(
        runtime
            .read()
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        6
    );

    apply_client_pane_input_events(
        &runtime,
        &[scroll(shepr_protocol::ClientMouseKind::ScrollDown)],
    )
    .expect("scroll down");
    assert_eq!(
        runtime
            .read()
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        3
    );

    runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h");
    apply_client_pane_input_events(
        &runtime,
        &[shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 3,
        }],
    )
    .expect("reported mouse motion");
    assert_eq!(
        runtime
            .read()
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        3
    );
    assert_eq!(
        input_rx.try_recv().expect("reported mouse motion"),
        Bytes::from_static(b"\x1b[<35;3;2M")
    );

    apply_client_pane_input_events(
        &runtime,
        &[shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Down(shepr_protocol::ClientMouseButton::Left),
            position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 3,
        }],
    )
    .expect("mouse button");
    assert_eq!(
        runtime
            .read()
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        0
    );
    assert_eq!(
        input_rx.try_recv().expect("reported mouse button"),
        Bytes::from_static(b"\x1b[<0;3;2M")
    );
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

fn with_scrollback_test_runtime(
    initial_bytes: &[u8],
    initial_scroll: usize,
    test: impl FnOnce(&shepr_mux::pane::PaneRuntime, &mut mpsc::Receiver<Bytes>),
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut bytes = initial_bytes.to_vec();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            20, 5, 4096, &bytes, 4,
        );
    if initial_scroll > 0 {
        runtime.scroll_up(initial_scroll);
    }

    test(&runtime, &mut input_rx);

    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

fn client_page_key(
    code: shepr_protocol::ClientKeyCode,
    modifiers: crossterm::event::KeyModifiers,
    kind: shepr_protocol::ClientKeyKind,
) -> shepr_protocol::ClientPaneInputEvent {
    shepr_protocol::ClientPaneInputEvent::Key {
        code,
        modifiers: shepr_protocol::WireModifiers::from_bits_retain(modifiers.bits()),
        kind,
        shifted_codepoint: None,
        generated_text: None,
    }
}

#[test]
fn client_plain_page_keys_scroll_shell_transcript_by_pane_height() {
    with_scrollback_test_runtime(b"", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                shepr_protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                shepr_protocol::ClientKeyKind::Press,
            )],
        )
        .expect("pane PageUp");
        assert_eq!(
            runtime
                .read()
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            5
        );

        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                shepr_protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                shepr_protocol::ClientKeyKind::Release,
            )],
        )
        .expect("pane PageUp release");
        assert_eq!(
            runtime
                .read()
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            5
        );

        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                shepr_protocol::ClientKeyCode::PageDown,
                crossterm::event::KeyModifiers::empty(),
                shepr_protocol::ClientKeyKind::Press,
            )],
        )
        .expect("pane PageDown");
        assert_eq!(
            runtime
                .read()
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert!(input_rx.try_recv().is_err(), "page keys reached the shell");
    });
}

#[test]
fn client_page_keys_forward_when_modified_or_owned_by_application() {
    with_scrollback_test_runtime(b"", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                shepr_protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::CONTROL,
                shepr_protocol::ClientKeyKind::Press,
            )],
        )
        .expect("modified pane PageUp");
        assert!(
            input_rx.try_recv().is_ok(),
            "modified PageUp was not forwarded"
        );
        assert_eq!(
            runtime
                .read()
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
    });

    with_scrollback_test_runtime(b"\x1b[?1h", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                shepr_protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                shepr_protocol::ClientKeyKind::Press,
            )],
        )
        .expect("application PageUp");
        assert_eq!(
            input_rx.try_recv().expect("forwarded application PageUp"),
            Bytes::from_static(b"\x1b[5~")
        );
        assert_eq!(
            runtime
                .read()
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
    });
}

#[test]
fn client_shell_streams_focused_pane_report_all_demand() {
    with_terminal_session_test_server(|server, terminal_id, _terminal_id_string, _pane_id| {
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.insert_test_client(
            1,
            ClientConnection::new(
                (80, 24),
                shepr_core::geometry::HostCell::Unknown,
                1,
                client_tx,
            ),
        );
        server.app.test_state_mut().seed_bookmark_index(Some(0));
        server
            .app
            .test_runtimes_mut()
            .get(&terminal_id)
            .expect("focused runtime")
            .test_process_pty_bytes(b"\x1b[>15u");

        server.stream_shell_keyboard_mode();

        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("shell keyboard mode message")
            ),
            ServerMessage::ClientShellKeyboardReportAll { enabled: true }
        ));
    });
}

#[tokio::test]
async fn client_shell_release_cleanup_does_not_promote_and_survives_disconnect() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[>3u");
    let pane_id = focused_test_pane(&server);
    for client_id in [1, 2] {
        server.insert_test_client(
            client_id,
            ClientConnection::new(
                (80, 24),
                shepr_core::geometry::HostCell::Unknown,
                client_id,
                crate::server::outbox::ClientOutbox::detached(),
            ),
        );
    }
    let _first_lanes = attach_test_writer(&mut server, 1);
    let _second_lanes = attach_test_writer(&mut server, 2);
    let key = |kind| shepr_protocol::ClientPaneInputEvent::Key {
        code: shepr_protocol::ClientKeyCode::Char('x'),
        modifiers: shepr_protocol::WireModifiers::NONE,
        kind,
        shifted_codepoint: None,
        // No generated text: the server only holds presses that will get a
        // release, and a key that committed text does not.
        generated_text: None,
    };

    assert!(
        server.test_handle_server_event(ServerEvent::ShellPaneInput {
            client_id: ClientId::test_new(1),
            pane_id,
            events: vec![key(shepr_protocol::ClientKeyKind::Press)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded press").is_empty());
    assert!(server.promote_client_to_foreground(ClientId::test_new(2)));

    assert!(
        !server.test_handle_server_event(ServerEvent::ShellPaneInput {
            client_id: ClientId::test_new(1),
            pane_id,
            events: vec![key(shepr_protocol::ClientKeyKind::Release)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded release").is_empty());
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(2))
    );

    // Taking the foreground back is not a view change by itself: the host
    // theme setters invalidate what a changed foreground reaches, and both
    // clients here present the same theme.
    assert!(
        !server.test_handle_server_event(ServerEvent::ShellPaneInput {
            client_id: ClientId::test_new(1),
            pane_id,
            events: vec![key(shepr_protocol::ClientKeyKind::Press)],
        })
    );
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(1))
    );
    assert!(
        !input_rx
            .recv()
            .await
            .expect("second encoded press")
            .is_empty()
    );
    assert!(server.test_handle_server_event(ServerEvent::Disconnected {
        client_id: ClientId::test_new(1)
    }));
    assert!(
        !input_rx
            .recv()
            .await
            .expect("disconnect synthesized release")
            .is_empty()
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn client_shell_mouse_capture_combines_local_preference_with_endpoint_demand() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new((80, 24), shepr_core::geometry::HostCell::Unknown, 1, writer),
    );

    server.stream_host_mouse_capture_mode();
    assert!(matches!(
        read_server_message(control_rx.recv().expect("initial mouse mode")),
        ServerMessage::MouseCapture {
            mode: shepr_term::mouse::HostMouseCapture::Off
        }
    ));
    server
        .clients
        .get_mut(&ClientId::test_new(1))
        .expect("shell client")
        .shell_state_mut()
        .mouse_capture = true;
    server.stream_host_mouse_capture_mode();
    assert!(matches!(
        read_server_message(control_rx.recv().expect("preferred mouse mode")),
        ServerMessage::MouseCapture {
            mode: shepr_term::mouse::HostMouseCapture::Cells
        }
    ));
}
