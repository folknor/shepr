use std::fs;

use super::*;
use crate::test_support::*;
use std::time::Duration;

use crate::server::client_transport::{ClientWriter, RenderLaneReceiver};
use bytes::Bytes;
use shepr_platform::ipc::{bind_local_listener, socket_file_identity};
use shepr_protocol::MAX_FRAME_SIZE;

pub(crate) fn handle_server_event(
    server: &mut HeadlessServer,
    event: crate::server::client_transport::ServerEvent,
) -> bool {
    server.handle_server_event(event)
}

pub(crate) fn render_and_stream(server: &mut HeadlessServer) {
    server.render_and_stream();
}

pub(crate) fn outer_terminal_focus(
    server: &HeadlessServer,
    client_id: crate::server::ClientId,
) -> Option<bool> {
    server
        .clients
        .get(&client_id)
        .and_then(crate::server::clients::ClientConnection::shell_state)
        .and_then(|shell| shell.outer_terminal_focus)
}

pub(crate) fn dispatch_lifecycle_messages(
    server: &mut HeadlessServer,
    client_id: crate::server::ClientId,
    messages: Vec<shepr_protocol::ClientMessage>,
) {
    for message in messages {
        let event = match message {
            shepr_protocol::ClientMessage::ClientShellResize { geometry } => {
                crate::server::client_transport::ServerEvent::ClientShellResize {
                    client_id,
                    cell_width_px: geometry.width(),
                    cell_height_px: geometry.height(),
                    surface_cols: geometry.cols(),
                    surface_rows: geometry.rows(),
                    pixel_mouse: geometry.pixel_mouse,
                }
            }
            shepr_protocol::ClientMessage::ClientShellFocus { focused } => {
                crate::server::client_transport::ServerEvent::ClientShellFocus {
                    client_id,
                    focused,
                }
            }
            shepr_protocol::ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                crate::server::client_transport::ServerEvent::ClientShellEndpointRequest {
                    client_id,
                    boot_id,
                    request: Box::new(serde_json::from_str(&request).expect("test precondition")),
                }
            }
            other => panic!("unhandled lifecycle message: {other:?}"),
        };
        server.handle_server_event(event);
    }
}

#[cfg(test)]
#[path = "already_running.rs"]
mod already_running_tests;
#[path = "server_stop.rs"]
mod server_stop_tests;
#[cfg(test)]
#[path = "surface_delta.rs"]
mod surface_delta_tests;
#[cfg(test)]
#[path = "surface_interest.rs"]
mod surface_interest_tests;

#[tokio::test]
async fn client_listener_readiness_wakes_for_new_connection() {
    let socket_path = crate::test_support::ScratchDir::new("listener-ready").join("client.sock");
    let listener = bind_local_listener(&socket_path).expect("bind test listener");
    listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .expect("set listener nonblocking");
    let listener_fd = match &listener {
        LocalListener::UdSocket(socket) => socket.as_fd().as_raw_fd(),
    };
    let ready = tokio::io::unix::AsyncFd::new(ListenerFd(listener_fd)).expect("register listener");
    let _client = shepr_platform::ipc::connect_local_stream(&socket_path).expect("connect client");
    let readiness = tokio::time::timeout(Duration::from_millis(500), ready.readable())
        .await
        .expect("listener should become readable")
        .expect("listener readiness failed");
    drop(readiness);
    assert!(listener.accept().is_ok());
}

pub(crate) fn client_shell_snapshot(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Box<shepr_protocol::ClientShellSnapshot> {
    let ServerMessage::EndpointSnapshot(snapshot) = read_server_message(
        receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("endpoint snapshot"),
    ) else {
        panic!("expected endpoint snapshot");
    };
    snapshot
}

pub(crate) fn test_headless_server() -> HeadlessServer {
    let config = shepr_config::Config::default();
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::new(&config, crate::app::AppPolicy::Test, api_rx);

    app.state.settings.default_shell = crate::app::exiting_test_command().into();
    // The server removes its socket when dropped.
    let socket_path = crate::test_support::ScratchDir::new("headless").join("client.sock");
    let client_socket_startup_lock = shepr_platform::ipc::acquire_socket_startup_lock(&socket_path)
        .expect("test socket startup lock");
    let listener = bind_local_listener(&socket_path).expect("bind test listener");
    let client_socket_identity =
        socket_file_identity(&socket_path).expect("test listener socket identity");
    listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .expect("set listener nonblocking");
    let (server_event_tx, server_event_rx) = mpsc::channel(64);
    let stop_requested = Arc::new(shepr_api::ServerStopSignal::default());
    let effective_size = app.state.settings.headless_size;
    let mut resolved_config = Vec::new();
    shepr_protocol::codec::encode_into(
        &mut resolved_config,
        &shepr_config::ValidatedConfig::test_default(),
    )
    .expect("test config encodes");

    HeadlessServer {
        app,
        _api_server: None,
        client_listener: listener,
        client_socket_path: socket_path,
        client_socket_identity,
        clients: ClientRegistry::default(),
        client_shell_boot_id: shepr_test_fixtures::fixed_boot_id(1),
        resolved_config,
        shell_session_cache: None,
        shell_session_generation: 0,
        sent_window_title: None,
        immediate_pty_sources_dirty: true,
        host_input_modes_dirty: true,
        retained_surface_fallback_reason: None,
        retained_surface_fallbacks_reported: HashSet::new(),
        effective_size,
        lifecycle: ShutdownLifecycle::new(stop_requested),
        host_shutdown_monitor: None,
        server_event_rx,
        server_event_tx,
        shutdown_flushes: Vec::new(),
        pending_checkpointed_pane_exits: std::collections::VecDeque::new(),
        _client_socket_startup_lock: client_socket_startup_lock,
    }
}

pub(crate) fn shutdown_test_runtimes(server: &mut HeadlessServer) {
    for (_, runtime) in server.app.terminal_runtimes.drain() {
        drop(runtime);
    }
}

pub(crate) fn read_server_message(bytes: Vec<u8>) -> ServerMessage {
    let mut cursor = std::io::Cursor::new(bytes);
    shepr_protocol::read_message(&mut cursor).expect("decode server message")
}

fn frame_text(frame: &FrameData) -> String {
    frame
        .cells
        .chunks(usize::from(frame.width))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn frame_server_message_refuses_payloads_over_the_frame_cap() {
    let small = HeadlessServer::frame_server_message(&ServerMessage::ClientShellError {
        kind: shepr_protocol::NoticeKind::PaneInputDropped {
            pane_id: shepr_protocol::PublicPaneId::new(
                &crate::test_support::test_workspace_id("w1"),
                1,
            ),
            events: 1,
        },
    })
    .expect("small message frames");
    assert!(matches!(
        read_server_message(small),
        ServerMessage::ClientShellError { kind: shepr_protocol::NoticeKind::PaneInputDropped { pane_id, events: 1 } } if pane_id == "w1:p1"
    ));

    let oversized = HeadlessServer::frame_server_message(&ServerMessage::Clipboard {
        data: "x".repeat(MAX_FRAME_SIZE + 1),
    });
    assert!(matches!(
        oversized,
        Err(shepr_protocol::FramingError::Oversized { max, .. }) if max == MAX_FRAME_SIZE
    ));
}

#[test]
fn default_headless_size_is_effective_without_clients() {
    let server = test_headless_server();

    assert_eq!(
        server.app.state.settings.headless_size,
        shepr_core::geometry::GridSize::clamped(
            shepr_config::DEFAULT_HEADLESS_COLS,
            shepr_config::DEFAULT_HEADLESS_ROWS
        )
    );
    assert_eq!(
        server.effective_size,
        server.app.state.settings.headless_size
    );
}

#[tokio::test]
async fn last_shell_disconnect_restores_headless_pane_size() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    server.app.state.settings.headless_size = shepr_core::geometry::GridSize::clamped(72, 18);
    let (_control, _render) = connect_test_shell(&mut server, 7, 112, 36);
    let client_size = server.app.test_runtime(pane_id).current_size();

    assert!(server.handle_server_event(ServerEvent::ClientDisconnected {
        client_id: ClientId::test_new(7),
    }));

    let target = crate::ui::TabSurfaceTarget::from_indices(&server.app.state, 0, 0)
        .expect("test tab target");
    let layout = crate::ui::compute_tab_surface_for(
        &server.app.state,
        &server.app.terminal_runtimes,
        Some(target),
        server.app.state.settings.headless_rect(),
    );
    let pane = layout.pane_infos.first().expect("test pane geometry");
    let headless_pane_size = (pane.inner_rect.height, pane.inner_rect.width);

    assert_eq!(
        server.effective_size,
        server.app.state.settings.headless_size
    );
    assert_ne!(client_size, headless_pane_size);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        headless_pane_size
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn headless_api_reads_latest_title() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("one")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let pane_id = server.app.state.workspaces[0].tabs()[0].root_pane();
    let terminal_id = server.app.state.workspaces[0].tabs()[0].panes()[&pane_id]
        .attached_terminal_id
        .clone();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .detected_agent = Some(shepr_agent::detect::Agent::Claude);
    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes(b"\x1b]0;\xe2\xa0\x8b task\x07");
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);
    server.app.render_dirty.request_terminal_title(pane_id);

    let first = headless_pane_list(&mut server)
        .pop()
        .expect("test precondition");
    assert_eq!(first.terminal_title.as_deref(), Some("⠋ task"));
    assert_eq!(first.terminal_title_stripped.as_deref(), Some("task"));
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .expect("test precondition")
        .test_process_pty_bytes(b"\x1b]2;\xe2\xa0\x99 task\x1b\\");
    server.app.render_dirty.request_terminal_title(pane_id);
    let second = headless_pane_list(&mut server)
        .pop()
        .expect("test precondition");
    assert_eq!(second.terminal_title.as_deref(), Some("⠙ task"));
    assert_eq!(second.terminal_title_stripped.as_deref(), Some("task"));
}

fn headless_pane_list(server: &mut HeadlessServer) -> Vec<shepr_api::schema::PaneInfo> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::Request {
            id: "list-titles".into(),
            method: shepr_api::schema::Method::SessionSnapshot(
                shepr_api::schema::EmptyParams::default(),
            ),
        },
        respond_to,
    });
    let response: shepr_api::schema::SuccessResponse = serde_json::from_str(
        &crate::test_support::test_json(&response_rx.recv().expect("test precondition")),
    )
    .expect("test precondition");
    let shepr_api::schema::ResponseResult::SessionSnapshot { snapshot } = response.result else {
        panic!("expected session snapshot");
    };
    snapshot.panes
}

#[test]
fn server_stop_interrupts_server_event_backlog() {
    let mut server = test_headless_server();
    for client_id in 1..=64 {
        server
            .server_event_tx
            .try_send(ServerEvent::ClientDisconnected {
                client_id: client_id.into(),
            })
            .expect("test precondition");
    }

    server.lifecycle.stop_signal().request();

    assert!(!server.drain_server_events());
    assert!(server.server_event_rx.try_recv().is_ok());
    shutdown_test_runtimes(&mut server);
}

fn shutdown_test_request(
    id: &str,
) -> (
    shepr_api::ApiRequestMessage,
    std::sync::mpsc::Receiver<shepr_api::error::ApiResult>,
) {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    (
        shepr_api::ApiRequestMessage {
            request: shepr_api::schema::Request {
                id: id.into(),
                method: shepr_api::schema::Method::ServerStop(
                    shepr_api::schema::EmptyParams::default(),
                ),
            },
            respond_to,
        },
        response_rx,
    )
}

fn assert_server_unavailable(
    response_rx: &std::sync::mpsc::Receiver<shepr_api::error::ApiResult>,
    id: &str,
) {
    let response = response_rx
        .try_recv()
        .expect("shutdown must answer the request, not drop it");
    let error = response.expect_err("shutdown must reject the request");
    assert_eq!(
        error.code,
        shepr_api::error::ApiErrorCode::ServerUnavailable,
        "{id}"
    );
}

#[tokio::test]
async fn complete_shutdown_answers_queued_requests_and_closes_the_channel() {
    let mut server = test_headless_server();
    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    server.app.api_rx = api_rx;

    let (queued, queued_rx) = shutdown_test_request("queued");
    api_tx.send(queued).expect("test precondition");

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown completes");

    assert_server_unavailable(&queued_rx, "queued");
    // A request dispatched after cleanup fails at the sender, which the API
    // thread turns into `server_unavailable` at once.
    let (late, _late_rx) = shutdown_test_request("late");
    assert!(api_tx.send(late).is_err());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn api_request_selected_during_shutdown_is_answered() {
    let mut server = test_headless_server();
    assert!(server.lifecycle.shutdown_error().is_none());
    server.initiate_shutdown();
    assert!(server.lifecycle.shutdown_error().is_some());
    let (request, response_rx) = shutdown_test_request("selected");
    server.reject_api_request_for_shutdown(&request);
    assert_server_unavailable(&response_rx, "selected");
    shutdown_test_runtimes(&mut server);
}

#[test]
fn headless_api_request_drains_all_pending_internal_events_before_reading_state() {
    let mut server = test_headless_server();
    for _ in 0..=crate::app::APP_EVENT_DRAIN_LIMIT {
        server
            .app
            .event_tx
            .try_send(AppEvent::GitStatusRefreshed {
                results: Vec::new(),
                cache_updates: Vec::new(),
            })
            .expect("test precondition");
    }

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    // An empty git refresh has no render impact, so the returned `changed` flag is
    // not asserted; this test only covers draining past the per-batch limit.
    server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::Request {
            id: "headless_list_after_events".into(),
            method: shepr_api::schema::Method::SessionSnapshot(
                shepr_api::schema::EmptyParams::default(),
            ),
        },
        respond_to,
    });
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("test precondition");
    let response: serde_json::Value =
        serde_json::from_str(&crate::test_support::test_json(&response))
            .expect("test precondition");

    assert_eq!(response["result"]["type"], "session_snapshot");
    assert!(server.app.event_rx.try_recv().is_err());
}

fn window_title_test_server() -> (HeadlessServer, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("herd")];
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            Some(client_tx),
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
fn window_title_waits_for_a_foreground_client_to_exist() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("herd")];
    server.app.state.set_active_index(Some(0));
    server.app.configure_window_title("{workspace}");

    // The server renders before the first client attaches. Nothing was
    // delivered, so nothing may be recorded as delivered either.
    server.sync_window_title();
    assert_eq!(server.sent_window_title, None);

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            Some(client_tx),
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
    server.app.configure_window_title("{workspace}");
    server.sync_window_title();
    assert_eq!(
        next_window_title(&first_control_rx),
        Some(Some("herd".to_string()))
    );

    // ClientShellConnected assigns the foreground client directly rather than
    // going through promote_client_to_foreground, so the cache must notice
    // the new client on its own.
    let (client_tx, second_control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            2,
            Some(client_tx),
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(2)));
    server.sync_window_title();

    assert_eq!(
        next_window_title(&second_control_rx),
        Some(Some("herd".to_string()))
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn configured_window_title_reaches_the_foreground_client_once_per_change() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}/{tab}");

    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd/1".to_string()))
    );

    // An unchanged title must not re-emit an OSC on every render.
    server.sync_window_title();
    assert!(no_window_title(&control_rx));

    server.app.state.workspaces[0].set_tab_custom_name(0, Some("build".into()));
    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd/build".to_string()))
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn focused_terminal_title_syncs_and_invalidates_shell_metadata() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("{terminal_title}");
    server.app.state.ensure_test_terminals();
    let pane_id = server.app.state.workspaces[0].tabs()[0].root_pane();
    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(pane_id)
        .expect("terminal")
        .clone();
    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes("\x1b]0;⠋ building\x07".as_bytes());
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);

    assert_eq!(
        server.sync_terminal_title_sources(&HashSet::from([pane_id])),
        (true, true)
    );
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("building".to_string()))
    );

    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .expect("runtime")
        .test_process_pty_bytes("\x1b]0;⠙ building\x07".as_bytes());
    assert_eq!(
        server.sync_terminal_title_sources(&HashSet::from([pane_id])),
        (true, true)
    );
    assert!(no_window_title(&control_rx));

    shutdown_test_runtimes(&mut server);
}

#[test]
fn a_foreground_client_without_a_writer_does_not_cache_the_window_title() {
    let (mut server, _control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}");

    // Production never keeps a writer-less client (a detach removes it), but
    // the targeted send must still report a writer-less entry as undelivered.
    if let Some(client) = server.clients.get_mut(&1) {
        client.writer = None;
    }
    assert!(!server.send_to_client(
        ClientId::test_new(1),
        &ServerMessage::WindowTitle {
            title: Some("probe".into()),
        }
    ));
    server.sync_window_title();
    assert!(server.sent_window_title.is_none());

    // Attaching again has to deliver the title rather than skip it as sent.
    let (client_tx, control_rx, _render_rx) = test_client_writer();
    if let Some(client) = server.clients.get_mut(&1) {
        client.writer = Some(client_tx);
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
    server.app.configure_window_title("");

    server.sync_window_title();

    assert!(no_window_title(&control_rx));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn a_newly_promoted_client_gets_the_window_title_again() {
    let (mut server, first_control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}");
    server.sync_window_title();
    assert_eq!(
        next_window_title(&first_control_rx),
        Some(Some("herd".to_string()))
    );

    // A second terminal starts on whatever its shell or ssh left behind.
    let (client_tx, second_control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            2,
            Some(client_tx),
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
    let mut survivor = shepr_mux::workspace::Workspace::test_new("survivor");
    let survivor_tab_index = survivor.test_add_tab(Some("survivor-tab"));
    let survivor_pane = survivor.tabs()[survivor_tab_index].layout().focused();
    let disconnected = shepr_mux::workspace::Workspace::test_new("disconnected");
    let disconnected_pane = disconnected.tabs()[0].layout().focused();
    server.app.state.workspaces = vec![survivor, disconnected];
    server.app.state.set_active_index(Some(1));
    server.app.state.set_selected_index(Some(1));
    server.app.state.ensure_test_terminals();
    let survivor_terminal = server.app.state.workspaces[0].tabs()[survivor_tab_index]
        .terminal_id(survivor_pane)
        .expect("survivor terminal")
        .clone();
    let terminal = server
        .app
        .state
        .terminals
        .get_mut(&survivor_terminal)
        .expect("survivor terminal state");
    terminal.manual_label = Some("client-pane".into());
    terminal.set_terminal_title(Some("CLIENT OSC".into()));
    server
        .app
        .configure_window_title("{workspace}/{tab}/{pane}/{terminal_title}");

    let (survivor_control, _) = connect_matching_test_shell(&mut server, 1);
    let (disconnected_control, _) = connect_matching_test_shell(&mut server, 2);
    let survivor_tab_id = server
        .app
        .public_tab_id(0, survivor_tab_index)
        .expect("survivor tab id");
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(1), &survivor_tab_id));
    server.promote_client_to_foreground(ClientId::test_new(2));
    drain_window_titles(&survivor_control);
    drain_window_titles(&disconnected_control);

    assert!(server.handle_server_event(ServerEvent::ClientDisconnected {
        client_id: ClientId::test_new(2)
    }));
    server.sync_window_title();

    assert_eq!(
        next_window_title(&survivor_control),
        Some(Some("survivor/survivor-tab/client-pane/CLIENT OSC".into()))
    );
    assert_eq!(server.app.state.active_index(), Some(1));
    assert_eq!(
        server
            .shell_tab_id_for_client(ClientId::test_new(1))
            .as_deref(),
        Some(survivor_tab_id.as_str())
    );

    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes(b"\x1b]0;UPDATED OSC\x07");
    server
        .app
        .terminal_runtimes
        .insert(survivor_terminal, runtime);
    assert!(
        server
            .sync_terminal_title_sources(&HashSet::from([survivor_pane]))
            .1
    );
    assert_eq!(
        next_window_title(&survivor_control),
        Some(Some("survivor/survivor-tab/client-pane/UPDATED OSC".into()))
    );
    assert!(
        !server
            .sync_terminal_title_sources(&HashSet::from([disconnected_pane]))
            .1
    );
    assert!(no_window_title(&survivor_control));

    shutdown_test_runtimes(&mut server);
}

pub(crate) fn test_client_writer() -> (
    ClientWriter,
    std::sync::mpsc::Receiver<Vec<u8>>,
    RenderLaneReceiver,
) {
    ClientWriter::test_pair()
}

#[tokio::test]
async fn client_shell_attach_seeds_workspace() {
    let mut server = test_headless_server();
    server.app.state.workspaces.clear();
    server.app.state.set_active_index(None);
    server.app.state.mode = crate::app::Mode::Navigate;
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: ClientId::test_new(6),
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );

    assert_eq!(server.app.state.mode, crate::app::Mode::Terminal);
    assert_eq!(server.app.state.workspaces.len(), 1);
    assert_eq!(server.app.state.active_index(), Some(0));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_snapshot_presents_unknown_agent_as_idle() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("endpoint");
    let pane_id = workspace.tabs()[0].root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    let terminal_id = server
        .app
        .state
        .terminal_id_for_pane(0, pane_id)
        .expect("terminal");
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("terminal")
        .set_detected_state(
            Some(shepr_agent::detect::Agent::Pi),
            shepr_agent::detect::AgentState::Unknown,
        );

    let (writer, control_rx, _render_rx) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        client_id: ClientId::test_new(78),
        surface_cols: 80,
        surface_rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        mouse_capture: false,
        surface_active: false,
        writer,
    });

    let snapshot = client_shell_snapshot(&control_rx);
    assert_eq!(
        snapshot.agents[0].agent_status,
        shepr_api::schema::AgentStatus::Idle
    );
    assert_eq!(
        snapshot.tabs[0].agent_status,
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
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("endpoint")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(41);
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let _initial_snapshot = client_shell_snapshot(&control_rx);
    let boot_id = server.client_shell_boot_id.clone();
    let rename = || {
        shepr_api::schema::Method::WorkspaceRename(shepr_api::schema::WorkspaceRenameParams {
            workspace_id: server.app.state.workspaces[0].id.to_string(),
            label: "renamed".into(),
        })
    };
    let first_rename = rename();
    let busy_rename = rename();

    // A rename is a UI mutation, so the accepted request reports a render.
    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(shepr_api::schema::Request {
                id: "client-shell:1".into(),
                method: first_rename,
            }),
        })
    );
    assert!(
        server.clients[&client_id]
            .shell_state()
            .is_some_and(|shell| shell.endpoint_command_in_flight)
    );

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(shepr_api::schema::Request {
                id: "client-shell:busy".into(),
                method: busy_rename,
            }),
        })
    );
    assert!(server.clients.contains_key(&client_id));
    let ServerMessage::ClientShellEndpointResponseChunk { data, .. } =
        read_server_message(control_rx.recv().expect("busy endpoint response"))
    else {
        panic!("expected busy endpoint response");
    };
    let response = serde_json::from_slice::<shepr_api::schema::ErrorResponse>(&data)
        .expect("typed busy response");
    assert_eq!(response.error.code, "endpoint_busy");

    let response_ready = server
        .server_event_rx
        .recv()
        .await
        .expect("endpoint response ready");
    assert!(!server.handle_server_event(response_ready));
    assert!(
        !server.clients[&client_id]
            .shell_state()
            .is_some_and(|shell| shell.endpoint_command_in_flight)
    );

    match read_server_message(control_rx.recv().expect("endpoint response")) {
        ServerMessage::ClientShellEndpointResponseChunk {
            boot_id: response_boot_id,
            request_id,
            final_chunk,
            data,
        } => {
            assert_eq!(response_boot_id, boot_id);
            assert_eq!(request_id, "client-shell:1");
            assert!(final_chunk);
            let response = serde_json::from_slice::<shepr_api::schema::SuccessResponse>(&data)
                .expect("success response");
            assert_eq!(response.id, "client-shell:1");
            assert!(matches!(
                response.result,
                shepr_api::schema::ResponseResult::WorkspaceInfo { .. }
            ));
        }
        other => panic!("expected client shell endpoint response, got {other:?}"),
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_receives_metadata_then_shell_free_pane_surface() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("shell-only-label");
    let pane_id = workspace.focused_pane_id();

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
            80,
            23,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016hCLIENT_SHELL_LIVE",
        ),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: ClientId::test_new(7),
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 10,
            cell_height_px: 20,
            pixel_mouse: true,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let snapshot = client_shell_snapshot(&control_rx);
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].label, "shell-only-label");
    server.render_and_stream();
    let initial_surface = match read_server_message(render_rx.recv().expect("pane surface")) {
        ServerMessage::PaneSurface(surface) => {
            assert_eq!((surface.frame.width, surface.frame.height), (80, 23));
            let text = frame_text(&surface.frame);
            assert!(text.contains("CLIENT_SHELL_LIVE"), "surface: {text:?}");
            assert!(!text.contains("shell-only-label"), "surface: {text:?}");
            assert_eq!(surface.panes.len(), 1);
            assert_eq!(surface.panes[0].rect.x, 0);
            assert_eq!(surface.panes[0].rect.y, 0);
            assert!(surface.panes[0].sgr_pixel_mouse);
            assert_eq!(
                surface.panes[0].pixel_width,
                u32::from(surface.panes[0].inner_rect.width) * 10
            );
            assert_eq!(
                surface.panes[0].pixel_height,
                u32::from(surface.panes[0].inner_rect.height) * 20
            );
            surface
        }
        other => panic!("expected pane surface, got {other:?}"),
    };

    let baseline = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("initial baseline");
    let cells_ptr = baseline.frame.cells.as_ptr();
    let untouched_symbol_ptr = baseline
        .frame
        .cells
        .last()
        .expect("test precondition")
        .symbol
        .as_ptr();

    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(b"\rPATCHED");
    let sources = std::collections::HashSet::from([pane_id]);
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    match read_server_message(render_rx.recv().expect("pane surface patch")) {
        ServerMessage::SurfaceUpdate(patch) => {
            assert_eq!(
                patch.base_surface_revision,
                initial_surface.surface_revision
            );
            assert_eq!(
                Some(patch.surface_revision),
                initial_surface.surface_revision.checked_next()
            );
            assert_eq!(patch.meta.as_ref().expect("metadata").panes.len(), 1);
            assert!(!patch.spans.is_empty());
            assert!(
                patch
                    .spans
                    .iter()
                    .flat_map(|row| &row.cells)
                    .any(|cell| cell.symbol == "P")
            );
        }
        other => panic!("expected pane surface patch, got {other:?}"),
    }
    let patched = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("test precondition");
    assert_eq!(
        (
            patched.frame.cells.as_ptr(),
            patched
                .frame
                .cells
                .last()
                .expect("test precondition")
                .symbol
                .as_ptr()
        ),
        (cells_ptr, untouched_symbol_ptr),
        "a text patch must preserve the frame and unchanged cell storage"
    );
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(b"\x1b[?1003l\x1b[?1006l\x1b[?1016l");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    match read_server_message(render_rx.recv().expect("metadata-only pane surface patch")) {
        ServerMessage::SurfaceUpdate(patch) => {
            assert!(patch.spans.is_empty(), "mouse modes only change metadata");
            let panes = &patch.meta.as_ref().expect("metadata").panes;
            assert_eq!(panes.len(), 1);
            assert!(!panes[0].mouse_reporting);
            assert!(!panes[0].sgr_pixel_mouse);
        }
        other => panic!("expected metadata-only pane surface patch, got {other:?}"),
    }
    let retained = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("committed retained surface");
    assert_eq!(retained.frame.cells.as_ptr(), cells_ptr);
    assert_eq!(
        retained
            .frame
            .cells
            .last()
            .expect("test precondition")
            .symbol
            .as_ptr(),
        untouched_symbol_ptr,
        "retained updates must not copy unchanged screen cells"
    );
    let retained = retained.clone();
    server
        .clients
        .get_mut(&7)
        .expect("test precondition")
        .render_state
        .request_repaint();
    server.render_and_stream();
    let full = match read_server_message(render_rx.recv().expect("full comparison surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected full comparison surface, got {other:?}"),
    };
    assert!(full.surface_revision > retained.surface_revision);
    assert_eq!(retained.frame, full.frame);
    assert_eq!(retained.panes, full.panes);
    assert_eq!(retained.splits, full.splits);
    shutdown_test_runtimes(&mut server);
}

fn install_shared_view_test_runtime(server: &mut HeadlessServer) -> shepr_core::layout::PaneId {
    let workspace = shepr_mux::workspace::Workspace::test_new("shared-view");
    let pane_id = workspace.focused_pane_id();

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"BASE"),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    pane_id
}

fn connect_test_shell(
    server: &mut HeadlessServer,
    client_id: u64,
    surface_cols: u16,
    surface_rows: u16,
) -> (std::sync::mpsc::Receiver<Vec<u8>>, RenderLaneReceiver) {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: client_id.into(),
            surface_cols,
            surface_rows,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    (control, render)
}

fn connect_matching_test_shell(
    server: &mut HeadlessServer,
    client_id: u64,
) -> (std::sync::mpsc::Receiver<Vec<u8>>, RenderLaneReceiver) {
    connect_test_shell(server, client_id, 80, 23)
}

fn write_shared_test_pane(
    server: &mut HeadlessServer,
    pane_id: shepr_core::layout::PaneId,
    bytes: &[u8],
) {
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(bytes);
}

#[tokio::test]
async fn unchanged_shell_render_reuses_session_and_sends_no_snapshot() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    server.app.state.ensure_test_terminals();
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let initial = client_shell_snapshot(&control);
    assert!(
        !initial.resolved_config.is_empty(),
        "the first snapshot of a connection carries the config"
    );
    server.render_and_stream();
    assert!(control.try_recv().is_err());
    let built_at = server
        .shell_session_cache
        .as_ref()
        .expect("session cache")
        .built_at;
    let generation = server.shell_session_generation;
    let unchanged = |server: &HeadlessServer| {
        // `built_at` only moves when the session (and its /proc probes) is
        // rebuilt; an unmoved generation means no client was projected again.
        assert_eq!(
            server
                .shell_session_cache
                .as_ref()
                .map(|cache| cache.built_at),
            Some(built_at)
        );
        assert_eq!(server.shell_session_generation, generation);
        assert_eq!(
            server.clients[&7]
                .shell_state()
                .map(|shell| shell.session_generation),
            Some(generation)
        );
    };

    server.render_and_stream();
    unchanged(&server);
    assert!(control.try_recv().is_err());

    let pane_id = server
        .app
        .public_pane_id(0, server.app.state.workspaces[0].tabs()[0].root_pane())
        .expect("pane id");
    assert!(api_through_server(
        &mut server,
        shepr_api::schema::Method::PaneScroll(shepr_api::schema::PaneScrollParams {
            pane_id: pane_id.to_string(),
            offset_from_bottom: 0,
        }),
    ));
    server.render_and_stream();
    unchanged(&server);
    assert!(control.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

/// Sends one API request the way the headless loop does and returns whether
/// it asked for a render.
fn api_through_server(server: &mut HeadlessServer, method: shepr_api::schema::Method) -> bool {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let render = server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::Request {
            id: "projection".into(),
            method,
        },
        respond_to,
    });
    let response = response_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("api response");
    assert!(response.is_ok(), "{response:?}");
    render
}

/// Renders and returns the one replacement the change must produce.
fn next_projection(
    server: &mut HeadlessServer,
    control: &std::sync::mpsc::Receiver<Vec<u8>>,
    previous: &mut shepr_protocol::ProjectionRevision,
) -> Box<shepr_protocol::ClientShellSnapshot> {
    server.render_and_stream();
    let snapshot = client_shell_snapshot(control);
    assert!(snapshot.revision > *previous);
    assert!(
        snapshot.resolved_config.is_empty(),
        "later snapshots reuse the connection's config"
    );
    *previous = snapshot.revision;
    snapshot
}

#[tokio::test]
async fn workspace_rename_reprojects_without_copying_connection_config() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let first = client_shell_snapshot(&control);
    server.render_and_stream();
    assert!(control.try_recv().is_err());

    let outcome = server
        .app
        .handle_api_request_with_render(shepr_api::schema::Request {
            id: "rename".into(),
            method: shepr_api::schema::Method::WorkspaceRename(
                shepr_api::schema::WorkspaceRenameParams {
                    workspace_id: first.workspaces[0].workspace_id.to_string(),
                    label: "renamed".into(),
                },
            ),
        });
    assert_eq!(outcome.render, RenderDemand::Full);
    server.render_and_stream();
    let renamed = client_shell_snapshot(&control);
    assert_eq!(renamed.workspaces[0].label, "renamed");
    assert!(renamed.revision > first.revision);
    assert!(renamed.resolved_config.is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn cwd_report_and_slow_probe_refresh_shell_projection() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    server.app.state.ensure_test_terminals();
    let pane_id = server.app.state.workspaces[0].tabs()[0].root_pane();
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let _ = client_shell_snapshot(&control);
    server.render_and_stream();
    assert!(control.try_recv().is_err());

    let cwd = server
        .client_socket_path
        .parent()
        .expect("socket directory")
        .to_path_buf();
    server
        .app
        .handle_internal_event(shepr_mux::events::AppEvent::TerminalCwdReported {
            pane_id,
            cwd: shepr_mux::UsableCwd::new(cwd.clone()).expect("socket directory is usable"),
        });
    server.render_and_stream();
    let reported = client_shell_snapshot(&control);
    assert_eq!(
        reported.panes[0].cwd.as_deref(),
        Some(cwd.to_str().expect("cwd utf8"))
    );
    assert!(reported.resolved_config.is_empty());

    let age_cache = |server: &mut HeadlessServer| {
        if let Some(cache) = server.shell_session_cache.as_mut() {
            cache.built_at -= super::render::SHELL_CWD_REFRESH_INTERVAL * 2;
        }
    };

    // Nothing changed: the timer re-reads the sources but neither moves the
    // generation nor asks for a render, and the next check is a full interval out.
    age_cache(&mut server);
    assert!(server.shell_cwd_refresh_due(Instant::now()));
    let generation = server.shell_session_generation;
    assert!(!server.refresh_shell_projection_sources());
    assert_eq!(server.shell_session_generation, generation);
    assert!(!server.shell_cwd_refresh_due(Instant::now()));
    server.render_and_stream();
    assert!(control.try_recv().is_err());

    // A change no event reports (standing in for a shell's /proc cwd) is
    // found by the timer and reaches the client with the next render.
    server.app.state.workspaces[0].custom_name = Some("silent".into());
    server.render_and_stream();
    assert!(control.try_recv().is_err(), "no event reported the change");
    age_cache(&mut server);
    assert!(server.refresh_shell_projection_sources());
    server.render_and_stream();
    assert_eq!(
        client_shell_snapshot(&control).workspaces[0].label,
        "silent"
    );

    // The timer only runs while a shell client is connected.
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected {
        client_id: ClientId::test_new(7),
    }));
    age_cache(&mut server);
    assert_eq!(server.shell_cwd_refresh_deadline(), None);
    shutdown_test_runtimes(&mut server);
}

/// Each change goes through the path production uses (API request, internal
/// event, title sync, metadata expiry) with no manual invalidation, and each
/// must reach the client as its own fresh projection.
#[tokio::test]
async fn each_kind_of_change_sends_a_new_projection_through_its_real_path() {
    use shepr_api::schema::Method;

    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let pane_id = server.app.state.workspaces[0].tabs()[0].root_pane();
    // A second pane so zoom has something to hide.
    server.app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    server.app.state.ensure_test_terminals();
    let public_pane_id = server.app.public_pane_id(0, pane_id).expect("pane id");
    let pane = |snapshot: &shepr_protocol::ClientShellSnapshot| {
        snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id.as_str() == public_pane_id.as_str())
            .cloned()
            .expect("projected pane")
    };
    let tab_id = server.app.public_tab_id(0, 0).expect("tab id");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let mut previous = client_shell_snapshot(&control).revision;
    server.render_and_stream();
    assert!(control.try_recv().is_err());

    assert!(api_through_server(
        &mut server,
        Method::PaneRename(shepr_api::schema::PaneRenameParams {
            pane_id: public_pane_id.clone().to_string(),
            label: Some("manual".into()),
        }),
    ));
    let renamed = next_projection(&mut server, &control, &mut previous);
    assert_eq!(pane(&renamed).label.as_deref(), Some("manual"));

    assert!(api_through_server(
        &mut server,
        Method::TabRename(shepr_api::schema::TabRenameParams {
            tab_id: tab_id.clone().to_string(),
            label: "named-tab".into(),
        }),
    ));
    let tab = next_projection(&mut server, &control, &mut previous);
    assert_eq!(tab.tabs[0].label, "named-tab");
    assert!(tab.tabs[0].custom_label);

    assert!(api_through_server(
        &mut server,
        Method::PaneInputSet(shepr_api::schema::PaneInputSetParams {
            pane_id: public_pane_id.clone().to_string(),
            right_click: shepr_api::schema::PaneRightClickTarget::Pane,
        }),
    ));
    assert!(pane(&next_projection(&mut server, &control, &mut previous)).right_click_passthrough);

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
            pane_id,
            agent: Some(shepr_agent::detect::Agent::Pi),
            state: shepr_agent::detect::AgentState::Working,
            visible_blocker: false,
            process_exited: false,
            observed_at: Instant::now(),
        })
    );
    let agent = next_projection(&mut server, &control, &mut previous);
    assert!(agent.agents.iter().any(|entry| entry.state_change_seq > 0));

    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(b"\x1b]0;compiling\x07");
    let (title_changed, _) = server.sync_terminal_title_sources(&HashSet::from([pane_id]));
    assert!(title_changed);
    let titled = next_projection(&mut server, &control, &mut previous);
    assert_eq!(
        titled.agents[0].terminal_title_stripped.as_deref(),
        Some("compiling")
    );

    let workspace_state_id = server.app.state.workspaces[0].id.to_string();
    let cwd = server.app.state.workspaces[0].identity_cwd.clone();
    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
            results: vec![shepr_mux::git::WorkspaceGitStatus {
                workspace_id: workspace_state_id,
                resolved_identity_cwd: cwd.clone(),
                status_cache_key: cwd,
                demand: shepr_mux::git::GitStatusRefreshDemand::ALL,
                auto_label: "focus-reporting".into(),
                branch: Some("feature".into()),
                ahead_behind: None,
                space: None,
            }],
            cache_updates: Vec::new(),
        })
    );
    assert_eq!(
        next_projection(&mut server, &control, &mut previous).workspaces[0]
            .branch
            .as_deref(),
        Some("feature")
    );

    assert!(api_through_server(
        &mut server,
        Method::PaneZoom(shepr_api::schema::PaneZoomParams {
            pane_id: Some(public_pane_id.clone().to_string()),
            mode: shepr_api::schema::PaneZoomMode::On,
        }),
    ));
    assert!(next_projection(&mut server, &control, &mut previous).tabs[0].zoomed);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_reconnecting_shell_gets_the_config_again_and_later_changes() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    assert!(!client_shell_snapshot(&control).resolved_config.is_empty());
    server.render_and_stream();
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected {
        client_id: ClientId::test_new(7),
    }));

    // The shared cache outlives the connection; the new one is seeded fresh
    // and still receives config bytes and subsequent changes.
    let (control, _render) = connect_matching_test_shell(&mut server, 8);
    let seed = client_shell_snapshot(&control);
    assert!(!seed.resolved_config.is_empty());
    let mut previous = seed.revision;
    server.render_and_stream();
    assert!(control.try_recv().is_err());
    assert!(api_through_server(
        &mut server,
        shepr_api::schema::Method::WorkspaceRename(shepr_api::schema::WorkspaceRenameParams {
            workspace_id: seed.workspaces[0].workspace_id.to_string(),
            label: "after-reconnect".into(),
        }),
    ));
    assert_eq!(
        next_projection(&mut server, &control, &mut previous).workspaces[0].label,
        "after-reconnect"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn ensure_default_workspace_invalidates_the_shell_projection() {
    let mut server = test_headless_server();
    let revision = server.app.state.shell_projection_revision;
    assert!(server.app.state.workspaces.is_empty());
    assert!(server.app.ensure_default_workspace());
    assert_ne!(server.app.state.shell_projection_revision, revision);
    shutdown_test_runtimes(&mut server);
}

/// Pairs a render receiver with the decoder that unwraps its surface reuse
/// and delta messages: the server encodes those against the last full
/// surface it sent on that connection, so decoding them here needs the same
/// running baseline a real endpoint client would keep.
struct PaneSurfaceReceiver {
    receiver: RenderLaneReceiver,
    decoder: shepr_protocol::surface_reuse::Decoder,
}

impl PaneSurfaceReceiver {
    fn new(receiver: RenderLaneReceiver) -> Self {
        Self {
            receiver,
            decoder: shepr_protocol::surface_reuse::Decoder::default(),
        }
    }

    fn try_recv(&self) -> Result<Vec<u8>, std::sync::mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    fn recv(&mut self, context: &str) -> ServerMessage {
        let message = read_server_message(
            self.receiver
                .recv()
                .unwrap_or_else(|error| panic!("{context}: {error}")),
        );
        self.decoder.decode(message.clone()).unwrap_or(message)
    }
}

fn recv_pane_surface(
    receiver: &mut PaneSurfaceReceiver,
    context: &str,
) -> shepr_protocol::PaneSurfaceFrame {
    match receiver.recv(context) {
        ServerMessage::PaneSurface(surface) => surface,
        ServerMessage::PaneSurfacePatch(_) => receiver.decoder.current_surface().expect("baseline"),
        other => panic!("{context}: expected pane surface, got {other:?}"),
    }
}

fn recv_pane_surface_patch(
    receiver: &mut PaneSurfaceReceiver,
    context: &str,
) -> shepr_protocol::SurfaceUpdate {
    let message = read_server_message(
        receiver
            .receiver
            .recv()
            .unwrap_or_else(|error| panic!("{context}: {error}")),
    );
    receiver
        .decoder
        .decode(message.clone())
        .expect("valid surface update");
    match message {
        ServerMessage::SurfaceUpdate(patch) => patch,
        other => panic!("{context}: expected pane surface patch, got {other:?}"),
    }
}

#[tokio::test]
async fn unrelated_render_keeps_synchronized_pane_frame_committed() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (_control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    server.render_and_stream();
    let before = recv_pane_surface(&mut render, "baseline");
    assert!(frame_text(&before.frame).contains("BASE"));
    let projection_before = server.clients[&7]
        .shell_state()
        .map_or(0, |shell| shell.projection_revision.get());

    write_shared_test_pane(
        &mut server,
        pane_id,
        b"\x1b[?2026h\x1b[?1049h\x1b[2J\x1b[HPARTIAL",
    );
    server.app.state.workspaces[0].custom_name = Some("renamed during frame".into());
    server.app.state.mark_shell_projection_dirty();
    server
        .clients
        .get_mut(&7)
        .expect("test precondition")
        .request_recompute();
    server.render_and_stream();
    assert!(render.try_recv().is_err(), "partial frame was published");
    assert_eq!(
        server.clients[&7]
            .shell_state()
            .map_or(0, |shell| shell.projection_revision.get()),
        projection_before
    );

    write_shared_test_pane(&mut server, pane_id, b"\rCOMPLETE\x1b[?2026l");
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    server.render_and_stream();
    let after = recv_pane_surface(&mut render, "completed frame");
    assert!(frame_text(&after.frame).contains("COMPLETE"));
    assert!(after.projection_revision > projection_before);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn sibling_retained_output_waits_for_synchronized_pane_to_finish() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("synchronized-split");
    let first = workspace.tabs()[0].root_pane();
    let second = workspace.test_split(shepr_core::layout::Direction::Vertical);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let (_control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    server.render_and_stream();
    let _ = recv_pane_surface(&mut render, "split baseline");

    write_shared_test_pane(&mut server, first, b"\x1b[?2026h\rPARTIAL");
    write_shared_test_pane(&mut server, second, b"\rUPDATED");
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([second])));
    server.render_and_stream();
    assert!(
        render.try_recv().is_err(),
        "sibling published partial frame"
    );

    write_shared_test_pane(&mut server, first, b"\rCOMPLETE\x1b[?2026l");
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([first])));
    server.render_and_stream();
    let after = recv_pane_surface(&mut render, "completed split");
    let text = frame_text(&after.frame);
    assert!(
        text.contains("COMPLETE") && text.contains("UPDATED"),
        "{text}"
    );
    assert!(!text.contains("PARTIAL"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn zoom_hidden_synchronized_pane_does_not_block_surface() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("zoomed-sync");
    let hidden = workspace.tabs()[0].root_pane();
    let visible = workspace.test_split(shepr_core::layout::Direction::Vertical);
    workspace.set_tab_zoomed(0, true);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        hidden,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"HIDDEN"),
    );
    server.app.insert_test_runtime(
        visible,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"VISIBLE"),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let (_control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    write_shared_test_pane(&mut server, hidden, b"\x1b[?2026h\rPARTIAL");
    server.render_and_stream();
    let surface = recv_pane_surface(&mut render, "zoomed visible pane");
    assert!(frame_text(&surface.frame).contains("VISIBLE"));
    assert!(!frame_text(&surface.frame).contains("PARTIAL"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn retained_snapshot_survives_a_writer_waiting_for_the_terminal_core() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    let _ = control.recv().expect("snapshot");
    server.render_and_stream();
    let _ = recv_pane_surface(&mut render, "initial surface");

    let (release, writer, revision) = {
        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rAAAA\x1b[?1003h");
        let revision = runtime.content_seq();
        let (release, writer) =
            runtime.test_contend_during_dirty_collection(b"\rBBBB\x1b[?1003l".to_vec());
        (release, writer, revision)
    };
    let retained = server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id]));
    release.send(()).expect("release waiting writer");
    let announced = writer.join().expect("writer completed");

    assert!(
        retained,
        "a waiting writer must not invalidate the collected snapshot"
    );
    assert!(
        !announced,
        "writer must wait before announcing a new revision"
    );
    let patch = recv_pane_surface_patch(&mut render, "snapshot before waiting write");
    assert_eq!(
        patch.meta.as_ref().expect("metadata").panes[0].content_revision,
        revision
    );
    assert!(revision.is_multiple_of(2));
    assert!(patch.meta.as_ref().expect("metadata").panes[0].mouse_reporting);
    let surface = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("surface");
    assert!(frame_text(&surface.frame).contains("AAAA"));
    assert!(!frame_text(&surface.frame).contains("BBBB"));

    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    let next = recv_pane_surface_patch(&mut render, "waiting write remains dirty");
    assert_eq!(
        next.meta.as_ref().expect("metadata").panes[0].content_revision,
        revision + 2
    );
    assert!(!next.meta.as_ref().expect("metadata").panes[0].mouse_reporting);
    let surface = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("next surface");
    assert!(frame_text(&surface.frame).contains("BBBB"));
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
    server.render_and_stream();

    let surface = recv_pane_surface(&mut render, "first alternate-screen surface");
    assert!(surface.panes[0].alternate_screen_active);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        (initial_size.0, initial_size.1 + 1)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn different_size_shells_receive_geometry_specific_patches_from_one_dirty_collection() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (large_control, large_render) = connect_test_shell(&mut server, 7, 80, 23);
    let mut large_render = PaneSurfaceReceiver::new(large_render);
    let (small_control, small_render) = connect_test_shell(&mut server, 8, 68, 17);
    let mut small_render = PaneSurfaceReceiver::new(small_render);
    let _ = large_control.recv().expect("large snapshot");
    let _ = small_control.recv().expect("small snapshot");
    server.render_and_stream();
    let large_initial = recv_pane_surface(&mut large_render, "large initial surface");
    let small_initial = recv_pane_surface(&mut small_render, "small initial surface");
    let initial_size = server.app.test_runtime(pane_id).current_size();
    assert_eq!(
        (large_initial.frame.width, large_initial.frame.height),
        (80, 23)
    );
    assert_eq!(
        (small_initial.frame.width, small_initial.frame.height),
        (68, 17)
    );
    assert_ne!(
        large_initial.panes[0].inner_rect,
        small_initial.panes[0].inner_rect
    );

    write_shared_test_pane(&mut server, pane_id, b"\rMIXED");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));

    let large_patch = recv_pane_surface_patch(&mut large_render, "large retained patch");
    let small_patch = recv_pane_surface_patch(&mut small_render, "small retained patch");
    assert_eq!(
        large_patch.base_surface_revision,
        large_initial.surface_revision
    );
    assert_eq!(
        small_patch.base_surface_revision,
        small_initial.surface_revision
    );
    assert!(large_patch.spans.iter().all(|row| {
        row.x
            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
            <= large_initial.frame.width
            && row.y < large_initial.frame.height
    }));
    assert!(small_patch.spans.iter().all(|row| {
        row.x
            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
            <= small_initial.frame.width
            && row.y < small_initial.frame.height
    }));
    assert_eq!(large_patch.spans, small_patch.spans);
    assert_ne!(
        large_patch.meta.as_ref().expect("metadata").panes[0].inner_rect,
        small_patch.meta.as_ref().expect("metadata").panes[0].inner_rect
    );
    assert!(
        frame_text(
            &server.clients[&7]
                .render_state
                .last_pane_surface()
                .expect("large retained surface")
                .frame
        )
        .contains("MIXED")
    );
    assert!(
        frame_text(
            &server.clients[&8]
                .render_state
                .last_pane_surface()
                .expect("small retained surface")
                .frame
        )
        .contains("MIXED")
    );

    write_shared_test_pane(&mut server, pane_id, b"\x1b[?1049hALT");
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    server.render_and_stream();
    let large_alt = recv_pane_surface(&mut large_render, "large alternate-screen surface");
    let small_alt = recv_pane_surface(&mut small_render, "small alternate-screen surface");
    assert!(large_alt.panes[0].alternate_screen_active);
    assert!(small_alt.panes[0].alternate_screen_active);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        (initial_size.0, initial_size.1 + 1)
    );
    assert_eq!(
        large_alt.panes[0].inner_rect.width,
        large_initial.panes[0].inner_rect.width + 1
    );
    assert_eq!(
        small_alt.panes[0].inner_rect.width,
        small_initial.panes[0].inner_rect.width + 1
    );

    write_shared_test_pane(&mut server, pane_id, b"\x1b[?1049l");
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    server.render_and_stream();
    let large_main = recv_pane_surface(&mut large_render, "large restored main-screen surface");
    let small_main = recv_pane_surface(&mut small_render, "small restored main-screen surface");
    assert!(!large_main.panes[0].alternate_screen_active);
    assert!(!small_main.panes[0].alternate_screen_active);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        initial_size
    );
    assert_eq!(
        large_main.panes[0].inner_rect,
        large_initial.panes[0].inner_rect
    );
    assert_eq!(
        small_main.panes[0].inner_rect,
        small_initial.panes[0].inner_rect
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn retained_patches_only_reach_shells_viewing_the_dirty_tab() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("divergent-retained");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");

    let (first_control, first_render) = connect_matching_test_shell(&mut server, 7);
    let mut first_render = PaneSurfaceReceiver::new(first_render);
    let (second_control, second_render) = connect_matching_test_shell(&mut server, 8);
    let mut second_render = PaneSurfaceReceiver::new(second_render);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(8), &second_tab_id));
    assert!(server.claim_shell_tab_geometry(ClientId::test_new(8), false));
    assert!(
        server.pty_sources_visible_to_any_render_target(&HashSet::from([first_pane, second_pane,]))
    );
    server.render_and_stream();
    let _ = recv_pane_surface(&mut first_render, "first baseline");
    let _ = recv_pane_surface(&mut second_render, "second baseline");

    server
        .app
        .test_runtime(first_pane)
        .test_process_pty_bytes(b"\rFIRST_PATCH");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([first_pane])));
    let first_patch = recv_pane_surface_patch(&mut first_render, "first patch");
    assert_eq!(first_patch.meta.as_ref().expect("metadata").panes.len(), 1);
    assert!(second_render.try_recv().is_err());

    server
        .app
        .test_runtime(second_pane)
        .test_process_pty_bytes(b"\rSECOND_PATCH");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([second_pane])));
    let second_patch = recv_pane_surface_patch(&mut second_render, "second patch");
    assert_eq!(second_patch.meta.as_ref().expect("metadata").panes.len(), 1);
    assert!(first_render.try_recv().is_err());

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn late_retained_fallback_leaves_all_client_baselines_unchanged() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (_first_control, first_render) = connect_matching_test_shell(&mut server, 7);
    let mut first_render = PaneSurfaceReceiver::new(first_render);
    let (_second_control, second_render) = connect_matching_test_shell(&mut server, 8);
    let mut second_render = PaneSurfaceReceiver::new(second_render);
    server.render_and_stream();
    let _ = recv_pane_surface(&mut first_render, "first baseline");
    let _ = recv_pane_surface(&mut second_render, "second baseline");

    // Foreground renders last. Its old hyperlink forces a fallback after the first plan.
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(8)));
    let linked = server
        .clients
        .get_mut(&8)
        .expect("test precondition")
        .render_state
        .last_surface_mut()
        .expect("test precondition");
    linked.frame.hyperlinks.push("https://example.com".into());
    linked.frame.cells[0].hyperlink = Some(0);
    let before = [7, 8].map(|id| {
        server.clients[&id]
            .render_state
            .last_pane_surface()
            .expect("test precondition")
            .clone()
    });

    write_shared_test_pane(&mut server, pane_id, b"\rNEXT\x1b[?1003h");
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    assert!(first_render.try_recv().is_err());
    assert!(second_render.try_recv().is_err());
    for (id, expected) in [7, 8].into_iter().zip(before) {
        assert_eq!(
            server.clients[&id].render_state.last_pane_surface(),
            Some(&expected)
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn backpressured_shell_does_not_disable_retained_patches_for_responsive_peer() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (responsive_control, responsive_render) = connect_matching_test_shell(&mut server, 7);
    let (slow_control, slow_render) = connect_matching_test_shell(&mut server, 8);
    let mut slow_render = PaneSurfaceReceiver::new(slow_render);
    let _ = responsive_control.recv().expect("responsive snapshot");
    let _ = slow_control.recv().expect("slow snapshot");
    server.render_and_stream();
    let _ = responsive_render
        .recv()
        .expect("responsive initial surface");
    let _ = slow_render.recv("slow initial surface");

    let sources = HashSet::from([pane_id]);
    write_shared_test_pane(&mut server, pane_id, b"\rONE");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive first patch")),
        ServerMessage::SurfaceUpdate(_)
    ));

    let slow_baseline = server.clients[&8]
        .render_state
        .last_pane_surface()
        .expect("test precondition")
        .clone();
    write_shared_test_pane(&mut server, pane_id, b"\rTWO\x1b[?1003h");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive second patch")),
        ServerMessage::SurfaceUpdate(_)
    ));
    assert_eq!(server.clients[&8].deferred_render(), RenderDemand::Full);
    assert_eq!(
        server.clients[&8].render_state.last_pane_surface(),
        Some(&slow_baseline),
        "queue-full must not advance cells, metadata, cursor, or revision"
    );

    write_shared_test_pane(&mut server, pane_id, b"\rTHREE");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive third patch")),
        ServerMessage::SurfaceUpdate(_)
    ));

    assert!(matches!(
        slow_render.recv("slow queued first patch"),
        ServerMessage::PaneSurfacePatch(_)
    ));
    assert!(
        server.handle_server_event(ServerEvent::ClientWriterDrained {
            client_id: ClientId::test_new(8)
        })
    );
    server.render_and_stream();
    assert!(matches!(
        slow_render.recv("slow full recovery surface"),
        ServerMessage::PaneSurface(_) | ServerMessage::PaneSurfacePatch(_)
    ));

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn full_render_backpressure_does_not_disable_responsive_peer_patches() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (responsive_control, responsive_render) = connect_matching_test_shell(&mut server, 7);
    let (slow_control, slow_render) = connect_matching_test_shell(&mut server, 8);
    let _ = responsive_control.recv().expect("responsive snapshot");
    let _ = slow_control.recv().expect("slow snapshot");
    server.render_and_stream();
    let _ = responsive_render
        .recv()
        .expect("responsive initial surface");
    // Keep the slow client's initial surface queued, then force another full
    // replacement for both clients.
    server
        .clients
        .get_mut(&7)
        .expect("test precondition")
        .request_repaint();
    server
        .clients
        .get_mut(&8)
        .expect("test precondition")
        .request_repaint();
    server.app.full_redraw_pending = true;
    server.render_and_stream();
    let _ = responsive_render
        .recv()
        .expect("responsive full replacement");
    assert_eq!(server.clients[&8].deferred_render(), RenderDemand::Full);
    assert!(!server.app.full_redraw_pending);

    write_shared_test_pane(&mut server, pane_id, b"\rPATCH");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive retained patch")),
        ServerMessage::SurfaceUpdate(_)
    ));

    let _ = slow_render.recv().expect("slow queued initial surface");
    assert!(
        server.handle_server_event(ServerEvent::ClientWriterDrained {
            client_id: ClientId::test_new(8)
        })
    );
    server.render_and_stream();
    assert!(matches!(
        read_server_message(slow_render.recv().expect("slow full recovery surface")),
        ServerMessage::PaneSurface(_)
    ));

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_tab_focus_changes_only_the_source_connection() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("independent-tabs");
    let second_tab = workspace.test_add_tab(Some("second"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let tab_ids = server
        .app
        .session_snapshot()
        .tabs
        .into_iter()
        .map(|tab| tab.tab_id)
        .collect::<Vec<_>>();
    let first_tab_id = tab_ids[0].clone();
    let second_tab_id = tab_ids[second_tab].clone();

    let (first_control, _first_render) = connect_test_shell(&mut server, 7, 100, 30);
    let (second_control, _second_render) = connect_test_shell(&mut server, 8, 80, 24);
    let first_initial = client_shell_snapshot(&first_control);
    let second_initial = client_shell_snapshot(&second_control);
    assert_eq!(
        first_initial.focused_tab_id.as_deref(),
        Some(first_tab_id.as_str())
    );
    assert_eq!(
        second_initial.focused_tab_id.as_deref(),
        Some(first_tab_id.as_str())
    );

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: ClientId::test_new(8),
            boot_id: server.client_shell_boot_id.clone(),
            request: Box::new(shepr_api::schema::Request {
                id: "focus-second".into(),
                method: shepr_api::schema::Method::TabFocus(shepr_api::schema::TabTarget {
                    tab_id: second_tab_id.clone().to_string(),
                }),
            }),
        })
    );
    let response_ready = server
        .server_event_rx
        .recv()
        .await
        .expect("focus response ready");
    assert!(!server.handle_server_event(response_ready));
    let _ = second_control.recv().expect("focus response");

    server.render_and_stream();

    assert!(
        first_control.try_recv().is_err(),
        "another shell must not receive a navigation replacement"
    );
    let second_replacement = client_shell_snapshot(&second_control);
    assert_eq!(
        second_replacement.focused_tab_id.as_deref(),
        Some(second_tab_id.as_str())
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_request_renders_and_refreshes_changed_default_focus() {
    use shepr_api::schema::{Method, PaneSelectionPoint, PaneSelectionReadParams};

    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("default-focus-cache");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_tab_id = server.app.public_tab_id(0, 0).expect("test precondition");
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");
    let first_pane_id = server
        .app
        .public_pane_id(0, first_pane)
        .expect("test precondition");

    let (control, _render) = connect_matching_test_shell(&mut server, 70);
    let initial = client_shell_snapshot(&control);
    assert_eq!(
        initial.focused_tab_id.as_deref(),
        Some(first_tab_id.as_str())
    );

    // A server-side focus change can leave this connection's location behind.
    assert!(server.app.state.switch_workspace_tab(0, second_tab));
    server.app.state.mark_shell_projection_dirty();
    server.render_and_stream();
    assert_eq!(
        server
            .shell_session_cache
            .as_ref()
            .and_then(|cache| cache.session.focused_tab_id.as_deref()),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        server
            .shell_tab_id_for_client(ClientId::test_new(70))
            .as_deref(),
        Some(first_tab_id.as_str())
    );
    assert_eq!(
        server
            .default_shell_target()
            .map(|target| target.tab_id.to_string())
            .as_deref(),
        Some(second_tab_id.as_str())
    );

    let previous_revision = server.app.state.shell_projection_revision;
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let changed = server.handle_client_shell_api_request(
        ClientId::test_new(70),
        shepr_api::ApiRequestMessage {
            request: shepr_api::schema::Request {
                id: "read-selection".into(),
                method: Method::PaneSelectionRead(PaneSelectionReadParams {
                    pane_id: first_pane_id.to_string(),
                    anchor: PaneSelectionPoint {
                        row: shepr_vt::AbsRow(0),
                        col: 0,
                    },
                    cursor: PaneSelectionPoint {
                        row: shepr_vt::AbsRow(0),
                        col: 0,
                    },
                    content_revision: None,
                }),
            },
            respond_to,
        },
    );
    assert!(response_rx.recv().is_ok());
    assert!(
        changed,
        "changing the session default must request a render"
    );
    assert!(server.app.state.shell_projection_revision > previous_revision);

    server.render_and_stream();
    let cache = server
        .shell_session_cache
        .as_ref()
        .expect("render should rebuild the shell session cache");
    assert_eq!(cache.revision, server.app.state.shell_projection_revision);
    assert_eq!(
        cache.session.focused_tab_id.as_deref(),
        Some(first_tab_id.as_str())
    );
    let shell = server.clients[&70]
        .shell_state()
        .expect("test shell connection");
    assert_eq!(shell.session_generation, server.shell_session_generation);
    assert_eq!(
        shell
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.focused_tab_id.as_deref()),
        Some(first_tab_id.as_str()),
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
    let mut workspace = shepr_mux::workspace::Workspace::test_new("independent-focus");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();
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

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(first_pane, first_runtime);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(62), &second_tab_id));
    server
        .clients
        .get_mut(&61)
        .expect("test precondition")
        .shell_state_mut()
        .expect("shell state")
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&62)
        .expect("test precondition")
        .shell_state_mut()
        .expect("shell state")
        .outer_terminal_focus = Some(true);

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_client_shell_api_request(
        ClientId::test_new(62),
        shepr_api::ApiRequestMessage {
            request: shepr_api::schema::Request {
                id: "focus-own-tab".into(),
                method: shepr_api::schema::Method::TabFocus(shepr_api::schema::TabTarget {
                    tab_id: second_tab_id.to_string(),
                }),
            },
            respond_to,
        },
    );
    server.app.sync_focus_events();
    assert!(first_input.try_recv().is_err());
    assert!(second_input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_local_navigation_leaves_the_other_clients_focus_alone() {
    use shepr_api::schema::{Method, PaneTarget, TabTarget};

    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("focus-events");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_tab_id = server.app.public_tab_id(0, 0).expect("test precondition");
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");
    let first_pane_id = server
        .app
        .public_pane_id(0, first_pane)
        .expect("test precondition");
    let second_pane_id = server
        .app
        .public_pane_id(0, second_pane)
        .expect("test precondition");

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    let first_tab = TabTarget {
        tab_id: first_tab_id.to_string(),
    };
    let second_tab = TabTarget {
        tab_id: second_tab_id.to_string(),
    };
    let cases = [
        (61, Method::TabFocus(second_tab.clone())),
        (62, Method::TabFocus(second_tab.clone())),
        (61, Method::TabFocus(second_tab.clone())),
        (61, Method::TabFocus(first_tab)),
        (
            62,
            Method::PaneFocus(PaneTarget {
                pane_id: second_pane_id.clone().to_string(),
            }),
        ),
        (
            62,
            Method::PaneFocus(PaneTarget {
                pane_id: first_pane_id.clone().to_string(),
            }),
        ),
        (61, Method::TabFocus(second_tab.clone())),
        (61, Method::TabClose(second_tab)),
    ];
    for (client_id, method) in cases {
        let other_client = ClientId::test_new(if client_id == 61 { 62 } else { 61 });
        let other_focus = server.shell_focus_target(other_client);
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        server.handle_client_shell_api_request(
            client_id.into(),
            shepr_api::ApiRequestMessage {
                request: shepr_api::schema::Request {
                    id: "navigate".into(),
                    method,
                },
                respond_to,
            },
        );
        let response = response_rx.recv().expect("navigation response");
        assert!(
            serde_json::from_str::<shepr_api::schema::SuccessResponse>(
                &crate::test_support::test_json(&response)
            )
            .is_ok(),
            "{response:?}"
        );
        server.app.sync_focus_events();

        assert_eq!(
            server.shell_focus_target(other_client),
            other_focus,
            "client {client_id}"
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_focus_moves_shell_focus_between_tabs() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("public-focus-events");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();
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

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(first_pane, first_runtime);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("second tab id");

    let (first_control, _) = connect_matching_test_shell(&mut server, 63);
    let (second_control, _) = connect_matching_test_shell(&mut server, 64);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(64), &second_tab_id));
    server
        .clients
        .get_mut(&63)
        .expect("test precondition")
        .shell_state_mut()
        .expect("shell state")
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&64)
        .expect("test precondition")
        .shell_state_mut()
        .expect("shell state")
        .outer_terminal_focus = Some(true);
    assert!(server.app.state.switch_workspace_tab(0, second_tab));

    server.focus_all_shell_clients_on_default_target();

    assert_eq!(
        server
            .shell_tab_id_for_client(ClientId::test_new(63))
            .as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        first_input.try_recv().expect("previous tab focus lost"),
        Bytes::from_static(b"\x1b[O")
    );
    assert!(
        second_input.try_recv().is_err(),
        "focus gain was duplicated"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn repeated_layout_action_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("layout-geometry");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let tab_id = server.app.public_tab_id(0, 0).expect("tab id");

    let (control, _) = connect_test_shell(&mut server, 65, 100, 30);
    let _ = control.recv().expect("snapshot");
    let before = server.app.test_runtime(first_pane).current_size();
    let (respond_to, _response_rx) = std::sync::mpsc::channel();

    assert!(server.handle_client_shell_api_request(
        ClientId::test_new(65),
        shepr_api::ApiRequestMessage {
            request: shepr_api::schema::Request {
                id: "resize-layout".into(),
                method: shepr_api::schema::Method::LayoutSetSplitRatio(
                    shepr_api::schema::LayoutSetSplitRatioParams {
                        tab_id: Some(tab_id.to_string()),
                        pane_id: None,
                        path: Vec::new(),
                        ratio: 0.8,
                    },
                ),
            },
            respond_to,
        },
    ));

    let after = server.app.test_runtime(first_pane).current_size();
    assert_ne!(after, before);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_close_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("public-close-geometry");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_pane = workspace.test_split(shepr_core::layout::Direction::Vertical);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_pane_id = server
        .app
        .public_pane_id(0, second_pane)
        .expect("test precondition");

    let (control, _) = connect_test_shell(&mut server, 66, 100, 30);
    let _ = control.recv().expect("snapshot");
    let shrunk = server.app.test_runtime(first_pane).current_size();
    assert!(shrunk.0 < 30);

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    assert!(
        server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
            request: shepr_api::schema::Request {
                id: "public-close-geometry".into(),
                method: shepr_api::schema::Method::PaneClose(shepr_api::schema::PaneTarget {
                    pane_id: second_pane_id.to_string(),
                }),
            },
            respond_to,
        })
    );

    let runtime = &server.app.test_runtime(first_pane);
    let grown = runtime.current_size();
    assert!(grown.0 > shrunk.0);
    assert_eq!(runtime.terminal_dimensions(), Some((grown.1, grown.0)));
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("test precondition")
            .viewport_rows,
        grown.0 as usize
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn geometry_reapply_replaces_a_controller_that_left_the_tab() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("geometry-controller-viewer");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();
    let third_tab = workspace.test_add_tab(Some("third"));
    let third_pane = workspace.tabs()[third_tab].root_pane();
    server.app.state.workspaces = vec![workspace];
    for pane_id in [first_pane, second_pane, third_pane] {
        server.app.insert_test_runtime(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");
    let third_tab_id = server
        .app
        .public_tab_id(0, third_tab)
        .expect("test precondition");

    let (first_control, _) = connect_test_shell(&mut server, 67, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 68, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    assert!(server.focus_shell_client_on_tab(ClientId::test_new(67), &second_tab_id));
    assert!(server.claim_shell_tab_geometry(ClientId::test_new(67), false));
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(67), &third_tab_id));
    assert!(server.claim_shell_tab_geometry(ClientId::test_new(67), false));
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(68), &second_tab_id));
    assert_eq!(
        server.clients.geometry_controller(&second_tab_id),
        Some(ClientId::test_new(67))
    );
    let stale_size = server.app.test_runtime(second_pane).current_size();

    assert!(server.reapply_controlled_shell_tab_geometry(false));

    assert_eq!(
        server.clients.geometry_controller(&second_tab_id),
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
    let mut workspace = shepr_mux::workspace::Workspace::test_new("controller-disconnect");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();
    server.app.state.workspaces = vec![workspace];
    for pane_id in [first_pane, second_pane] {
        server.app.insert_test_runtime(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");

    let (first_control, _) = connect_test_shell(&mut server, 31, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 32, 70, 20);
    let (third_control, _) = connect_test_shell(&mut server, 33, 60, 16);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    let _ = third_control.recv().expect("third snapshot");

    assert!(server.focus_shell_client_on_tab(ClientId::test_new(32), &second_tab_id));
    assert!(server.claim_shell_tab_geometry(ClientId::test_new(32), false));
    let remaining_viewer_size = server.app.test_runtime(second_pane).current_size();
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(31), &second_tab_id));
    assert!(server.claim_shell_tab_geometry(ClientId::test_new(31), false));
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        remaining_viewer_size
    );

    // Two shells remain, so this is not the single-shell resize path.
    server.remove_client_and_resize_if_needed(ClientId::test_new(31));

    assert_eq!(
        server.clients.geometry_controller(&second_tab_id),
        Some(ClientId::test_new(32))
    );
    assert_eq!(
        server.app.test_runtime(second_pane).current_size(),
        remaining_viewer_size
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_tabs_render_accept_input_and_resize_independently() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("independent-geometry");
    let first_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();

    let (second_runtime, mut second_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"SECOND_TAB",
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"FIRST_TAB"),
    );
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");
    let second_pane_id = server
        .app
        .public_pane_id(0, second_pane)
        .expect("test precondition");
    let initial_second_size = server.app.test_runtime(second_pane).current_size();

    let (first_control, first_render) = connect_test_shell(&mut server, 21, 100, 30);
    let _ = first_control.recv().expect("first snapshot");
    let first_size = server.app.test_runtime(first_pane).current_size();
    let singleton_second_size = server.app.test_runtime(second_pane).current_size();
    assert_ne!(singleton_second_size, initial_second_size);
    assert_eq!(singleton_second_size, first_size);

    let (second_control, second_render) = connect_test_shell(&mut server, 22, 70, 20);
    let _ = second_control.recv().expect("second snapshot");

    assert!(server.focus_shell_client_on_tab(ClientId::test_new(22), &second_tab_id));
    assert!(server.claim_shell_tab_geometry(ClientId::test_new(22), false));
    let second_size = server.app.test_runtime(second_pane).current_size();
    assert_ne!(first_size, second_size);
    assert_eq!(
        server.app.test_runtime(first_pane).current_size(),
        first_size
    );

    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: ClientId::test_new(22),
        pane_id: second_pane_id.parse().expect("test precondition"),
        events: vec![shepr_protocol::ClientPaneInputEvent::TextCommit(
            "typed".into(),
        )],
    });
    assert_eq!(
        second_input.try_recv().expect("second tab input"),
        Bytes::from_static(b"typed")
    );

    server.render_and_stream();
    let first_surface = match read_server_message(first_render.recv().expect("first surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected first pane surface, got {other:?}"),
    };
    let second_surface = match read_server_message(second_render.recv().expect("second surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected second pane surface, got {other:?}"),
    };
    assert!(frame_text(&first_surface.frame).contains("FIRST_TAB"));
    assert!(frame_text(&second_surface.frame).contains("SECOND_TAB"));

    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: ClientId::test_new(22),
        surface_cols: 60,
        surface_rows: 16,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    let resized_second = server.app.test_runtime(second_pane).current_size();
    assert_ne!(resized_second, second_size);
    assert_eq!(
        server.app.test_runtime(first_pane).current_size(),
        first_size
    );

    assert!(server.focus_shell_client_on_tab(ClientId::test_new(21), &second_tab_id));
    assert!(server.claim_shell_tab_geometry(ClientId::test_new(21), false));
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        resized_second
    );

    server.remove_client_and_resize_if_needed(ClientId::test_new(21));
    let singleton_first = server.app.test_runtime(first_pane).current_size();
    let singleton_second = server.app.test_runtime(second_pane).current_size();
    assert_ne!(singleton_first, first_size);
    assert_eq!(singleton_first, singleton_second);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_background_tab_create_preserves_client_locations() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("background-create");
    let second_tab = workspace.test_add_tab(Some("second"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let workspace_id = server
        .app
        .public_workspace_id(0)
        .expect("test precondition");
    let first_tab_id = server.app.public_tab_id(0, 0).expect("test precondition");
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");

    let (first_control, _) = connect_matching_test_shell(&mut server, 71);
    let (second_control, _) = connect_matching_test_shell(&mut server, 72);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(71), &second_tab_id));

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::Request {
            id: "create-background-tab".into(),
            method: shepr_api::schema::Method::TabCreate(shepr_api::schema::TabCreateParams {
                workspace_id: Some(workspace_id.to_string()),
                cwd: None,
                focus: false,
                label: Some("background".into()),
                env: std::collections::HashMap::new(),
            }),
        },
        respond_to,
    });

    assert_eq!(
        server
            .shell_tab_id_for_client(ClientId::test_new(71))
            .as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        server
            .shell_tab_id_for_client(ClientId::test_new(72))
            .as_deref(),
        Some(first_tab_id.as_str())
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_workspace_focus_preserves_each_clients_remembered_tabs() {
    let mut server = test_headless_server();
    let mut first = shepr_mux::workspace::Workspace::test_new("first");
    let second_tab = first.test_add_tab(Some("second"));
    let second = shepr_mux::workspace::Workspace::test_new("second");
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_workspace_id = server.app.state.workspaces[0].id.clone();
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");
    let first_tab_id = server.app.public_tab_id(0, 0).expect("test precondition");
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");

    let (first_control, _) = connect_test_shell(&mut server, 41, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 42, 80, 24);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(41), &second_tab_id));

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::Request {
            id: "focus-second-workspace".into(),
            method: shepr_api::schema::Method::WorkspaceFocus(shepr_api::schema::WorkspaceTarget {
                workspace_id: second_workspace_id.clone().to_string(),
            }),
        },
        respond_to,
    });

    let first_location = server.clients[&41]
        .shell_state()
        .and_then(|shell| shell.location.as_ref())
        .expect("test precondition");
    let second_location = server.clients[&42]
        .shell_state()
        .and_then(|shell| shell.location.as_ref())
        .expect("test precondition");
    assert_eq!(
        first_location.focused_workspace_id.as_deref(),
        Some(second_workspace_id.as_str())
    );
    assert_eq!(
        second_location.focused_workspace_id.as_deref(),
        Some(second_workspace_id.as_str())
    );
    assert_eq!(
        first_location.active_tab_ids[&first_workspace_id].to_string(),
        second_tab_id
    );
    assert_eq!(
        second_location.active_tab_ids[&first_workspace_id].to_string(),
        first_tab_id
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_pane_focus_replaces_a_diverged_client_shell_projection() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("first");
    let first_pane = first.tabs()[0].root_pane();

    let second = shepr_mux::workspace::Workspace::test_new("second");
    let second_pane = second.tabs()[0].root_pane();

    server.app.state.workspaces = vec![first, second];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"FIRST_AGENT"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"SECOND_WORKSPACE"),
    );
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_workspace_id = server
        .app
        .public_workspace_id(0)
        .expect("test precondition");
    let first_tab_id = server.app.public_tab_id(0, 0).expect("test precondition");
    let first_pane_id = server
        .app
        .public_pane_id(0, first_pane)
        .expect("test precondition");
    let second_tab_id = server.app.public_tab_id(1, 0).expect("test precondition");

    let (control_rx, render_rx) = connect_test_shell(&mut server, 9, 80, 23);
    let mut render_rx = PaneSurfaceReceiver::new(render_rx);
    let _ = client_shell_snapshot(&control_rx);
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(9), &second_tab_id));
    assert!(server.claim_shell_tab_geometry(ClientId::test_new(9), false));
    server.render_and_stream();
    let diverged = client_shell_snapshot(&control_rx);
    assert_eq!(
        diverged.focused_workspace_id.as_deref(),
        Some(
            server
                .app
                .public_workspace_id(1)
                .expect("test precondition")
                .as_str()
        )
    );
    let diverged_surface = recv_pane_surface(&mut render_rx, "diverged surface");
    assert!(frame_text(&diverged_surface.frame).contains("SECOND_WORKSPACE"));

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::Request {
            id: "focus-first-pane".into(),
            method: shepr_api::schema::Method::PaneFocus(shepr_api::schema::PaneTarget {
                pane_id: first_pane_id.clone().to_string(),
            }),
        },
        respond_to,
    });
    let response: shepr_api::schema::SuccessResponse = serde_json::from_str(
        &crate::test_support::test_json(&response_rx.recv().expect("pane focus response")),
    )
    .expect("test precondition");
    let shepr_api::schema::ResponseResult::PaneInfo { pane } = response.result else {
        panic!("expected pane info");
    };
    assert_eq!(pane.pane_id, first_pane_id);
    assert!(pane.focused);
    assert_eq!(server.app.state.active_index(), Some(0));
    let location = server.clients[&9]
        .shell_state()
        .and_then(|shell| shell.location.as_ref())
        .expect("test precondition");
    assert_eq!(
        location.focused_workspace_id.as_deref(),
        Some(first_workspace_id.as_str())
    );
    assert_eq!(
        location.focused_tab_id().map(ToString::to_string),
        Some(first_tab_id.clone()).map(|id| id.to_string())
    );

    server.render_and_stream();
    let replacement = client_shell_snapshot(&control_rx);
    assert_eq!(
        replacement.focused_workspace_id.as_deref(),
        Some(first_workspace_id.as_str())
    );
    let replacement_surface = recv_pane_surface(&mut render_rx, "pane focus replacement surface");
    assert!(frame_text(&replacement_surface.frame).contains("FIRST_AGENT"));
    assert!(!frame_text(&replacement_surface.frame).contains("SECOND_WORKSPACE"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_api_focus_replaces_every_client_shell_projection() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("first");
    let second = shepr_mux::workspace::Workspace::test_new("second");
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_id = server.app.session_snapshot().workspaces[1]
        .workspace_id
        .clone();

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: ClientId::test_new(9),
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let initial_revision = client_shell_snapshot(&control_rx).revision;

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(shepr_api::ApiRequestMessage {
        request: shepr_api::schema::Request {
            id: "test.client.shell.workspace.focus".into(),
            method: shepr_api::schema::Method::WorkspaceFocus(shepr_api::schema::WorkspaceTarget {
                workspace_id: second_id.clone().to_string(),
            }),
        },
        respond_to,
    });
    assert_eq!(server.app.state.active_index(), Some(1));
    server.render_and_stream();

    let replacement = client_shell_snapshot(&control_rx);
    assert!(replacement.revision > initial_revision);
    assert_eq!(
        replacement.focused_workspace_id.as_deref(),
        Some(second_id.as_str())
    );
    match read_server_message(render_rx.recv().expect("replacement pane surface")) {
        ServerMessage::PaneSurface(surface) => {
            assert_eq!(surface.projection_revision, replacement.revision);
        }
        other => panic!("expected replacement pane surface, got {other:?}"),
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_input_targets_runtime_without_server_shell_classification() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1000h\x1b[?1006h");
    let pane_id = server
        .app
        .session_snapshot()
        .focused_pane_id
        .expect("test precondition");
    server.clients.insert(
        11,
        ClientConnection::with_shell(
            ClientShellState::active(),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            None,
        ),
    );

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id: pane_id.parse().expect("test precondition"),
            events: vec![
                shepr_protocol::ClientPaneInputEvent::Key {
                    code: shepr_protocol::ClientKeyCode::Char('c'),
                    modifiers: shepr_protocol::WireModifiers::CONTROL,
                    kind: shepr_protocol::ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                },
                shepr_protocol::ClientPaneInputEvent::Key {
                    code: shepr_protocol::ClientKeyCode::Char('c'),
                    modifiers: shepr_protocol::WireModifiers::CONTROL,
                    kind: shepr_protocol::ClientKeyKind::Release,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                },
                shepr_protocol::ClientPaneInputEvent::Key {
                    code: shepr_protocol::ClientKeyCode::Char('x'),
                    modifiers: shepr_protocol::WireModifiers::ALT,
                    kind: shepr_protocol::ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                },
                shepr_protocol::ClientPaneInputEvent::Mouse {
                    kind: shepr_protocol::ClientMouseKind::Down(
                        shepr_protocol::ClientMouseButton::Left,
                    ),
                    position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                    geometry: None,
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
    let pane_id = server
        .app
        .session_snapshot()
        .focused_pane_id
        .expect("test precondition");

    let (workspace_index, runtime_pane_id) = server
        .app
        .parse_pane_id(&pane_id)
        .expect("runtime pane target");
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(
            &server.app.terminal_runtimes,
            workspace_index,
            runtime_pane_id,
        )
        .expect("focused runtime");
    assert_eq!(runtime.current_size(), (24, 79));
    assert!(input_rx.try_recv().is_err(), "legacy release emitted bytes");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_hidden_pane_rejects_presses_but_accepts_releases() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("hidden-input");
    let hidden_tab = workspace.test_add_tab(Some("hidden"));
    let hidden_pane = workspace.tabs()[hidden_tab].root_pane();
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[>3u",
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(hidden_pane, runtime);
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    let pane_id = server
        .app
        .public_pane_id(0, hidden_pane)
        .expect("test precondition");
    server.clients.insert(
        11,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            None,
        ),
    );
    let key = |kind| shepr_protocol::ClientPaneInputEvent::Key {
        code: shepr_protocol::ClientKeyCode::Char('x'),
        modifiers: shepr_protocol::WireModifiers::NONE,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
    };

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id: pane_id.parse().expect("test precondition"),
            events: vec![key(shepr_protocol::ClientKeyKind::Press)],
        })
    );
    assert!(input_rx.try_recv().is_err());
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id: pane_id.parse().expect("test precondition"),
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
    let pane_id = workspace.tabs()[0].root_pane();
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
            .scroll_metrics()
            .is_some_and(|metrics| metrics.offset_from_bottom > 0)
    );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    let public_pane_id = server
        .app
        .public_pane_id(0, pane_id)
        .expect("test precondition");
    server.clients.insert(
        11,
        ClientConnection::with_shell(
            ClientShellState::active(),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            None,
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(11)));

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id: public_pane_id.parse().expect("test precondition"),
            events: vec![shepr_protocol::ClientPaneInputEvent::TextCommit(
                "x".to_owned(),
            )],
        });

    assert_eq!(render_impact, RenderDemand::Full);
    assert_eq!(
        input_rx.try_recv().expect("text must reach the PTY"),
        Bytes::from_static(b"x")
    );
    assert_eq!(
        server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .and_then(shepr_mux::pane::PaneRuntime::scroll_metrics)
            .map(|metrics| metrics.offset_from_bottom),
        Some(0)
    );

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id: public_pane_id.parse().expect("test precondition"),
            events: vec![shepr_protocol::ClientPaneInputEvent::TextCommit(
                "y".to_owned(),
            )],
        });
    assert_eq!(render_impact, RenderDemand::None);
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
    let pane_id = server
        .app
        .session_snapshot()
        .focused_pane_id
        .expect("test precondition");
    server.clients.insert(
        11,
        ClientConnection::with_shell(
            ClientShellState::active(),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            None,
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(11)));
    assert!(server.claim_unowned_shell_tab_geometry(ClientId::test_new(11), false));

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id: pane_id.parse().expect("test precondition"),
            events: vec![shepr_protocol::ClientPaneInputEvent::Mouse {
                kind: shepr_protocol::ClientMouseKind::Moved,
                position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                geometry: None,
                modifiers: shepr_protocol::WireModifiers::NONE,
                lines: 0,
            }],
        });

    assert_eq!(render_impact, RenderDemand::None);
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
    let pane_id = server
        .app
        .session_snapshot()
        .focused_pane_id
        .expect("test precondition");
    server.clients.insert(
        11,
        ClientConnection::with_shell(
            ClientShellState::active(),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            None,
        ),
    );

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(11),
            pane_id: pane_id.parse().expect("test precondition"),
            events: vec![shepr_protocol::ClientPaneInputEvent::Mouse {
                kind: shepr_protocol::ClientMouseKind::Moved,
                position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                geometry: None,
                modifiers: shepr_protocol::WireModifiers::NONE,
                lines: 0,
            }],
        });

    assert_eq!(render_impact, RenderDemand::Full);
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
    let pane_id = server
        .app
        .session_snapshot()
        .focused_pane_id
        .expect("test precondition");
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        11,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            Some(writer),
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(11)));

    let events = ["a", "b", "c", "d", "e", "f"]
        .into_iter()
        .map(|text| shepr_protocol::ClientPaneInputEvent::TextCommit(text.to_owned()))
        .collect();
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: ClientId::test_new(11),
        pane_id: pane_id.parse().expect("test precondition"),
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
    assert!(message.contains(pane_id.as_str()), "message: {message}");
    assert!(message.contains("2 events"), "message: {message}");
    shutdown_test_runtimes(&mut server);
}

pub(crate) fn install_focused_test_runtime(
    server: &mut HeadlessServer,
    terminal_bytes: &[u8],
) -> tokio::sync::mpsc::Receiver<Bytes> {
    let workspace = shepr_mux::workspace::Workspace::test_new("focus-reporting");
    let pane_id = workspace.tabs()[0].root_pane();
    let (runtime, input_rx) = shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
        80,
        24,
        0,
        terminal_bytes,
        4,
    );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    input_rx
}

fn retained_test_server_with_control(
    initial_screen: &[u8],
) -> (
    HeadlessServer,
    std::sync::mpsc::Receiver<Vec<u8>>,
    RenderLaneReceiver,
    shepr_core::layout::PaneId,
) {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("test");
    let pane_id = workspace.focused_pane_id();

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, initial_screen),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;

    let (client_tx, client_control_rx, client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            Some(client_tx),
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(1)));
    server.sync_foreground_client_state();
    assert!(server.claim_unowned_shell_tab_geometry(ClientId::test_new(1), true));

    (server, client_control_rx, client_rx, pane_id)
}

#[test]
fn client_shell_host_theme_follows_foreground_client() {
    let mut server = test_headless_server();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            2,
            None,
        ),
    );
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
        server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: ClientId::test_new(1),
            update: shepr_protocol::ClientHostThemeUpdate::DefaultColor {
                kind: shepr_protocol::ClientHostDefaultColorKind::Background,
                color: dark,
            },
        })
    );
    assert!(
        server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: ClientId::test_new(1),
            update: shepr_protocol::ClientHostThemeUpdate::PaletteColors(vec![(4, blue)]),
        })
    );
    server.handle_server_event(ServerEvent::ClientShellHostTheme {
        client_id: ClientId::test_new(1),
        update: shepr_protocol::ClientHostThemeUpdate::Appearance(
            shepr_protocol::ClientHostAppearance::Dark,
        ),
    });
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(dark.into())
    );
    assert_eq!(
        server.app.state.host_terminal_theme.palette[4],
        Some(blue.into())
    );
    assert_eq!(
        server.app.state.host_terminal_appearance,
        Some(shepr_termio::host_term::theme::HostAppearance::Dark)
    );
    assert!(server.app.state.host_terminal_appearance_explicit);

    let light = shepr_protocol::ClientHostColor {
        r: 240,
        g: 230,
        b: 220,
    };
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: ClientId::test_new(2),
            update: shepr_protocol::ClientHostThemeUpdate::DefaultColor {
                kind: shepr_protocol::ClientHostDefaultColorKind::Background,
                color: light,
            },
        })
    );
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(dark.into())
    );

    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(2)));
    server.sync_foreground_client_state();
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(light.into())
    );
    assert_eq!(
        server.app.state.host_terminal_appearance,
        Some(shepr_termio::host_term::theme::HostAppearance::Light)
    );
    assert!(!server.app.state.host_terminal_appearance_explicit);
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
        server.handle_server_event(ServerEvent::ClientPasteRejected {
            client_id: ClientId::test_new(7),
            size: 2_000_000,
            max: 1_048_576,
        });
    }
    let pastes = notices();
    assert_eq!(pastes.len(), 2);
    assert!(pastes[0].starts_with("Paste rejected"));
    assert!(
        server.clients.contains_key(&7),
        "notices never end the connection"
    );
    shutdown_test_runtimes(&mut server);
}

fn with_terminal_session_test_server(
    test: impl FnOnce(&mut HeadlessServer, shepr_protocol::TerminalId, String, String),
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("test");
    let pane_id = workspace.tabs()[0].root_pane();
    let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
    let terminal_id_string = terminal_id.to_string();
    let public_pane_id = format!("{}:p1", workspace.id);
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.terminal_runtimes.insert(
        terminal_id.clone(),
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );

    test(&mut server, terminal_id, terminal_id_string, public_pane_id);

    drop(server);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn unchanged_git_refresh_does_not_request_headless_render() {
    let mut server = test_headless_server();
    server.app.git_refresh.git_refresh_in_flight = true;
    let mut workspace = shepr_mux::workspace::Workspace::test_new("one");
    let workspace_id = workspace.id.to_string();
    let cwd = workspace.identity_cwd.clone();
    workspace.cached_auto_label = "cached".into();
    workspace.cached_git_status_key = cwd.clone();
    workspace.cached_git_branch = None;
    server.app.state.workspaces.push(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        results: vec![shepr_mux::git::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: shepr_mux::git::GitStatusRefreshDemand::ALL,
            auto_label: "cached".into(),
            branch: None,
            ahead_behind: None,
            space: None,
        }],
        cache_updates: Vec::new(),
    });

    assert!(!changed);
    assert!(!server.app.git_refresh.git_refresh_in_flight);
}

#[test]
fn changed_git_refresh_requests_headless_render() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("one");
    let workspace_id = workspace.id.to_string();
    let cwd = workspace.identity_cwd.clone();
    server.app.state.workspaces.push(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        results: vec![shepr_mux::git::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: shepr_mux::git::GitStatusRefreshDemand::ALL,
            auto_label: "one".into(),
            branch: Some("changed".into()),
            ahead_behind: None,
            space: None,
        }],
        cache_updates: Vec::new(),
    });

    assert!(changed);
}

#[tokio::test]
async fn host_shutdown_warning_freezes_saves_before_applying_events_and_thaws_on_cancel() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("host-shutdown");
    let pane_id = workspace.tabs()[0].root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(true, Ordering::Release);
    // The test policy never saves, so the checkpoint writes nothing and the
    // real session file is untouched.
    server.sync_host_shutdown_freeze(Instant::now());
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Frozen);
    assert_eq!(server.lifecycle.frozen_session_policy(), Some(false));
    // Pretend saving was on before the warning, so the thaw has to restore it.
    server.lifecycle.set_frozen_session_policy_for_test(true);
    assert!(!server.app.policy.persists_session());
    assert!(server.app.session_saver.session_save_deadline.is_none());

    // The server keeps running and applies pane deaths; only the disk is frozen.
    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Exited,
        })
    );
    assert!(server.app.find_pane(pane_id).is_none());
    assert!(!server.app.policy.persists_session());

    // Cancellation reported through the flag thaws and re-saves current state.
    server.app.state.session_dirty = false;
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(false, Ordering::Release);
    server.sync_host_shutdown_freeze(Instant::now());
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Running);
    assert!(server.app.policy.persists_session());
    assert!(server.app.state.session_dirty);
    // Not stopping: the warning alone never ends the server.
    assert!(
        !server
            .lifecycle
            .stop_requested(server.app.state.should_quit)
    );
    server.app.policy = crate::app::AppPolicy::Suspended;
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn oversized_shell_frame_is_reported_once_until_a_frame_is_sent() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("oversized");
    let pane_id = workspace.tabs()[0].root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    // Keep the surface within the protocol's cell-count limit, but make one
    // displayed grapheme large enough that its encoded frame exceeds the byte
    // limit. This exercises the oversized-frame path with valid geometry.
    let mut screen = String::with_capacity(2_200_001);
    screen.push('x');
    for _ in 0..1_100_000 {
        screen.push('\u{0301}');
    }
    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, screen.as_bytes()),
    );
    let (control, render_rx) = connect_test_shell(&mut server, 91, 80, 24);
    // The test writer forwards queued control messages from a background
    // thread (see `ClientWriter::test_pair`), so a message queued by this
    // render is not necessarily visible to a bare `try_recv` yet. Wait a
    // short beat for the drain thread instead of racing it.
    let drain_notices = || {
        std::iter::from_fn(|| {
            control
                .recv_timeout(std::time::Duration::from_millis(500))
                .ok()
        })
        .map(read_server_message)
        .filter(|message| matches!(message, ServerMessage::ClientShellError { .. }))
        .count()
    };
    let reported = |server: &HeadlessServer| {
        server
            .clients
            .get(&91)
            .expect("client stays connected")
            .oversized_frame_reported
    };

    server.render_and_stream();
    assert!(reported(&server));
    assert_eq!(drain_notices(), 1, "the first oversized frame is reported");
    assert!(
        render_rx.try_recv().is_err(),
        "nothing oversized was queued"
    );

    server.render_and_stream();
    assert!(reported(&server));
    assert_eq!(drain_notices(), 0, "the report is not repeated per render");

    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: ClientId::test_new(91),
        surface_cols: 80,
        surface_rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    server.render_and_stream();
    assert!(!reported(&server), "a frame that fits clears the report");
    assert!(render_rx.try_recv().is_ok(), "the smaller frame was sent");
    assert!(
        shepr_protocol::NoticeKind::OversizedFrame {
            claimed: 3_000_000,
            max: MAX_FRAME_SIZE
        }
        .to_string()
        .contains("too large")
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn client_socket_is_owner_only_from_the_moment_it_is_reachable() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = crate::test_support::ScratchDir::new("hb");
    let path = dir.join("client.sock");

    let (listener, startup_lock, _) =
        shepr_platform::ipc::bind_private_socket(&path).expect("bind");
    let mode = fs::metadata(&path)
        .expect("socket exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    // The staging directory is gone; only the socket and persistent lock remain.
    let entries = fs::read_dir(&dir)
        .expect("test precondition")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect::<std::collections::BTreeSet<_>>();
    let expected_entries = ["client.sock", "client.sock.lock"]
        .map(std::ffi::OsString::from)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(entries, expected_entries);
    // The linked name reaches the listener.
    assert!(shepr_platform::ipc::connect_local_stream(&path).is_ok());
    assert!(listener.accept().is_ok());
    // A second server never replaces a socket that is already there.
    let err = shepr_platform::ipc::bind_private_socket(&path)
        .err()
        .expect("startup lock is held");
    assert_eq!(err.kind(), io::ErrorKind::AddrInUse);

    drop(listener);
    drop(startup_lock);
}

#[tokio::test]
async fn host_shutdown_freeze_waits_for_monitor_cancellation() {
    let mut server = test_headless_server();
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(true, Ordering::Release);
    let warned_at = Instant::now();
    server.sync_host_shutdown_freeze(warned_at);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Frozen);

    server.sync_host_shutdown_freeze(warned_at + Duration::from_secs(30));
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Frozen);
    assert!(server.lifecycle.host_shutdown_requested());

    server.sync_host_shutdown_freeze(warned_at + Duration::from_secs(60));
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Frozen);
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(false, Ordering::Release);
    server.sync_host_shutdown_freeze(warned_at + Duration::from_secs(61));
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Running);
    assert!(!server.lifecycle.host_shutdown_requested());
    // No monitor ran before the warning, so none was started by the thaw.
    assert!(server.host_shutdown_monitor.is_none());
}

#[tokio::test]
async fn signal_quit_drain_keeps_dying_panes_in_the_layout() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("signal-quit");
    let pane_id = workspace.tabs()[0].root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server
        .app
        .event_tx
        .try_send(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Exited,
        })
        .expect("test precondition");
    server
        .lifecycle
        .signal_quit_request_flag()
        .store(true, Ordering::Release);
    server.lifecycle.stop_signal().request();

    // The quit-path drain still consumes the queue ...
    let (had_event, _) =
        server.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_CHANNEL_CAPACITY);
    assert!(had_event);
    assert!(server.app.event_rx.try_recv().is_err());
    // ... but the pane stays in the layout the final save captures.
    assert!(server.app.find_pane(pane_id).is_some());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_death_reconciles_each_client_view_and_focus() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("pane-death-views");
    let dead_pane = workspace.tabs()[0].root_pane();
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs()[second_tab].root_pane();
    let (second_runtime, mut second_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.ensure_test_terminals();
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("second tab id");

    let (first_control, _) = connect_test_shell(&mut server, 71, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 72, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(ClientId::test_new(72), &second_tab_id));
    server
        .clients
        .get_mut(&71)
        .expect("test precondition")
        .shell_state_mut()
        .expect("shell state")
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&72)
        .expect("test precondition")
        .shell_state_mut()
        .expect("shell state")
        .outer_terminal_focus = Some(false);

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: shepr_platform::ChildExitReason::Exited
        })
    );

    assert_eq!(
        server
            .shell_tab_id_for_client(ClientId::test_new(71))
            .as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        server
            .shell_tab_id_for_client(ClientId::test_new(72))
            .as_deref(),
        Some(second_tab_id.as_str())
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
        server.clients.geometry_controller(&second_tab_id),
        Some(ClientId::test_new(71))
    );
    let before_resize = server.app.test_runtime(second_pane).current_size();
    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: ClientId::test_new(71),
        surface_cols: 90,
        surface_rows: 25,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        before_resize
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_death_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("pane-death-geometry");
    let first_pane = workspace.tabs()[0].root_pane();
    let dead_pane = workspace.test_split(shepr_core::layout::Direction::Vertical);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        dead_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.set_active_index(Some(0));
    server.app.state.set_selected_index(Some(0));
    server.app.state.mode = crate::app::Mode::Terminal;

    let (control, _) = connect_test_shell(&mut server, 73, 185, 46);
    let _ = control.recv().expect("snapshot");
    let shrunk = server.app.test_runtime(first_pane).current_size();
    assert!(shrunk.0 < 46);

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: shepr_platform::ChildExitReason::Exited
        })
    );

    let runtime = &server.app.test_runtime(first_pane);
    let grown = runtime.current_size();
    assert!(grown.0 > shrunk.0);
    assert_eq!(runtime.terminal_dimensions(), Some((grown.1, grown.0)));
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("test precondition")
            .viewport_rows,
        grown.0 as usize
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
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            20,
            5,
            0,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016h",
            4,
        );
    runtime.resize(shepr_core::geometry::PaneGeometry::new(20, 5, 10, 20));

    apply_client_pane_input_events(
        &runtime,
        &[shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: None,
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
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?1006h",
            4,
        );
    runtime.resize(shepr_core::geometry::PaneGeometry::new(80, 24, 10, 20));

    apply_client_pane_input_events(
        &runtime,
        &[shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Down(shepr_protocol::ClientMouseButton::Left),
            position: shepr_protocol::ClientMousePosition::Pixels {
                x: 403,
                y: 240,
                column: 40,
                row: 12,
            },
            geometry: None,
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
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            20,
            5,
            0,
            b"\x1b[?1003h\x1b[?1006h",
            4,
        );
    runtime.resize(shepr_core::geometry::PaneGeometry::new(20, 5, 10, 20));

    apply_client_pane_input_events(
        &runtime,
        &[shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: None,
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
        geometry: None,
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
            geometry: None,
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 3,
        }],
    )
    .expect("reported mouse motion");
    assert_eq!(
        runtime
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
            geometry: None,
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 3,
        }],
    )
    .expect("mouse button");
    assert_eq!(
        runtime
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
        repeat_count: 1,
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
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
    });
}

#[tokio::test]
async fn headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client() {
    let mut server = test_headless_server();
    // Keep a shell reading its PTY so the resume command cannot race the
    // default test shell, which exits at once, before the input is queued.
    server.app.state.settings.default_shell = shepr_test_support::fixture::idle_shell().into();
    let workspace = shepr_mux::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tabs()[0].root_pane();
    let terminal_id = workspace
        .terminal_id(pane_id)
        .cloned()
        .expect("test precondition");
    server.app.state.workspaces = vec![workspace];
    server.app.state.set_active_index(Some(0));
    server.app.state.ensure_test_terminals();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
        "shepr:codex\0codex\0Id\0codex-session",
        vec![crate::app::exiting_test_command().into()],
    ));

    server.render_and_stream();
    assert_ne!(server.app.state.view.terminal_area, Rect::default());

    let now = Instant::now();
    assert!(!server.handle_scheduled_tasks_headless(now));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
    let deadline = server
        .app
        .pending_agent_resume_deadline
        .expect("clientless resume should wait briefly for a host theme");

    assert!(server.handle_scheduled_tasks_headless(deadline));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_some());
    assert!(
        server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_none()
    );
    shutdown_test_runtimes(&mut server);
}

/// Busy loop iterations (any pane printing keeps a render pending) must not
/// re-arm the theme wait: the first restored agent still launches once the
/// deadline armed on the first tick passes.
#[tokio::test]
async fn headless_scheduled_tasks_keep_pending_agent_resume_deadline_across_ticks() {
    let mut server = test_headless_server();
    // Keep a shell reading its PTY so the resume command cannot race the
    // default test shell, which exits at once, before the input is queued.
    server.app.state.settings.default_shell = shepr_test_support::fixture::idle_shell().into();
    let workspace = shepr_mux::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tabs()[0].root_pane();
    let terminal_id = workspace
        .terminal_id(pane_id)
        .cloned()
        .expect("test precondition");
    server.app.state.workspaces = vec![workspace];
    server.app.state.set_active_index(Some(0));
    server.app.state.ensure_test_terminals();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
        "shepr:codex\0codex\0Id\0codex-session",
        vec![crate::app::exiting_test_command().into()],
    ));
    server.render_and_stream();

    let now = Instant::now();
    assert!(!server.handle_scheduled_tasks_headless(now));
    let deadline = server
        .app
        .pending_agent_resume_deadline
        .expect("clientless resume should arm the theme wait");
    for step in 1..5 {
        let tick = now + Duration::from_millis(step * 100);
        assert!(
            tick < deadline,
            "test ticks must stay inside the theme wait"
        );
        assert!(!server.handle_scheduled_tasks_headless(tick));
        assert_eq!(server.app.pending_agent_resume_deadline, Some(deadline));
    }

    assert!(server.handle_scheduled_tasks_headless(deadline));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_some());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn client_shell_streams_focused_pane_report_all_demand() {
    with_terminal_session_test_server(|server, terminal_id, _terminal_id_string, _pane_id| {
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                1,
                Some(client_tx),
            ),
        );
        server.app.state.set_active_index(Some(0));
        server
            .app
            .terminal_runtimes
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
    let pane_id = server
        .app
        .session_snapshot()
        .focused_pane_id
        .expect("test precondition");
    for client_id in [1, 2] {
        server.clients.insert(
            client_id,
            ClientConnection::new(
                (80, 24),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                client_id,
                None,
            ),
        );
    }
    let key = |kind| shepr_protocol::ClientPaneInputEvent::Key {
        code: shepr_protocol::ClientKeyCode::Char('x'),
        modifiers: shepr_protocol::WireModifiers::NONE,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        // No generated text: the server only holds presses that will get a
        // release, and a key that committed text does not.
        generated_text: None,
    };

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(1),
            pane_id: pane_id.parse().expect("test precondition"),
            events: vec![key(shepr_protocol::ClientKeyKind::Press)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded press").is_empty());
    assert!(server.promote_client_to_foreground(ClientId::test_new(2)));

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(1),
            pane_id: pane_id.parse().expect("test precondition"),
            events: vec![key(shepr_protocol::ClientKeyKind::Release)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded release").is_empty());
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(2))
    );

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: ClientId::test_new(1),
            pane_id: pane_id.parse().expect("test precondition"),
            events: vec![key(shepr_protocol::ClientKeyKind::Press)],
        })
    );
    assert!(
        !input_rx
            .recv()
            .await
            .expect("second encoded press")
            .is_empty()
    );
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected {
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
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            Some(writer),
        ),
    );

    server.stream_host_mouse_capture_mode();
    assert!(matches!(
        read_server_message(control_rx.recv().expect("initial mouse mode")),
        ServerMessage::MouseCapture {
            enabled: false,
            sgr_pixels: false
        }
    ));
    server
        .clients
        .get_mut(&1)
        .expect("shell client")
        .shell_state_mut()
        .expect("shell state")
        .mouse_capture = true;
    server.stream_host_mouse_capture_mode();
    assert!(matches!(
        read_server_message(control_rx.recv().expect("preferred mouse mode")),
        ServerMessage::MouseCapture {
            enabled: true,
            sgr_pixels: false
        }
    ));
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
        server
            .app
            .terminal_runtimes
            .insert(terminal_id.clone(), runtime);
        server.app.state.set_active_index(Some(0));
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                1,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (100, 30),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                2,
                None,
            ),
        );
        server
            .clients
            .set_foreground_client_id(Some(ClientId::test_new(2)));
        server.sync_foreground_client_state();
        assert!(server.claim_unowned_shell_tab_geometry(ClientId::test_new(2), true));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("focused runtime")
                .current_size(),
            (30, 99)
        );

        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: ClientId::test_new(1),
            focused: true,
        }));
        assert_eq!(
            server.clients.foreground_client_id(),
            Some(ClientId::test_new(1))
        );
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("focused runtime")
                .current_size(),
            (24, 79)
        );
        assert_eq!(
            input_rx.try_recv().expect("focus gained input"),
            Bytes::from_static(b"\x1b[I")
        );

        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: ClientId::test_new(2),
            focused: true,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "second viewer duplicated focus gain"
        );
        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: ClientId::test_new(1),
            focused: false,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "remaining viewer lost tab focus"
        );
        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: ClientId::test_new(2),
            focused: false,
        }));
        assert_eq!(server.app.state.outer_terminal_focus, Some(false));
        assert_eq!(
            input_rx.try_recv().expect("last viewer focus lost input"),
            Bytes::from_static(b"\x1b[O")
        );
    });
}

#[test]
fn clipboard_write_targets_foreground_client_only() {
    let mut server = test_headless_server();
    let (background_tx, background_control_rx, _background_rx) = test_client_writer();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::new(
            (120, 40),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            Some(background_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            2,
            Some(foreground_tx),
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(2)));
    server.sync_foreground_client_state();

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
    match read_server_message(
        foreground_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("foreground clipboard message"),
    ) {
        ServerMessage::Clipboard { data } => assert_eq!(data, "dGVzdA=="),
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

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
}

#[test]
fn clipboard_write_failed_foreground_send_removes_client_without_visual_change() {
    let mut server = test_headless_server();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();
    drop(foreground_control_rx);
    foreground_tx.test_close();

    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            Some(foreground_tx),
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(1)));

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
    assert!(
        !server.clients.contains_key(&1),
        "failed targeted send should remove the broken foreground client"
    );
}
