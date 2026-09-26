use super::*;
use bytes::Bytes;

#[path = "pane_move.rs"]
mod pane_move_tests;
#[path = "surface_delta.rs"]
mod surface_delta_tests;
#[path = "surface_interest.rs"]
mod surface_interest_tests;

fn client_shell_projection(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> (
    Box<protocol::ClientShellSnapshot>,
    protocol::endpoint::EndpointAgentCompletions,
) {
    let read_control = |expected| {
        let ServerMessage::EndpointControl { kind, data } = read_server_message(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("endpoint projection"),
        ) else {
            panic!("expected endpoint control {expected}");
        };
        assert_eq!(kind, expected);
        data
    };
    let completions: protocol::endpoint::EndpointAgentCompletions =
        serde_json::from_str(&read_control(protocol::endpoint::AGENT_COMPLETIONS_KIND))
            .expect("test precondition");
    let snapshot: Box<protocol::ClientShellSnapshot> =
        serde_json::from_str(&read_control(protocol::endpoint::ENDPOINT_SNAPSHOT_KIND))
            .expect("test precondition");
    assert_eq!(completions.boot_id, snapshot.boot_id);
    assert_eq!(completions.revision, snapshot.revision);
    (snapshot, completions)
}

fn client_shell_snapshot(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Box<protocol::ClientShellSnapshot> {
    client_shell_projection(receiver).0
}

fn test_headless_server() -> HeadlessServer {
    test_headless_server_with_event_hub(api::EventHub::default())
}

fn test_headless_server_with_event_hub(event_hub: api::EventHub) -> HeadlessServer {
    let config = crate::config::Config::default();
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::new(&config, crate::app::AppPolicy::TEST, api_rx, event_hub);

    app.state.default_shell = crate::app::exiting_test_command().into();
    let dir = std::env::temp_dir().join(format!(
        "hh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::create_dir_all(&dir);
    let socket_path = dir.join("client.sock");
    let _ = fs::remove_file(&socket_path);
    let listener = bind_local_listener(&socket_path).expect("bind test listener");
    let client_socket_identity =
        socket_file_identity(&socket_path).expect("test listener socket identity");
    listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .expect("set listener nonblocking");
    let (server_event_tx, server_event_rx) = mpsc::channel(64);
    let should_quit = Arc::new(AtomicBool::new(false));
    let headless_size = app.state.headless_size;

    HeadlessServer {
        app,
        _api_server: None,
        client_listener: listener,
        client_socket_path: socket_path,
        client_socket_identity,
        clients: HashMap::new(),
        next_client_id: 1,
        foreground_client_id: None,
        tab_geometry_controllers: HashMap::new(),
        client_shell_boot_id: "test-boot".into(),
        sent_window_title: None,
        api_window_title: None,
        server_config_diagnostic: None,
        server_config_diagnostic_without_keybindings: None,
        terminal_attach_owners: HashMap::new(),
        pending_alt_screen_reads: Vec::new(),
        deferred_alt_screen_reads: Vec::new(),
        next_activity_stamp: 1,
        immediate_pty_sources_dirty: true,
        host_input_modes_dirty: true,
        headless_size,
        effective_size: headless_size,
        shutting_down: false,
        host_shutdown_requested: Arc::new(AtomicBool::new(false)),
        host_shutdown_freeze: None,
        host_shutdown_monitor: None,
        signal_quit_requested: Arc::new(AtomicBool::new(false)),
        should_quit,
        server_event_rx,
        server_event_tx,
    }
}

fn shutdown_test_runtimes(server: &mut HeadlessServer) {
    for (_, runtime) in server.app.terminal_runtimes.drain() {
        runtime.shutdown();
    }
}

fn read_server_message(bytes: Vec<u8>) -> ServerMessage {
    let mut cursor = std::io::Cursor::new(bytes);
    protocol::read_message(&mut cursor, MAX_FRAME_SIZE).expect("decode server message")
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

fn read_server_shutdown_reason(bytes: Vec<u8>) -> Option<String> {
    match read_server_message(bytes) {
        ServerMessage::ServerShutdown { reason } => reason,
        other => panic!("expected shutdown, got {other:?}"),
    }
}

#[test]
fn frame_server_message_refuses_payloads_over_the_frame_cap() {
    let small = HeadlessServer::frame_server_message(&ServerMessage::ClientShellError {
        message: "ok".into(),
    })
    .expect("small message frames");
    assert!(matches!(
        read_server_message(small),
        ServerMessage::ClientShellError { message } if message == "ok"
    ));

    let oversized = HeadlessServer::frame_server_message(&ServerMessage::ClientShellError {
        message: "x".repeat(MAX_FRAME_SIZE + 1),
    });
    assert!(matches!(
        oversized,
        Err(protocol::FramingError::Oversized { max, .. }) if max == MAX_FRAME_SIZE
    ));
}

#[test]
fn default_headless_size_is_effective_without_clients() {
    let server = test_headless_server();

    assert_eq!(
        server.headless_size,
        (
            crate::config::DEFAULT_HEADLESS_COLS,
            crate::config::DEFAULT_HEADLESS_ROWS
        )
    );
    assert_eq!(server.effective_size, server.headless_size);
}

#[tokio::test]
async fn headless_api_reads_latest_title_without_spinner_event_flooding() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("one")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.state.sidebar_agents.rows = vec![vec![
        crate::config::AgentSidebarToken::TerminalTitleStripped,
    ]];
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    let terminal_id = server.app.state.workspaces[0].tabs[0].panes[&pane_id]
        .attached_terminal_id
        .clone();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .detected_agent = Some(crate::detect::Agent::Claude);
    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
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
    assert_eq!(pane_updated_events(&event_hub), 1);
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
    assert_eq!(pane_updated_events(&event_hub), 1);
}

fn headless_pane_list(server: &mut HeadlessServer) -> Vec<api::schema::PaneInfo> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "list-titles".into(),
            method: api::schema::Method::PaneList(api::schema::PaneListParams::default()),
        },
        respond_to,
    });
    let response: api::schema::SuccessResponse =
        serde_json::from_str(&response_rx.recv().expect("test precondition"))
            .expect("test precondition");
    let api::schema::ResponseResult::PaneList { panes } = response.result else {
        panic!("expected pane list");
    };
    panes
}

fn pane_updated_events(event_hub: &api::EventHub) -> usize {
    event_hub
        .events_after(0)
        .iter()
        .filter(|(_, event)| event.event == api::schema::EventKind::PaneUpdated)
        .count()
}

#[test]
fn server_stop_interrupts_server_event_backlog() {
    let mut server = test_headless_server();
    for client_id in 1..=64 {
        server
            .server_event_tx
            .try_send(ServerEvent::ClientDisconnected { client_id })
            .expect("test precondition");
    }

    server.should_quit.store(true, Ordering::Release);

    assert!(!server.drain_server_events());
    assert!(server.server_event_rx.try_recv().is_ok());
    shutdown_test_runtimes(&mut server);
}

fn shutdown_test_request(id: &str) -> (api::ApiRequestMessage, std::sync::mpsc::Receiver<String>) {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    (
        api::ApiRequestMessage {
            request: api::schema::Request {
                id: id.into(),
                method: api::schema::Method::ServerStop(api::schema::EmptyParams::default()),
            },
            respond_to,
        },
        response_rx,
    )
}

fn assert_server_unavailable(response_rx: &std::sync::mpsc::Receiver<String>, id: &str) {
    let response = response_rx
        .try_recv()
        .expect("shutdown must answer the request, not drop it");
    let response: serde_json::Value = serde_json::from_str(&response).expect("json response");
    assert_eq!(response["id"], id);
    assert_eq!(response["error"]["code"], "server_unavailable");
}

#[tokio::test]
async fn complete_shutdown_answers_queued_and_deferred_requests_and_closes_the_channel() {
    let mut server = test_headless_server();
    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    server.app.api_rx = api_rx;

    let (queued, queued_rx) = shutdown_test_request("queued");
    api_tx.send(queued).expect("test precondition");
    let (deferred, deferred_rx) = shutdown_test_request("deferred");
    server.deferred_alt_screen_reads.push(deferred);

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown completes");

    assert_server_unavailable(&queued_rx, "queued");
    assert_server_unavailable(&deferred_rx, "deferred");
    assert!(server.deferred_alt_screen_reads.is_empty());
    // A request dispatched after cleanup fails at the sender, which the API
    // thread turns into `server_unavailable` at once.
    let (late, _late_rx) = shutdown_test_request("late");
    assert!(api_tx.send(late).is_err());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn api_request_selected_during_shutdown_is_answered() {
    let (request, response_rx) = shutdown_test_request("selected");
    HeadlessServer::reject_api_request_for_shutdown(request);
    assert_server_unavailable(&response_rx, "selected");
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
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "headless_list_after_events".into(),
            method: api::schema::Method::WorkspaceList(api::schema::EmptyParams::default()),
        },
        respond_to,
    });
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("test precondition");
    let response: serde_json::Value = serde_json::from_str(&response).expect("test precondition");

    assert_eq!(response["result"]["type"], "workspace_list");
    assert!(server.app.event_rx.try_recv().is_err());
}

fn window_title_test_server() -> (HeadlessServer, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("herd")];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.promote_client_to_foreground(1);
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
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("herd")];
    server.app.state.active = Some(0);
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
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.promote_client_to_foreground(1);
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

    // ClientConnected assigns the foreground client directly rather than
    // going through promote_client_to_foreground, so the cache must notice
    // the new client on its own.
    let (client_tx, second_control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(2);
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

    server.app.state.workspaces[0].tabs[0].custom_name = Some("build".into());
    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd/build".to_string()))
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn focused_terminal_title_syncs_without_requesting_a_sidebar_render() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("{terminal_title}");
    server.app.state.ensure_test_terminals();
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(pane_id)
        .expect("terminal")
        .clone();
    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes("\x1b]0;⠋ building\x07".as_bytes());
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);

    assert_eq!(
        server.sync_terminal_title_sources(&HashSet::from([pane_id])),
        (false, true)
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
        (false, true)
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
        1,
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
fn api_window_title_wins_until_it_is_cleared() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}");

    server.handle_client_window_title_api("set".into(), Some("shepr api".into()));
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("shepr api".to_string()))
    );

    server.app.state.workspaces[0].custom_name = Some("ops".into());
    server.sync_window_title();
    assert!(no_window_title(&control_rx));

    // Clearing hands the title back to ui.window_title, not to "shepr".
    server.handle_client_window_title_api("clear".into(), None);
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("ops".to_string()))
    );

    shutdown_test_runtimes(&mut server);
}

#[test]
fn clearing_the_api_title_falls_back_to_shepr_when_window_titles_are_disabled() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("");

    server.handle_client_window_title_api("set".into(), Some("shepr api".into()));
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("shepr api".to_string()))
    );

    server.handle_client_window_title_api("clear".into(), None);
    assert_eq!(next_window_title(&control_rx), Some(None));

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
            crate::terminal_cell_size::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.promote_client_to_foreground(2);
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
    let mut survivor = crate::workspace::Workspace::test_new("survivor");
    let survivor_tab_index = survivor.test_add_tab(Some("survivor-tab"));
    let survivor_pane = survivor.tabs[survivor_tab_index].layout.focused();
    let disconnected = crate::workspace::Workspace::test_new("disconnected");
    let disconnected_pane = disconnected.tabs[0].layout.focused();
    server.app.state.workspaces = vec![survivor, disconnected];
    server.app.state.active = Some(1);
    server.app.state.selected = 1;
    server.app.state.ensure_test_terminals();
    let survivor_terminal = server.app.state.workspaces[0].tabs[survivor_tab_index]
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
    assert!(server.focus_shell_client_on_tab(1, &survivor_tab_id));
    server.promote_client_to_foreground(2);
    drain_window_titles(&survivor_control);
    drain_window_titles(&disconnected_control);

    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 2 }));
    server.sync_window_title();

    assert_eq!(
        next_window_title(&survivor_control),
        Some(Some("survivor/survivor-tab/client-pane/CLIENT OSC".into()))
    );
    assert_eq!(server.app.state.active, Some(1));
    assert_eq!(
        server.shell_tab_id_for_client(1).as_deref(),
        Some(survivor_tab_id.as_str())
    );

    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
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

fn test_client_writer() -> (
    ClientWriter,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (control_tx, control_rx) = std::sync::mpsc::channel();
    let (render_tx, render_rx) = std::sync::mpsc::sync_channel(1);
    (
        ClientWriter::test_channel(control_tx, render_tx),
        control_rx,
        render_rx,
    )
}

#[tokio::test]
async fn client_shell_attach_seeds_workspace() {
    let mut server = test_headless_server();
    server.app.state.workspaces.clear();
    server.app.state.active = None;
    server.app.state.mode = crate::app::Mode::Navigate;
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 6,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );

    assert_eq!(server.app.state.mode, crate::app::Mode::Terminal);
    assert_eq!(server.app.state.workspaces.len(), 1);
    assert_eq!(server.app.state.active, Some(0));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn completion_guard_endpoint_pairs_runtime_completions_with_snapshots() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("endpoint");
    let pane_id = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        client_id: 78,
        surface_cols: 80,
        surface_rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        endpoint_keybindings: false,
        mouse_capture: false,
        surface_active: false,
        writer,
    });
    let (_, initial) = client_shell_projection(&control_rx);
    assert!(initial.completions.is_empty());
    for first_state in [
        crate::detect::AgentState::Working,
        crate::detect::AgentState::Unknown,
    ] {
        for state in [first_state, crate::detect::AgentState::Idle] {
            server.app.state.handle_app_event(AppEvent::StateChanged {
                pane_id,
                agent: Some(crate::detect::Agent::Pi),
                state,
                visible_blocker: false,
                visible_working: state == crate::detect::AgentState::Working,
                process_exited: false,
                observed_at: Instant::now(),
            });
        }
        server.render_and_stream();
        let (snapshot, completions) = client_shell_projection(&control_rx);
        assert_eq!(snapshot.agents.len(), 1);
        let agent = &snapshot.agents[0];
        let expected =
            (first_state == crate::detect::AgentState::Working).then_some(agent.state_change_seq);
        assert_eq!(
            completions.completions.get(&agent.pane_id).copied(),
            expected
        );
        assert_eq!(
            server.app.session_snapshot().agents[0].completion_seq,
            expected
        );
    }
}

#[tokio::test]
async fn client_shell_endpoint_request_uses_the_selected_connection() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("endpoint")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = 41;
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let _initial_snapshot = client_shell_snapshot(&control_rx);
    let boot_id = server.client_shell_boot_id.clone();
    let rename = || {
        api::schema::Method::WorkspaceRename(api::schema::WorkspaceRenameParams {
            workspace_id: server.app.state.workspaces[0].id.clone(),
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
            request: Box::new(api::schema::Request {
                id: "client-shell:1".into(),
                method: first_rename,
            }),
        })
    );
    assert!(server.clients[&client_id].shell_endpoint_command_in_flight);

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
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
    let response =
        serde_json::from_slice::<api::schema::ErrorResponse>(&data).expect("typed busy response");
    assert_eq!(response.error.code, "endpoint_busy");

    let response_ready = server
        .server_event_rx
        .recv()
        .await
        .expect("endpoint response ready");
    assert!(!server.handle_server_event(response_ready));
    assert!(!server.clients[&client_id].shell_endpoint_command_in_flight);

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
            let response = serde_json::from_slice::<api::schema::SuccessResponse>(&data)
                .expect("success response");
            assert_eq!(response.id, "client-shell:1");
            assert!(matches!(
                response.result,
                api::schema::ResponseResult::WorkspaceInfo { .. }
            ));
        }
        other => panic!("expected client shell endpoint response, got {other:?}"),
    }
    shutdown_test_runtimes(&mut server);
}

#[test]
fn terminal_client_endpoint_request_error_removes_client() {
    let mut server = test_headless_server();
    let (writer, _control_rx, _render_rx) = test_client_writer();
    let client_id = 42;
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        writer,
    }));

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequestError {
            client_id,
            boot_id: "boot".into(),
            request_id: "request".into(),
            code: "unsupported_method",
            message: "unsupported".into(),
        })
    );
    assert!(!server.clients.contains_key(&client_id));
}

#[tokio::test]
async fn client_shell_receives_metadata_then_shell_free_pane_surface() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("shell-only-label");
    let pane_id = workspace.focused_pane_id().expect("focused pane");

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(
            80,
            23,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016hCLIENT_SHELL_LIVE",
        ),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.server_config_diagnostic_without_keybindings = Some("endpoint config warning".into());

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 7,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 10,
            cell_height_px: 20,
            pixel_mouse: true,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let snapshot = client_shell_snapshot(&control_rx);
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].label, "shell-only-label");
    assert_eq!(
        snapshot.config_diagnostic.as_deref(),
        Some("endpoint config warning")
    );
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
        ServerMessage::PaneSurfacePatch(patch) => {
            assert_eq!(
                patch.base_surface_revision,
                initial_surface.surface_revision
            );
            assert_eq!(patch.surface_revision, initial_surface.surface_revision + 1);
            assert_eq!(patch.panes.len(), 1);
            assert!(!patch.rows.is_empty());
            assert!(
                patch
                    .rows
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
        ServerMessage::PaneSurfacePatch(patch) => {
            assert!(patch.rows.is_empty(), "mouse modes only change metadata");
            assert_eq!(patch.panes.len(), 1);
            assert!(!patch.panes[0].mouse_reporting);
            assert!(!patch.panes[0].sgr_pixel_mouse);
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

fn install_shared_view_test_runtime(server: &mut HeadlessServer) -> crate::layout::PaneId {
    let workspace = crate::workspace::Workspace::test_new("shared-view");
    let pane_id = workspace.focused_pane_id().expect("focused pane");

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"BASE"),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    pane_id
}

fn connect_test_shell(
    server: &mut HeadlessServer,
    client_id: u64,
    surface_cols: u16,
    surface_rows: u16,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id,
            surface_cols,
            surface_rows,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            endpoint_keybindings: false,
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
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    connect_test_shell(server, client_id, 80, 23)
}

fn write_shared_test_pane(
    server: &mut HeadlessServer,
    pane_id: crate::layout::PaneId,
    bytes: &[u8],
) {
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(bytes);
}

/// Pairs a render receiver with the decoder that unwraps its surface reuse
/// and delta messages: the server encodes those against the last full
/// surface it sent on that connection, so decoding them here needs the same
/// running baseline a real endpoint client would keep.
struct PaneSurfaceReceiver {
    receiver: std::sync::mpsc::Receiver<Vec<u8>>,
    decoder: crate::protocol::surface_reuse::Decoder,
}

impl PaneSurfaceReceiver {
    fn new(receiver: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            decoder: crate::protocol::surface_reuse::Decoder::new(true),
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
) -> crate::protocol::PaneSurfaceFrame {
    match receiver.recv(context) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("{context}: expected pane surface, got {other:?}"),
    }
}

fn recv_pane_surface_patch(
    receiver: &mut PaneSurfaceReceiver,
    context: &str,
) -> crate::protocol::PaneSurfacePatch {
    match receiver.recv(context) {
        ServerMessage::PaneSurfacePatch(patch) => patch,
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
    let projection_before = server.clients[&7].shell_projection_revision;

    write_shared_test_pane(
        &mut server,
        pane_id,
        b"\x1b[?2026h\x1b[?1049h\x1b[2J\x1b[HPARTIAL",
    );
    server.app.state.workspaces[0].custom_name = Some("renamed during frame".into());
    server
        .clients
        .get_mut(&7)
        .expect("test precondition")
        .request_recompute();
    server.render_and_stream();
    assert!(render.try_recv().is_err(), "partial frame was published");
    assert_eq!(
        server.clients[&7].shell_projection_revision,
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
    let mut workspace = crate::workspace::Workspace::test_new("synchronized-split");
    let first = workspace.tabs[0].root_pane;
    let second = workspace.test_split(ratatui::layout::Direction::Vertical);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
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
    let mut workspace = crate::workspace::Workspace::test_new("zoomed-sync");
    let hidden = workspace.tabs[0].root_pane;
    let visible = workspace.test_split(ratatui::layout::Direction::Vertical);
    workspace.tabs[0].zoomed = true;

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        hidden,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"HIDDEN"),
    );
    server.app.insert_test_runtime(
        visible,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"VISIBLE"),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
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
    assert_eq!(patch.panes[0].content_revision, revision);
    assert!(revision.is_multiple_of(2));
    assert!(patch.panes[0].mouse_reporting);
    let surface = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("surface");
    assert!(frame_text(&surface.frame).contains("AAAA"));
    assert!(!frame_text(&surface.frame).contains("BBBB"));

    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    let next = recv_pane_surface_patch(&mut render, "waiting write remains dirty");
    assert_eq!(next.panes[0].content_revision, revision + 2);
    assert!(!next.panes[0].mouse_reporting);
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
    assert!(large_patch.rows.iter().all(|row| {
        row.x
            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
            <= large_initial.frame.width
            && row.y < large_initial.frame.height
    }));
    assert!(small_patch.rows.iter().all(|row| {
        row.x
            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
            <= small_initial.frame.width
            && row.y < small_initial.frame.height
    }));
    assert_eq!(large_patch.rows, small_patch.rows);
    assert_ne!(
        large_patch.panes[0].inner_rect,
        small_patch.panes[0].inner_rect
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
    let mut workspace = crate::workspace::Workspace::test_new("divergent-retained");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
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
    assert!(server.focus_shell_client_on_tab(8, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(8, false));
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
    assert_eq!(first_patch.panes.len(), 1);
    assert!(second_render.try_recv().is_err());

    server
        .app
        .test_runtime(second_pane)
        .test_process_pty_bytes(b"\rSECOND_PATCH");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([second_pane])));
    let second_patch = recv_pane_surface_patch(&mut second_render, "second patch");
    assert_eq!(second_patch.panes.len(), 1);
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
    server.foreground_client_id = Some(8);
    let crate::server::render_stream::ClientRenderState::Semantic { last_surface, .. } =
        &mut server
            .clients
            .get_mut(&8)
            .expect("test precondition")
            .render_state
    else {
        panic!("semantic client");
    };
    let linked = last_surface.as_mut().expect("test precondition");
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
        ServerMessage::PaneSurfacePatch(_)
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
        ServerMessage::PaneSurfacePatch(_)
    ));
    assert_eq!(server.clients[&8].deferred_render(), DeferredRender::Full);
    assert_eq!(
        server.clients[&8].render_state.last_pane_surface(),
        Some(&slow_baseline),
        "queue-full must not advance cells, metadata, cursor, or revision"
    );

    write_shared_test_pane(&mut server, pane_id, b"\rTHREE");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive third patch")),
        ServerMessage::PaneSurfacePatch(_)
    ));

    assert!(matches!(
        slow_render.recv("slow queued first patch"),
        ServerMessage::PaneSurfacePatch(_)
    ));
    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 8 }));
    server.render_and_stream();
    assert!(matches!(
        slow_render.recv("slow full recovery surface"),
        ServerMessage::PaneSurface(_)
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
    assert_eq!(server.clients[&8].deferred_render(), DeferredRender::Full);
    assert!(!server.app.full_redraw_pending);

    write_shared_test_pane(&mut server, pane_id, b"\rPATCH");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive retained patch")),
        ServerMessage::PaneSurfacePatch(_)
    ));

    let _ = slow_render.recv().expect("slow queued initial surface");
    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 8 }));
    server.render_and_stream();
    assert!(matches!(
        read_server_message(slow_render.recv().expect("slow full recovery surface")),
        ServerMessage::PaneSurface(_)
    ));

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_config_diagnostics_follow_keybinding_ownership() {
    let mut server = test_headless_server();
    server.server_config_diagnostic = Some("server keybinding warning\ntheme warning".into());
    server.server_config_diagnostic_without_keybindings = Some("theme warning".into());

    let (local_writer, local_control, _local_render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 13,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer: local_writer,
        })
    );
    let local_snapshot = client_shell_snapshot(&local_control);
    assert_eq!(
        local_snapshot.config_diagnostic.as_deref(),
        Some("theme warning")
    );

    let (endpoint_writer, endpoint_control, _endpoint_render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 14,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            endpoint_keybindings: true,
            mouse_capture: false,
            surface_active: true,
            writer: endpoint_writer,
        })
    );
    let endpoint_snapshot = client_shell_snapshot(&endpoint_control);
    assert_eq!(
        endpoint_snapshot.config_diagnostic.as_deref(),
        Some("server keybinding warning\ntheme warning")
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_tab_focus_changes_only_the_source_connection() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("independent-tabs");
    let second_tab = workspace.test_add_tab(Some("second"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
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
            client_id: 8,
            boot_id: server.client_shell_boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "focus-second".into(),
                method: api::schema::Method::TabFocus(api::schema::TabTarget {
                    tab_id: second_tab_id.clone(),
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
async fn client_local_navigation_does_not_emit_global_focus_transitions() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("independent-focus");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let (first_runtime, mut first_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(first_pane, first_runtime);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(62, &second_tab_id));
    server
        .clients
        .get_mut(&61)
        .expect("test precondition")
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&62)
        .expect("test precondition")
        .outer_terminal_focus = Some(true);

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_client_shell_api_request(
        62,
        crate::api::ApiRequestMessage {
            request: crate::api::schema::Request {
                id: "focus-own-tab".into(),
                method: crate::api::schema::Method::TabFocus(crate::api::schema::TabTarget {
                    tab_id: second_tab_id,
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
async fn client_local_navigation_emits_pane_focused_only_when_that_client_moves() {
    use api::schema::{EventData, Method, PaneTarget, TabTarget};

    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    let mut workspace = crate::workspace::Workspace::test_new("focus-events");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
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
    let workspace_id = server.app.public_workspace_id(0);

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    let first_tab = TabTarget {
        tab_id: first_tab_id,
    };
    let second_tab = TabTarget {
        tab_id: second_tab_id,
    };
    let cases = [
        (
            61,
            Method::TabFocus(second_tab.clone()),
            Some(&second_pane_id),
        ),
        // Both clients selecting the same destination must each emit an event.
        (
            62,
            Method::TabFocus(second_tab.clone()),
            Some(&second_pane_id),
        ),
        (61, Method::TabFocus(second_tab.clone()), None),
        (61, Method::TabFocus(first_tab), Some(&first_pane_id)),
        // Switching the server's default to this client's unchanged tab is not navigation.
        (
            62,
            Method::PaneFocus(PaneTarget {
                pane_id: second_pane_id.clone(),
            }),
            None,
        ),
        (
            62,
            Method::PaneFocus(PaneTarget {
                pane_id: first_pane_id.clone(),
            }),
            Some(&first_pane_id),
        ),
        (
            61,
            Method::TabFocus(second_tab.clone()),
            Some(&second_pane_id),
        ),
        (61, Method::TabClose(second_tab), Some(&first_pane_id)),
    ];
    for (client_id, method, expected_pane) in cases {
        let other_client = if client_id == 61 { 62 } else { 61 };
        let other_focus = server.shell_focus_target(other_client);
        let sequence = event_hub.current_sequence();
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        server.handle_client_shell_api_request(
            client_id,
            api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "navigate".into(),
                    method,
                },
                respond_to,
            },
        );
        let response = response_rx.recv().expect("navigation response");
        assert!(
            serde_json::from_str::<api::schema::SuccessResponse>(&response).is_ok(),
            "{response}"
        );
        server.app.sync_focus_events();

        let focused = event_hub
            .events_after(sequence)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                EventData::PaneFocused {
                    pane_id,
                    workspace_id,
                } => Some((pane_id, workspace_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let expected = expected_pane
            .map(|pane_id| (pane_id.clone(), workspace_id.clone()))
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(focused, expected, "client {client_id}");
        assert_eq!(server.shell_focus_target(other_client), other_focus);
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_focus_moves_shell_focus_between_tabs() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("public-focus-events");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let (first_runtime, mut first_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(first_pane, first_runtime);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("second tab id");

    let (first_control, _) = connect_matching_test_shell(&mut server, 63);
    let (second_control, _) = connect_matching_test_shell(&mut server, 64);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(64, &second_tab_id));
    server
        .clients
        .get_mut(&63)
        .expect("test precondition")
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&64)
        .expect("test precondition")
        .outer_terminal_focus = Some(true);
    assert!(server.app.state.switch_workspace_tab(0, second_tab));

    server.focus_all_shell_clients_on_default_target();

    assert_eq!(
        server.shell_tab_id_for_client(63).as_deref(),
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
    let mut workspace = crate::workspace::Workspace::test_new("layout-geometry");
    let first_pane = workspace.tabs[0].root_pane;
    let second_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let tab_id = server.app.public_tab_id(0, 0).expect("tab id");

    let (control, _) = connect_test_shell(&mut server, 65, 100, 30);
    let _ = control.recv().expect("snapshot");
    let before = server.app.test_runtime(first_pane).current_size();
    let (respond_to, _response_rx) = std::sync::mpsc::channel();

    assert!(server.handle_client_shell_api_request(
        65,
        crate::api::ApiRequestMessage {
            request: crate::api::schema::Request {
                id: "resize-layout".into(),
                method: crate::api::schema::Method::LayoutSetSplitRatio(
                    crate::api::schema::LayoutSetSplitRatioParams {
                        tab_id: Some(tab_id),
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
    let mut workspace = crate::workspace::Workspace::test_new("public-close-geometry");
    let first_pane = workspace.tabs[0].root_pane;
    let second_pane = workspace.test_split(ratatui::layout::Direction::Vertical);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
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
        server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
            request: crate::api::schema::Request {
                id: "public-close-geometry".into(),
                method: crate::api::schema::Method::PaneClose(crate::api::schema::PaneTarget {
                    pane_id: second_pane_id,
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
    let mut workspace = crate::workspace::Workspace::test_new("geometry-controller-viewer");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let third_tab = workspace.test_add_tab(Some("third"));
    let third_pane = workspace.tabs[third_tab].root_pane;
    server.app.state.workspaces = vec![workspace];
    for pane_id in [first_pane, second_pane, third_pane] {
        server.app.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
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

    assert!(server.focus_shell_client_on_tab(67, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(67, false));
    assert!(server.focus_shell_client_on_tab(67, &third_tab_id));
    assert!(server.claim_shell_tab_geometry(67, false));
    assert!(server.focus_shell_client_on_tab(68, &second_tab_id));
    assert_eq!(
        server.tab_geometry_controllers.get(&second_tab_id),
        Some(&67)
    );
    let stale_size = server.app.test_runtime(second_pane).current_size();

    assert!(server.reapply_controlled_shell_tab_geometry(false));

    assert_eq!(
        server.tab_geometry_controllers.get(&second_tab_id),
        Some(&68)
    );
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        stale_size
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_tabs_render_accept_input_and_resize_independently() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("independent-geometry");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;

    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"SECOND_TAB",
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"FIRST_TAB"),
    );
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
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

    assert!(server.focus_shell_client_on_tab(22, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(22, false));
    let second_size = server.app.test_runtime(second_pane).current_size();
    assert_ne!(first_size, second_size);
    assert_eq!(
        server.app.test_runtime(first_pane).current_size(),
        first_size
    );

    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: 22,
        pane_id: second_pane_id,
        events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
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
        client_id: 22,
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

    assert!(server.focus_shell_client_on_tab(21, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(21, false));
    assert_ne!(
        server.app.test_runtime(second_pane).current_size(),
        resized_second
    );

    server.remove_client_and_resize_if_needed(21);
    let singleton_first = server.app.test_runtime(first_pane).current_size();
    let singleton_second = server.app.test_runtime(second_pane).current_size();
    assert_ne!(singleton_first, first_size);
    assert_eq!(singleton_first, singleton_second);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_background_tab_create_preserves_client_locations() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("background-create");
    let second_tab = workspace.test_add_tab(Some("second"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let workspace_id = server.app.public_workspace_id(0);
    let first_tab_id = server.app.public_tab_id(0, 0).expect("test precondition");
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");

    let (first_control, _) = connect_matching_test_shell(&mut server, 71);
    let (second_control, _) = connect_matching_test_shell(&mut server, 72);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(71, &second_tab_id));

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
        request: crate::api::schema::Request {
            id: "create-background-tab".into(),
            method: crate::api::schema::Method::TabCreate(crate::api::schema::TabCreateParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                focus: false,
                label: Some("background".into()),
                env: std::collections::HashMap::new(),
            }),
        },
        respond_to,
    });

    assert_eq!(
        server.shell_tab_id_for_client(71).as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        server.shell_tab_id_for_client(72).as_deref(),
        Some(first_tab_id.as_str())
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_workspace_focus_preserves_each_clients_remembered_tabs() {
    let mut server = test_headless_server();
    let mut first = crate::workspace::Workspace::test_new("first");
    let second_tab = first.test_add_tab(Some("second"));
    let second = crate::workspace::Workspace::test_new("second");
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_workspace_id = server.app.public_workspace_id(0);
    let second_workspace_id = server.app.public_workspace_id(1);
    let first_tab_id = server.app.public_tab_id(0, 0).expect("test precondition");
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("test precondition");

    let (first_control, _) = connect_test_shell(&mut server, 41, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 42, 80, 24);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(41, &second_tab_id));

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
        request: crate::api::schema::Request {
            id: "focus-second-workspace".into(),
            method: crate::api::schema::Method::WorkspaceFocus(
                crate::api::schema::WorkspaceTarget {
                    workspace_id: second_workspace_id.clone(),
                },
            ),
        },
        respond_to,
    });

    let first_location = server.clients[&41]
        .shell_location
        .as_ref()
        .expect("test precondition");
    let second_location = server.clients[&42]
        .shell_location
        .as_ref()
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
        first_location.active_tab_ids[&first_workspace_id],
        second_tab_id
    );
    assert_eq!(
        second_location.active_tab_ids[&first_workspace_id],
        first_tab_id
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_agent_focus_replaces_a_diverged_client_shell_projection() {
    let mut server = test_headless_server();
    let first = crate::workspace::Workspace::test_new("first");
    let first_pane = first.tabs[0].root_pane;

    let second = crate::workspace::Workspace::test_new("second");
    let second_pane = second.tabs[0].root_pane;

    server.app.state.workspaces = vec![first, second];
    server.app.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"FIRST_AGENT"),
    );
    server.app.insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"SECOND_WORKSPACE"),
    );
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_workspace_id = server.app.public_workspace_id(0);
    let first_tab_id = server.app.public_tab_id(0, 0).expect("test precondition");
    let first_pane_id = server
        .app
        .public_pane_id(0, first_pane)
        .expect("test precondition");
    let second_tab_id = server.app.public_tab_id(1, 0).expect("test precondition");

    let (control_rx, render_rx) = connect_test_shell(&mut server, 9, 80, 23);
    let mut render_rx = PaneSurfaceReceiver::new(render_rx);
    let _ = client_shell_snapshot(&control_rx);
    assert!(server.focus_shell_client_on_tab(9, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(9, false));
    server.render_and_stream();
    let diverged = client_shell_snapshot(&control_rx);
    assert_eq!(
        diverged.focused_workspace_id.as_deref(),
        Some(server.app.public_workspace_id(1).as_str())
    );
    let diverged_surface = recv_pane_surface(&mut render_rx, "diverged surface");
    assert!(frame_text(&diverged_surface.frame).contains("SECOND_WORKSPACE"));

    server
        .app
        .event_tx
        .try_send(AppEvent::AgentProcessDetected {
            pane_id: first_pane,
            agent: crate::detect::Agent::Claude,
            observed_at: Instant::now(),
        })
        .expect("test precondition");
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
        request: crate::api::schema::Request {
            id: "focus-first-agent".into(),
            method: crate::api::schema::Method::AgentFocus(crate::api::schema::AgentTarget {
                target: first_pane_id.clone(),
            }),
        },
        respond_to,
    });
    let response: crate::api::schema::SuccessResponse =
        serde_json::from_str(&response_rx.recv().expect("agent focus response"))
            .expect("test precondition");
    let crate::api::schema::ResponseResult::AgentInfo { agent } = response.result else {
        panic!("expected agent info");
    };
    assert_eq!(agent.pane_id, first_pane_id);
    assert!(agent.focused);
    assert_eq!(server.app.state.active, Some(0));
    let location = server.clients[&9]
        .shell_location
        .as_ref()
        .expect("test precondition");
    assert_eq!(
        location.focused_workspace_id.as_deref(),
        Some(first_workspace_id.as_str())
    );
    assert_eq!(location.focused_tab_id(), Some(first_tab_id.as_str()));

    server.render_and_stream();
    let replacement = client_shell_snapshot(&control_rx);
    assert_eq!(
        replacement.focused_workspace_id.as_deref(),
        Some(first_workspace_id.as_str())
    );
    let replacement_surface = recv_pane_surface(&mut render_rx, "agent focus replacement surface");
    assert!(frame_text(&replacement_surface.frame).contains("FIRST_AGENT"));
    assert!(!frame_text(&replacement_surface.frame).contains("SECOND_WORKSPACE"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_api_focus_replaces_every_client_shell_projection() {
    let mut server = test_headless_server();
    let first = crate::workspace::Workspace::test_new("first");
    let second = crate::workspace::Workspace::test_new("second");
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_id = server.app.session_snapshot().workspaces[1]
        .workspace_id
        .clone();

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 9,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let initial_revision = client_shell_snapshot(&control_rx).revision;

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
        request: crate::api::schema::Request {
            id: "test.client.shell.workspace.focus".into(),
            method: crate::api::schema::Method::WorkspaceFocus(
                crate::api::schema::WorkspaceTarget {
                    workspace_id: second_id.clone(),
                },
            ),
        },
        respond_to,
    });
    assert_eq!(server.app.state.active, Some(1));
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
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id,
            events: vec![
                crate::protocol::ClientPaneInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('c'),
                    modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
                    kind: crate::protocol::ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                    tracks_release: true,
                    physical_key_id: None,
                    windows_record: None,
                },
                crate::protocol::ClientPaneInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('c'),
                    modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
                    kind: crate::protocol::ClientKeyKind::Release,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                    tracks_release: true,
                    physical_key_id: None,
                    windows_record: None,
                },
                crate::protocol::ClientPaneInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('x'),
                    modifiers: crossterm::event::KeyModifiers::ALT.bits(),
                    kind: crate::protocol::ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                    tracks_release: true,
                    physical_key_id: None,
                    windows_record: None,
                },
                crate::protocol::ClientPaneInputEvent::Mouse {
                    kind: crate::protocol::ClientMouseKind::Down(
                        crate::protocol::ClientMouseButton::Left,
                    ),
                    position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                    geometry: None,
                    modifiers: 0,
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
    assert_eq!(server.foreground_client_id, Some(11));
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
    let mut workspace = crate::workspace::Workspace::test_new("hidden-input");
    let hidden_tab = workspace.test_add_tab(Some("hidden"));
    let hidden_pane = workspace.tabs[hidden_tab].root_pane;
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[>3u",
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(hidden_pane, runtime);
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let pane_id = server
        .app
        .public_pane_id(0, hidden_pane)
        .expect("test precondition");
    server.clients.insert(
        11,
        ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    let key = |kind| crate::protocol::ClientPaneInputEvent::Key {
        code: crate::protocol::ClientKeyCode::Char('x'),
        modifiers: 0,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: true,
        physical_key_id: Some(0x2d),
        windows_record: None,
    };

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id: pane_id.clone(),
            events: vec![key(crate::protocol::ClientKeyKind::Press)],
        })
    );
    assert!(input_rx.try_recv().is_err());
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id,
            events: vec![key(crate::protocol::ClientKeyKind::Release)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded release").is_empty());
    assert_eq!(server.foreground_client_id, None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_text_input_renders_only_when_resetting_scrollback() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("scrolled-input");
    let pane_id = workspace.tabs[0].root_pane;
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
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
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let public_pane_id = server
        .app
        .public_pane_id(0, pane_id)
        .expect("test precondition");
    server.clients.insert(
        11,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(11);

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id: public_pane_id.clone(),
            events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
                "x".to_owned(),
            )],
        });

    assert_eq!(render_impact, RenderImpact::Full);
    assert_eq!(
        input_rx.try_recv().expect("text must reach the PTY"),
        Bytes::from_static(b"x")
    );
    assert_eq!(
        server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .and_then(crate::terminal::TerminalRuntime::scroll_metrics)
            .map(|metrics| metrics.offset_from_bottom),
        Some(0)
    );

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id: public_pane_id,
            events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
                "y".to_owned(),
            )],
        });
    assert_eq!(render_impact, RenderImpact::None);
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
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(11);
    assert!(server.claim_unowned_shell_tab_geometry(11, false));

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id,
            events: vec![crate::protocol::ClientPaneInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                geometry: None,
                modifiers: 0,
                lines: 0,
            }],
        });

    assert_eq!(render_impact, RenderImpact::None);
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
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id,
            events: vec![crate::protocol::ClientPaneInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                geometry: None,
                modifiers: 0,
                lines: 0,
            }],
        });

    assert_eq!(render_impact, RenderImpact::Full);
    assert_eq!(server.foreground_client_id, Some(11));
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
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(writer),
        ),
    );
    server.foreground_client_id = Some(11);

    let events = ["a", "b", "c", "d", "e", "f"]
        .into_iter()
        .map(|text| crate::protocol::ClientPaneInputEvent::TextCommit(text.to_owned()))
        .collect();
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: 11,
        pane_id: pane_id.clone(),
        events,
    });

    for expected in ["a", "b", "c", "d"] {
        assert_eq!(
            input_rx.try_recv().expect("queued input"),
            Bytes::from(expected)
        );
    }
    let message = loop {
        if let ServerMessage::ClientShellError { message } = read_server_message(
            control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("dropped-input error"),
        ) {
            break message;
        }
    };
    assert!(message.contains(&pane_id), "message: {message}");
    assert!(message.contains("2 events"), "message: {message}");
    shutdown_test_runtimes(&mut server);
}

fn install_focused_test_runtime(
    server: &mut HeadlessServer,
    terminal_bytes: &[u8],
) -> tokio::sync::mpsc::Receiver<Bytes> {
    let workspace = crate::workspace::Workspace::test_new("focus-reporting");
    let pane_id = workspace.tabs[0].root_pane;
    let (runtime, input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            terminal_bytes,
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    input_rx
}

fn retained_test_server_with_control(
    initial_screen: &[u8],
) -> (
    HeadlessServer,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
    crate::layout::PaneId,
) {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("test");
    let pane_id = workspace.focused_pane_id().expect("focused pane");

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, initial_screen),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (client_tx, client_control_rx, client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    assert!(server.claim_unowned_shell_tab_geometry(1, true));

    (server, client_control_rx, client_rx, pane_id)
}

#[test]
fn server_keybinding_filter_keeps_whole_config_failures() {
    assert!(!config::is_keybinding_config_diagnostic(
        "config parse error: invalid value at `keys.new_tab = @`; using defaults"
    ));
    assert!(!config::is_keybinding_config_diagnostic(
        "config read error: permission denied at keys.toml; using defaults"
    ));
    assert!(config::is_keybinding_config_diagnostic(
        "unsafe direct keybinding: keys.close_pane would intercept typing"
    ));
}

#[test]
fn client_shell_host_theme_follows_foreground_client() {
    let mut server = test_headless_server();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);

    let dark = protocol::ClientHostColor {
        r: 20,
        g: 30,
        b: 40,
    };
    let blue = protocol::ClientHostColor {
        r: 10,
        g: 20,
        b: 200,
    };
    assert!(
        server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: 1,
            update: protocol::ClientHostThemeUpdate::DefaultColor {
                kind: protocol::ClientHostDefaultColorKind::Background,
                color: dark,
            },
        })
    );
    assert!(
        server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: 1,
            update: protocol::ClientHostThemeUpdate::PaletteColors(vec![(4, blue)]),
        })
    );
    server.handle_server_event(ServerEvent::ClientShellHostTheme {
        client_id: 1,
        update: protocol::ClientHostThemeUpdate::Appearance(protocol::ClientHostAppearance::Dark),
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
        Some(crate::terminal_theme::HostAppearance::Dark)
    );
    assert!(server.app.state.host_terminal_appearance_explicit);

    let light = protocol::ClientHostColor {
        r: 240,
        g: 230,
        b: 220,
    };
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: 2,
            update: protocol::ClientHostThemeUpdate::DefaultColor {
                kind: protocol::ClientHostDefaultColorKind::Background,
                color: light,
            },
        })
    );
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(dark.into())
    );

    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(light.into())
    );
    assert_eq!(
        server.app.state.host_terminal_appearance,
        Some(crate::terminal_theme::HostAppearance::Light)
    );
    assert!(!server.app.state.host_terminal_appearance_explicit);
}

#[test]
fn terminal_clients_store_known_cell_geometry_independently_of_pixel_mouse() {
    let mut server = test_headless_server();

    let (writer, _control_rx, _render_rx) = test_client_writer();
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: true,
        writer,
    }));
    assert!(!server.clients[&7].pixel_mouse);
    assert_eq!(
        server.clients[&7].cell_size,
        crate::terminal_cell_size::HostCellSize::default()
    );

    let (writer, _control_rx, _render_rx) = test_client_writer();
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 8,
        cols: 80,
        rows: 24,
        cell_width_px: 10,
        cell_height_px: 20,
        pixel_mouse: false,
        writer,
    }));
    assert!(!server.clients[&8].pixel_mouse);
    assert_eq!(
        server.clients[&8].cell_size,
        crate::terminal_cell_size::HostCellSize {
            width_px: 10,
            height_px: 20,
        }
    );
}

#[test]
fn terminal_attach_rejects_missing_terminal_and_removes_client() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();

    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        writer,
    }));
    assert!(matches!(
        server.clients.get(&7).map(|client| &client.mode),
        Some(ClientConnectionMode::TerminalPending)
    ));

    assert!(
        !server.handle_server_event(ServerEvent::ClientAttachTerminal {
            client_id: 7,
            terminal_id: "term_missing".to_owned(),
            takeover: false,
        })
    );
    assert!(!server.clients.contains_key(&7));
    let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
    assert_eq!(
        reason,
        Some("terminal attach failed: terminal term_missing not found".to_owned())
    );
}

fn with_terminal_session_test_server(
    test: impl FnOnce(&mut HeadlessServer, crate::terminal::TerminalId, String, String),
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("test");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
    let terminal_id_string = terminal_id.to_string();
    let public_pane_id = format!("{}:p1", workspace.id);
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.terminal_runtimes.insert(
        terminal_id.clone(),
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );

    test(&mut server, terminal_id, terminal_id_string, public_pane_id);

    drop(server);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

fn connect_pending_terminal_client(server: &mut HeadlessServer, client_id: u64) {
    let _control_rx = connect_pending_terminal_client_with_control_rx(server, client_id);
}

fn connect_pending_terminal_client_with_control_rx(
    server: &mut HeadlessServer,
    client_id: u64,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (writer, control_rx, _render_rx) = test_client_writer();
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id,
        cols: 100,
        rows: 30,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        writer,
    }));
    assert!(matches!(
        server.clients.get(&client_id).map(|client| &client.mode),
        Some(ClientConnectionMode::TerminalPending)
    ));
    control_rx
}

#[test]
fn explicit_agent_history_read_requires_idle_on_alternate_screen() {
    with_terminal_session_test_server(
        |server, terminal_id, _terminal_id_string, public_pane_id| {
            let terminal = server
                .app
                .state
                .terminals
                .get_mut(&terminal_id)
                .expect("terminal");
            terminal.detected_agent = Some(crate::detect::Agent::Claude);
            terminal.state = crate::detect::AgentState::Working;
            server.app.terminal_runtimes.insert(
                terminal_id,
                crate::terminal::TerminalRuntime::test_with_screen_bytes(
                    80,
                    24,
                    b"\x1b[?1049hworking",
                ),
            );
            let request = api::schema::Request {
                id: "read".into(),
                method: api::schema::Method::AgentRead(api::schema::AgentReadParams {
                    target: public_pane_id.clone(),
                    source: api::schema::ReadSource::Recent,
                    lines: Some(200),
                    format: api::schema::ReadFormat::Text,
                    strip_ansi: true,
                }),
            };

            assert_eq!(
                server.agent_read_not_idle_error(&request),
                Some(api::schema::ErrorBody {
                    code: "agent_not_idle".into(),
                    message: format!(
                        "cannot read 200 lines while {public_pane_id} is working: its alternate-screen history can only be captured by scrolling while idle. Wait and retry, or use --source visible"
                    ),
                })
            );

            let mut default_request = request.clone();
            let api::schema::Method::AgentRead(params) = &mut default_request.method else {
                unreachable!();
            };
            params.lines = None;
            assert_eq!(server.agent_read_not_idle_error(&default_request), None);

            let mut visible_request = request;
            let api::schema::Method::AgentRead(params) = &mut visible_request.method else {
                unreachable!();
            };
            params.source = api::schema::ReadSource::Visible;
            assert_eq!(server.agent_read_not_idle_error(&visible_request), None);
        },
    );
}

#[test]
fn terminal_attach_disconnect_restores_client_shell_pane_size() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let second_tab = workspace.test_add_tab(Some("second"));
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
    let terminal_id_string = terminal_id.to_string();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("second tab id");
    server.app.terminal_runtimes.insert(
        terminal_id.clone(),
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.clients.insert(
        1,
        ClientConnection::new(
            (120, 40),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.reconcile_client_shell_locations();
    assert!(server.claim_unowned_shell_tab_geometry(1, true));
    let expected_shell_size = server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .expect("runtime")
        .current_size();

    connect_pending_terminal_client(&mut server, 2);
    assert!(
        server.handle_server_event(ServerEvent::ClientAttachTerminal {
            client_id: 2,
            terminal_id: terminal_id_string,
            takeover: false,
        })
    );
    assert_eq!(server.foreground_client_id, Some(1));
    assert!(
        server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id)
    );
    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size(),
        (30, 100)
    );

    assert!(server.focus_shell_client_on_tab(1, &second_tab_id));
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 2 }));
    assert!(
        !server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id)
    );
    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size(),
        expected_shell_size
    );

    drop(server);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn terminal_attach_is_rejected_during_alt_screen_read() {
    with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
        let (respond_to, _response_rx) = std::sync::mpsc::channel();
        server.pending_alt_screen_reads.push(
            crate::server::alt_screen_read::PendingAltScreenRead::start(
                terminal_id,
                "read".into(),
                respond_to,
                "fallback".into(),
                api::schema::PaneReadResult {
                    pane_id: "w1:p1".into(),
                    workspace_id: "w1".into(),
                    tab_id: "w1:t1".into(),
                    source: api::schema::ReadSource::Recent,
                    format: api::schema::ReadFormat::Text,
                    text: String::new(),
                    revision: 0,
                    truncated: false,
                },
                120,
                false,
                crate::terminal::ScreenSnapshot {
                    cols: 80,
                    rows: Vec::new(),
                },
                0,
                Instant::now(),
            ),
        );
        let control_rx = connect_pending_terminal_client_with_control_rx(server, 7);

        assert!(
            !server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );
        assert!(!server.clients.contains_key(&7));
        assert!(
            !server
                .terminal_attach_owners
                .contains_key(&terminal_id_string)
        );
        let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
        assert_eq!(
            reason,
            Some(format!(
                "terminal attach failed: terminal {terminal_id_string} has a read in progress; retry"
            ))
        );
    });
}

#[test]
fn terminal_attach_rejects_second_client_without_takeover() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );

        connect_pending_terminal_client(server, 8);
        assert!(
            !server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 8,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );

        assert!(server.clients.contains_key(&7));
        assert!(!server.clients.contains_key(&8));
        assert_eq!(
            server.terminal_attach_owners.get(&terminal_id_string),
            Some(&7)
        );
    });
}

#[test]
fn terminal_attach_takeover_replaces_existing_client() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );

        connect_pending_terminal_client(server, 8);
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 8,
                terminal_id: terminal_id_string.clone(),
                takeover: true,
            })
        );

        assert!(!server.clients.contains_key(&7));
        assert!(server.clients.contains_key(&8));
        assert_eq!(
            server.terminal_attach_owners.get(&terminal_id_string),
            Some(&8)
        );
    });
}

#[test]
fn terminal_attach_detach_sends_shutdown_before_removal() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
        let control_rx = connect_pending_terminal_client_with_control_rx(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );

        assert!(server.handle_server_event(ServerEvent::ClientDetach { client_id: 7 }));

        assert!(!server.clients.contains_key(&7));
        assert!(
            !server
                .terminal_attach_owners
                .contains_key(&terminal_id_string)
        );
        let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
        assert_eq!(reason, Some("detached".to_owned()));
    });
}

#[test]
fn terminal_attach_is_told_about_rejected_pastes_and_dropped_input_once() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
        let control_rx = connect_pending_terminal_client_with_control_rx(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );
        let notices = || {
            std::iter::from_fn(|| {
                control_rx
                    .recv_timeout(std::time::Duration::from_millis(300))
                    .ok()
            })
            .map(read_server_message)
            .filter_map(|message| match message {
                ServerMessage::DirectTerminalNotice { message } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>()
        };

        // Every rejected paste is its own user action and is reported.
        for _ in 0..2 {
            server.handle_server_event(ServerEvent::ClientPasteRejected {
                client_id: 7,
                size: 2_000_000,
                max: 1_048_576,
            });
        }
        let pastes = notices();
        assert_eq!(pastes.len(), 2);
        assert!(pastes[0].starts_with("Paste rejected"));

        // Dropped input is reported once until input gets through again.
        server.report_terminal_attach_input(7, AttachInputDelivery::Dropped);
        server.report_terminal_attach_input(7, AttachInputDelivery::Dropped);
        server.report_terminal_attach_input(7, AttachInputDelivery::Failed);
        let dropped = notices();
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].contains(&terminal_id_string));
        server.report_terminal_attach_input(7, AttachInputDelivery::Delivered);
        server.report_terminal_attach_input(7, AttachInputDelivery::Dropped);
        assert_eq!(
            notices().len(),
            1,
            "a new streak of drops is reported again"
        );
        assert!(
            server.clients.contains_key(&7),
            "notices never end the attach"
        );
    });
}

#[test]
fn unchanged_git_refresh_does_not_request_headless_render() {
    let mut server = test_headless_server();
    server.app.git_refresh_in_flight = true;
    let mut workspace = crate::workspace::Workspace::test_new("one");
    let workspace_id = workspace.id.clone();
    let cwd = workspace.identity_cwd.clone();
    workspace.cached_auto_label = "cached".into();
    workspace.cached_git_status_key = cwd.clone();
    workspace.cached_git_branch = None;
    server.app.state.workspaces.push(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        results: vec![crate::workspace::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: crate::workspace::GitStatusRefreshDemand::ALL,
            auto_label: "cached".into(),
            branch: None,
            ahead_behind: None,
            space: None,
        }],
        cache_updates: Vec::new(),
    });

    assert!(!changed);
    assert!(!server.app.git_refresh_in_flight);
}

#[test]
fn changed_git_refresh_requests_headless_render() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("one");
    let workspace_id = workspace.id.clone();
    let cwd = workspace.identity_cwd.clone();
    server.app.state.workspaces.push(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        results: vec![crate::workspace::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: crate::workspace::GitStatusRefreshDemand::ALL,
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
    let workspace = crate::workspace::Workspace::test_new("host-shutdown");
    let pane_id = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server
        .host_shutdown_requested
        .store(true, Ordering::Release);
    // The test policy never saves, so the checkpoint writes nothing and the
    // real session file is untouched.
    server.sync_host_shutdown_freeze(Instant::now());
    let freeze = server
        .host_shutdown_freeze
        .as_mut()
        .expect("warning freezes saving");
    assert!(!freeze.persist_session);
    // Pretend saving was on before the warning, so the thaw has to restore it.
    freeze.persist_session = true;
    assert!(!server.app.policy.persist_session);
    assert!(server.app.session_save_deadline.is_none());

    // The server keeps running and applies pane deaths; only the disk is frozen.
    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
            pane_id,
            exit_reason: crate::platform::ChildExitReason::Exited,
        })
    );
    assert!(server.app.find_pane(pane_id).is_none());
    assert!(!server.app.policy.persist_session);

    // Cancellation reported through the flag thaws and re-saves current state.
    server.app.state.session_dirty = false;
    server
        .host_shutdown_requested
        .store(false, Ordering::Release);
    server.sync_host_shutdown_freeze(Instant::now());
    assert!(server.host_shutdown_freeze.is_none());
    assert!(server.app.policy.persist_session);
    assert!(server.app.state.session_dirty);
    // Not stopping: the warning alone never ends the server.
    assert!(!server.shutting_down);
    assert!(!server.should_quit.load(Ordering::Acquire));
    server.app.policy.persist_session = false;
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn focused_foreground_marks_its_own_tab_seen_not_the_global_active_tab() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("seen");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_tab_id = server.app.public_tab_id(0, 0).expect("first tab id");

    let (control, _) = connect_test_shell(&mut server, 81, 100, 30);
    let _ = control.recv().expect("snapshot");
    assert!(server.focus_shell_client_on_tab(81, &first_tab_id));
    // Another client's endpoint request would leave the global active tab
    // on the second tab while client 81 keeps looking at the first.
    assert!(server.app.state.switch_workspace_tab(0, second_tab));
    for tab in &mut server.app.state.workspaces[0].tabs {
        for pane in tab.panes.values_mut() {
            pane.seen = false;
        }
    }
    server.foreground_client_id = Some(81);
    server
        .clients
        .get_mut(&81)
        .expect("test precondition")
        .outer_terminal_focus = Some(true);

    server.sync_foreground_client_state();

    let tabs = &server.app.state.workspaces[0].tabs;
    assert!(tabs[0].panes[&first_pane].seen, "the viewed tab is seen");
    assert!(
        !tabs[second_tab].panes[&second_pane].seen,
        "a tab nobody focused keeps its done marker"
    );

    // An unfocused foreground terminal marks nothing.
    for pane in server.app.state.workspaces[0].tabs[0].panes.values_mut() {
        pane.seen = false;
    }
    server
        .clients
        .get_mut(&81)
        .expect("test precondition")
        .outer_terminal_focus = Some(false);
    server.sync_foreground_client_state();
    assert!(!server.app.state.workspaces[0].tabs[0].panes[&first_pane].seen);
    assert_eq!(server.app.state.outer_terminal_focus, Some(false));
}

#[tokio::test]
async fn oversized_shell_frame_is_reported_once_until_a_frame_is_sent() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("oversized")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    // Even blank cells cost several bytes each on the wire, so 600k of them
    // cannot fit in one frame.
    let (control, render_rx) = connect_test_shell(&mut server, 91, 2000, 300);
    // The test writer forwards queued control messages from a background
    // thread (see `ClientWriter::test_channel`), so a message queued by this
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

    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: 91,
        surface_cols: 80,
        surface_rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    server.render_and_stream();
    assert!(!reported(&server), "a frame that fits clears the report");
    assert!(render_rx.try_recv().is_ok(), "the smaller frame was sent");
    assert!(render::oversized_frame_notice(3_000_000, MAX_FRAME_SIZE).contains("too large"));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn client_socket_is_owner_only_from_the_moment_it_is_reachable() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = std::env::temp_dir().join(format!(
        "hb-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir).expect("test precondition");
    let path = dir.join("client.sock");

    let listener = bind_owner_only_listener(&path).expect("bind");
    let mode = fs::metadata(&path)
        .expect("socket exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    // The staging directory is gone; only the socket is left.
    let entries = fs::read_dir(&dir)
        .expect("test precondition")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from("client.sock")]);
    // The linked name reaches the listener.
    assert!(crate::ipc::connect_local_stream(&path).is_ok());
    assert!(listener.accept().is_ok());
    // A second server never replaces a socket that is already there.
    let err = bind_owner_only_listener(&path).expect_err("path is taken");
    assert_eq!(err.kind(), io::ErrorKind::AddrInUse);

    drop(listener);
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn host_shutdown_freeze_waits_for_monitor_cancellation() {
    let mut server = test_headless_server();
    server
        .host_shutdown_requested
        .store(true, Ordering::Release);
    let warned_at = Instant::now();
    server.sync_host_shutdown_freeze(warned_at);
    assert!(server.host_shutdown_freeze.is_some());

    server.sync_host_shutdown_freeze(warned_at + Duration::from_secs(30));
    assert!(server.host_shutdown_freeze.is_some());
    assert!(server.host_shutdown_requested.load(Ordering::Acquire));

    server.sync_host_shutdown_freeze(warned_at + Duration::from_secs(60));
    assert!(server.host_shutdown_freeze.is_some());
    server
        .host_shutdown_requested
        .store(false, Ordering::Release);
    server.sync_host_shutdown_freeze(warned_at + Duration::from_secs(61));
    assert!(server.host_shutdown_freeze.is_none());
    assert!(!server.host_shutdown_requested.load(Ordering::Acquire));
    // No monitor ran before the warning, so none was started by the thaw.
    assert!(server.host_shutdown_monitor.is_none());
}

#[tokio::test]
async fn signal_quit_drain_keeps_dying_panes_in_the_layout() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("signal-quit");
    let pane_id = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server
        .app
        .event_tx
        .try_send(AppEvent::PaneDied {
            pane_id,
            exit_reason: crate::platform::ChildExitReason::Exited,
        })
        .expect("test precondition");
    server.signal_quit_requested.store(true, Ordering::Release);
    server.should_quit.store(true, Ordering::Release);

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
    let mut workspace = crate::workspace::Workspace::test_new("pane-death-views");
    let dead_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("second tab id");

    let (first_control, _) = connect_test_shell(&mut server, 71, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 72, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(72, &second_tab_id));
    server
        .clients
        .get_mut(&71)
        .expect("test precondition")
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&72)
        .expect("test precondition")
        .outer_terminal_focus = Some(false);

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: crate::platform::ChildExitReason::Exited
        })
    );

    assert_eq!(
        server.shell_tab_id_for_client(71).as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        server.shell_tab_id_for_client(72).as_deref(),
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
        server.tab_geometry_controllers.get(&second_tab_id),
        Some(&71)
    );
    let before_resize = server.app.test_runtime(second_pane).current_size();
    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: 71,
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
    let mut workspace = crate::workspace::Workspace::test_new("pane-death-geometry");
    let first_pane = workspace.tabs[0].root_pane;
    let dead_pane = workspace.test_split(ratatui::layout::Direction::Vertical);

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.insert_test_runtime(
        dead_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (control, _) = connect_test_shell(&mut server, 73, 185, 46);
    let _ = control.recv().expect("snapshot");
    let shrunk = server.app.test_runtime(first_pane).current_size();
    assert!(shrunk.0 < 46);

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: crate::platform::ChildExitReason::Exited
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
fn terminal_attach_scroll_moves_attached_runtime_viewport() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut bytes = Vec::new();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let runtime = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);

    apply_terminal_attach_scroll(
        &runtime,
        AttachScrollSource::Wheel,
        AttachScrollDirection::Up,
        3,
        None,
        None,
        0,
    )
    .expect("scroll up");
    let metrics = runtime.scroll_metrics().expect("scroll metrics");
    assert_eq!(metrics.offset_from_bottom, 3);

    apply_terminal_attach_scroll(
        &runtime,
        AttachScrollSource::Wheel,
        AttachScrollDirection::Down,
        2,
        None,
        None,
        0,
    )
    .expect("scroll down");
    let metrics = runtime.scroll_metrics().expect("scroll metrics");
    assert_eq!(metrics.offset_from_bottom, 1);
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn client_pane_pixel_mouse_uses_runtime_pixel_encoding() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20,
            5,
            0,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016h",
            4,
        );
    runtime.resize(5, 20, 10, 20);

    apply_client_pane_input_events(
        &runtime,
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: None,
            modifiers: 0,
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
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?1006h",
            4,
        );
    runtime.resize(24, 80, 10, 20);

    apply_client_pane_input_events(
        &runtime,
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 403,
                y: 240,
                column: 40,
                row: 12,
            },
            geometry: None,
            modifiers: 0,
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
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20,
            5,
            0,
            b"\x1b[?1003h\x1b[?1006h",
            4,
        );
    runtime.resize(5, 20, 10, 20);

    apply_client_pane_input_events(
        &runtime,
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: None,
            modifiers: 0,
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
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20, 5, 4096, &bytes, 4,
        );
    let scroll = |kind| crate::protocol::ClientPaneInputEvent::Mouse {
        kind,
        position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
        geometry: None,
        modifiers: 0,
        lines: 3,
    };

    apply_client_pane_input_events(
        &runtime,
        &[scroll(crate::protocol::ClientMouseKind::ScrollUp)],
    )
    .expect("first scroll up");
    apply_client_pane_input_events(
        &runtime,
        &[scroll(crate::protocol::ClientMouseKind::ScrollUp)],
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
        &[scroll(crate::protocol::ClientMouseKind::ScrollDown)],
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
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            geometry: None,
            modifiers: 0,
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
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
            position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            geometry: None,
            modifiers: 0,
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

#[test]
fn terminal_attach_input_resets_scrolled_viewport() {
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
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20, 5, 4096, &bytes, 4,
        );

    runtime.scroll_up(4);
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        4
    );

    apply_terminal_attach_input(&runtime, b"x".to_vec()).expect("attach input");
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        0
    );
    assert_eq!(
        input_rx.try_recv().expect("forwarded input"),
        Bytes::from("x")
    );

    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

fn with_terminal_attach_runtime(
    initial_bytes: &[u8],
    initial_scroll: usize,
    test: impl FnOnce(&crate::terminal::TerminalRuntime, &mut mpsc::Receiver<Bytes>),
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
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
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

fn apply_terminal_attach_page_up(runtime: &crate::terminal::TerminalRuntime) {
    apply_terminal_attach_scroll(
        runtime,
        AttachScrollSource::PageKey {
            input: b"\x1b[5~".to_vec(),
        },
        AttachScrollDirection::Up,
        4,
        None,
        None,
        0,
    )
    .expect("page key");
}

fn client_page_key(
    code: crate::protocol::ClientKeyCode,
    modifiers: crossterm::event::KeyModifiers,
    kind: crate::protocol::ClientKeyKind,
) -> crate::protocol::ClientPaneInputEvent {
    crate::protocol::ClientPaneInputEvent::Key {
        code,
        modifiers: modifiers.bits(),
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: true,
        physical_key_id: None,
        windows_record: None,
    }
}

#[test]
fn client_plain_page_keys_scroll_shell_transcript_by_pane_height() {
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Press,
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
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Release,
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
                crate::protocol::ClientKeyCode::PageDown,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Press,
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
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::CONTROL,
                crate::protocol::ClientKeyKind::Press,
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

    with_terminal_attach_runtime(b"\x1b[?1h", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Press,
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

#[test]
fn terminal_attach_paste_uses_plain_text_when_runtime_did_not_enable_brackets() {
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_terminal_attach_input(runtime, b"\x1b[200~line one\nline two\x1b[201~".to_vec())
            .expect("attach paste");

        assert_eq!(
            input_rx.try_recv().expect("forwarded paste"),
            Bytes::from_static(b"line one\nline two")
        );
    });
}

#[test]
fn terminal_attach_paste_preserves_brackets_when_runtime_enabled_them() {
    with_terminal_attach_runtime(b"\x1b[?2004h", 0, |runtime, input_rx| {
        apply_terminal_attach_input(runtime, b"\x1b[200~line one\nline two\x1b[201~".to_vec())
            .expect("attach paste");

        assert_eq!(
            input_rx.try_recv().expect("forwarded paste"),
            Bytes::from_static(b"\x1b[200~line one\nline two\x1b[201~")
        );
    });
}

#[test]
fn terminal_attach_page_key_host_scrolls_plain_terminal() {
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            4
        );
        assert!(input_rx.try_recv().is_err());
    });
}

#[test]
fn terminal_attach_page_key_forwards_when_mouse_reporting() {
    with_terminal_attach_runtime(b"\x1b[?1000h", 3, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input_rx.try_recv().expect("forwarded page key"),
            Bytes::from_static(b"\x1b[5~")
        );
    });
}

#[test]
fn terminal_attach_page_key_forwards_when_application_cursor() {
    with_terminal_attach_runtime(b"\x1b[?1h", 3, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input_rx.try_recv().expect("forwarded page key"),
            Bytes::from_static(b"\x1b[5~")
        );
    });
}

#[test]
fn terminal_attach_page_key_host_scrolls_shell_like_decckm_with_bracketed_paste() {
    with_terminal_attach_runtime(b"\x1b[?1h\x1b[?2004h", 0, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            4
        );
        assert!(input_rx.try_recv().is_err());
    });
}

#[test]
fn terminal_attach_page_key_forwards_in_alternate_screen_without_mouse_reporting() {
    with_terminal_attach_runtime(b"\x1b[?1049h", 3, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input_rx.try_recv().expect("forwarded page key"),
            Bytes::from_static(b"\x1b[5~")
        );
    });
}

#[test]
fn headless_scheduled_tasks_expire_agent_metadata() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("metadata");
    let pane_id = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::HookStateReported {
            pane_id,
            source: "custom:pi".into(),
            agent_label: "pi".into(),
            state: crate::detect::AgentState::Working,
            message: None,
            seq: None,
            session_ref: None,
        })
    );
    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::HookMetadataReported {
            pane_id,
            source: "user:pi-display".into(),
            agent_label: Some("pi".into()),
            applies_to_source: Some("custom:pi".into()),
            title: Some("short lived".into()),
            display_agent: None,
            state_labels: HashMap::new(),
            clear_title: false,
            clear_display_agent: false,
            clear_state_labels: false,
            seq: None,
            // Expiry is advanced with the captured deadline below; keep the
            // pre-expiry assertion independent of wall-clock scheduling.
            ttl: Some(Duration::from_secs(60)),
        })
    );

    let deadline = server
        .app
        .agent_metadata_deadline
        .expect("metadata deadline");
    let terminal_id = server.app.state.workspaces[0]
        .pane_state(pane_id)
        .expect("pane")
        .attached_terminal_id
        .clone();
    assert_eq!(
        server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal")
            .effective_title()
            .as_deref(),
        Some("short lived")
    );

    assert!(server.handle_scheduled_tasks_headless(deadline + Duration::from_millis(1)));

    assert_eq!(server.app.agent_metadata_deadline, None);
    assert_eq!(
        server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal")
            .effective_title(),
        None
    );
    assert!(
        server
            .app
            .event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| {
                event.event == crate::api::schema::EventKind::PaneAgentStatusChanged
                    && matches!(
                        &event.data,
                        crate::api::schema::EventData::PaneAgentStatusChanged {
                            title,
                            ..
                        } if title.is_none()
                    )
            })
    );
}

#[tokio::test]
async fn headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace
        .terminal_id(pane_id)
        .cloned()
        .expect("test precondition");
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.ensure_test_terminals();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
        agent: "codex".into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
        dedupe_key: "shepr:codex\0codex\0Id\0codex-session".into(),
    });

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
    let workspace = crate::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace
        .terminal_id(pane_id)
        .cloned()
        .expect("test precondition");
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.ensure_test_terminals();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
        agent: "codex".into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
        dedupe_key: "shepr:codex\0codex\0Id\0codex-session".into(),
    });
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
fn terminal_attach_resize_uses_known_cell_geometry_without_pixel_mouse() {
    with_terminal_session_test_server(|server, _other_terminal_id, terminal_id, _pane_id| {
        let mut client = ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        );
        client.mode = ClientConnectionMode::TerminalAttach {
            terminal_id: terminal_id.clone(),
        };
        server.clients.insert(1, client);

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 1,
            cols: 100,
            rows: 30,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
        }));
        assert_eq!(
            server
                .runtime_for_terminal_id_string(&terminal_id)
                .expect("test precondition")
                .pixel_size(),
            Some((800, 480))
        );
        assert_eq!(
            server.clients[&1].cell_size,
            crate::terminal_cell_size::HostCellSize {
                width_px: 8,
                height_px: 16,
            }
        );
        assert!(!server.clients[&1].pixel_mouse);

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 1,
            cols: 100,
            rows: 30,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        }));
        assert_eq!(
            server
                .runtime_for_terminal_id_string(&terminal_id)
                .expect("test precondition")
                .pixel_size(),
            None
        );
        assert_eq!(
            server.clients[&1].cell_size,
            crate::terminal_cell_size::HostCellSize::default()
        );
        assert!(!server.clients[&1].pixel_mouse);
    });
}

#[test]
fn pending_terminal_resize_does_not_take_shell_foreground_or_geometry() {
    let mut server = test_headless_server();
    server.clients.insert(
        1,
        ClientConnection::new(
            (100, 30),
            crate::terminal_cell_size::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalPending,
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::TerminalAnsi,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    let shell_size = server.effective_size;

    assert!(server.handle_server_event(ServerEvent::ClientResize {
        client_id: 2,
        cols: 200,
        rows: 60,
        cell_width_px: 10,
        cell_height_px: 20,
        pixel_mouse: false,
    }));

    assert_eq!(server.foreground_client_id, Some(1));
    assert_eq!(server.effective_size, shell_size);
    assert_eq!(server.clients[&2].terminal_size, (200, 60));
}

#[tokio::test]
async fn direct_terminal_clients_never_become_foreground_or_claim_tab_geometry() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    for (client_id, mode) in [
        (1, ClientConnectionMode::TerminalPending),
        (
            2,
            ClientConnectionMode::TerminalAttach {
                terminal_id: "t1".into(),
            },
        ),
    ] {
        server.clients.insert(
            client_id,
            ClientConnection::new_with_mode(
                mode,
                (80, 24),
                crate::terminal_cell_size::HostCellSize::default(),
                1,
                RenderEncoding::TerminalAnsi,
                None,
            ),
        );
        assert!(!server.promote_client_to_foreground(client_id));
        assert!(!server.claim_shell_tab_geometry(client_id, false));
        assert!(!server.claim_unowned_shell_tab_geometry(client_id, false));
        assert!(!server.resize_shell_tab_if_controller(client_id, false));
    }
    assert_eq!(server.foreground_client_id, None);
    assert!(server.tab_geometry_controllers.is_empty());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn client_shell_streams_focused_pane_report_all_demand() {
    with_terminal_session_test_server(|server, _other_terminal_id, terminal_id, _pane_id| {
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::terminal_cell_size::HostCellSize::default(),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.app.state.active = Some(0);
        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("focused runtime")
            .test_process_pty_bytes(b"\x1b[>15u");

        server.stream_direct_terminal_keyboard_mode();

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
                crate::terminal_cell_size::HostCellSize::default(),
                client_id,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
    }
    let key = |kind| crate::protocol::ClientPaneInputEvent::Key {
        code: crate::protocol::ClientKeyCode::Char('x'),
        modifiers: 0,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: (kind == crate::protocol::ClientKeyKind::Press).then(|| "x".to_owned()),
        tracks_release: true,
        physical_key_id: Some(0x2d),
        windows_record: None,
    };

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 1,
            pane_id: pane_id.clone(),
            events: vec![key(crate::protocol::ClientKeyKind::Press)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded press").is_empty());
    assert!(server.promote_client_to_foreground(2));

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 1,
            pane_id: pane_id.clone(),
            events: vec![key(crate::protocol::ClientKeyKind::Release)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded release").is_empty());
    assert_eq!(server.foreground_client_id, Some(2));

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 1,
            pane_id,
            events: vec![key(crate::protocol::ClientKeyKind::Press)],
        })
    );
    assert!(
        !input_rx
            .recv()
            .await
            .expect("second encoded press")
            .is_empty()
    );
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 1 }));
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
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
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
        .shell_mouse_capture = true;
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
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
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
        server.app.state.active = Some(0);
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::terminal_cell_size::HostCellSize::default(),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (100, 30),
                crate::terminal_cell_size::HostCellSize::default(),
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        assert!(server.claim_unowned_shell_tab_geometry(2, true));
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
            client_id: 1,
            focused: true,
        }));
        assert_eq!(server.foreground_client_id, Some(1));
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
            client_id: 2,
            focused: true,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "second viewer duplicated focus gain"
        );
        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: 1,
            focused: false,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "remaining viewer lost tab focus"
        );
        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: 2,
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
fn direct_terminal_streams_child_keyboard_and_mouse_modes() {
    with_terminal_session_test_server(|server, _other_terminal_id, terminal_id, _pane_id| {
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new_with_mode(
                ClientConnectionMode::TerminalAttach {
                    terminal_id: terminal_id.clone(),
                },
                (80, 24),
                crate::terminal_cell_size::HostCellSize::default(),
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server
            .clients
            .get_mut(&1)
            .expect("direct attach client")
            .pixel_mouse = true;
        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[>15u\x1b[?1000h");

        server.stream_direct_terminal_keyboard_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("keyboard mode message")
            ),
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 15,
                modify_other_keys_level: 0
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[<u\x1b[>3u\x1b[>4;1m");
        server.stream_direct_terminal_keyboard_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("modifyOtherKeys mode-one keyboard message")
            ),
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 3,
                modify_other_keys_level: 1
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[>4;2m");
        server.stream_direct_terminal_keyboard_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("modifyOtherKeys mode-two keyboard message")
            ),
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 3,
                modify_other_keys_level: 2
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[<u");
        server.stream_direct_terminal_keyboard_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("modifyOtherKeys-only keyboard mode message")
            ),
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 0,
                modify_other_keys_level: 2
            }
        ));

        server.stream_host_mouse_capture_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("mouse capture message")
            ),
            ServerMessage::MouseCapture {
                enabled: true,
                sgr_pixels: false
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[?1016h");
        server.stream_host_mouse_capture_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("pixel mouse capture message")
            ),
            ServerMessage::MouseCapture {
                enabled: true,
                sgr_pixels: true
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[?1000l\x1b[?1016l");
        server.stream_host_mouse_capture_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("child mouse disable message")
            ),
            ServerMessage::MouseCapture {
                enabled: false,
                sgr_pixels: false
            }
        ));
    });
}

#[test]
fn direct_terminal_mouse_uses_runtime_protocol_encoding() {
    with_terminal_session_test_server(|server, runtime_terminal_id, terminal_id, _pane_id| {
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                b"\x1b[?1000h\x1b[?1006h",
                4,
            );
        server
            .app
            .terminal_runtimes
            .insert(runtime_terminal_id, runtime);
        server.clients.insert(
            1,
            ClientConnection::new_with_mode(
                ClientConnectionMode::TerminalAttach {
                    terminal_id: terminal_id.clone(),
                },
                (80, 24),
                crate::terminal_cell_size::HostCellSize::default(),
                1,
                RenderEncoding::TerminalAnsi,
                None,
            ),
        );

        assert!(server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Down(protocol::ClientMouseButton::Left),
            position: protocol::ClientMousePosition::Cell { column: 10, row: 5 },
            geometry: None,
            modifiers: 0,
            lines: 1,
        }));
        assert_eq!(
            input_rx.try_recv().expect("encoded direct mouse input"),
            Bytes::from_static(b"\x1b[<0;11;6M")
        );
    });
}

#[test]
fn direct_terminal_pixel_mouse_uses_runtime_tracking_and_coordinates() {
    with_terminal_session_test_server(|server, runtime_terminal_id, terminal_id, _pane_id| {
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                b"\x1b[?1000h\x1b[?1006h\x1b[?1016h",
                4,
            );
        runtime.resize(24, 80, 10, 20);
        server
            .app
            .terminal_runtimes
            .insert(runtime_terminal_id, runtime);
        server.clients.insert(
            1,
            ClientConnection::new_with_mode(
                ClientConnectionMode::TerminalAttach {
                    terminal_id: terminal_id.clone(),
                },
                (80, 24),
                crate::terminal_cell_size::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                1,
                RenderEncoding::TerminalAnsi,
                None,
            ),
        );
        let client = server.clients.get_mut(&1).expect("direct attach client");
        client.pixel_mouse = true;
        client.host_sgr_pixels_active = Some(true);

        assert!(!server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Down(protocol::ClientMouseButton::Left),
            position: protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 3,
                row: 1,
            },
            geometry: Some(protocol::ClientMouseGeometry {
                cols: 80,
                rows: 24,
                width_px: 800,
                height_px: 480,
            }),
            modifiers: 0,
            lines: 1,
        }));
        assert!(input_rx.try_recv().is_err());

        assert!(server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Down(protocol::ClientMouseButton::Left),
            position: protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: Some(protocol::ClientMouseGeometry {
                cols: 80,
                rows: 24,
                width_px: 805,
                height_px: 485,
            }),
            modifiers: 0,
            lines: 1,
        }));
        assert_eq!(
            input_rx
                .try_recv()
                .expect("proportionally mapped direct pixel mouse input"),
            Bytes::from_static(b"\x1b[<0;21;22M")
        );

        assert!(server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Moved,
            position: protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: Some(protocol::ClientMouseGeometry {
                cols: 80,
                rows: 24,
                width_px: 800,
                height_px: 480,
            }),
            modifiers: 0,
            lines: 1,
        }));
        assert!(input_rx.try_recv().is_err());

        assert!(server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Down(protocol::ClientMouseButton::Left),
            position: protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: Some(protocol::ClientMouseGeometry {
                cols: 80,
                rows: 24,
                width_px: 800,
                height_px: 480,
            }),
            modifiers: 0,
            lines: 1,
        }));
        assert_eq!(
            input_rx
                .try_recv()
                .expect("encoded direct pixel mouse input"),
            Bytes::from_static(b"\x1b[<0;21;22M")
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
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(background_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_tx),
        ),
    );
    server.foreground_client_id = Some(2);
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
    server.foreground_client_id = None;

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
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(foreground_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
    assert!(
        !server.clients.contains_key(&1),
        "failed targeted send should remove the broken foreground client"
    );
}

/// Verify that calls to the app's internal-event methods only occur inside
/// `handle_internal_event_with_forwarding`. This ensures the forwarding
/// bypass cannot be reintroduced.
#[test]
fn no_handle_internal_event_bypass_in_module() {
    let source = include_str!("../../headless.rs");

    // Find all lines containing handle_internal_event
    let mut bypass_lines: Vec<String> = Vec::new();
    let mut inside_forwarding_method = false;
    let mut forwarding_method_brace_depth = 0u32;

    for (i, line) in source.lines().enumerate() {
        let line_num = i + 1;

        // Track when we're inside handle_internal_event_with_forwarding
        if line.contains("fn handle_internal_event_with_forwarding") {
            inside_forwarding_method = true;
            forwarding_method_brace_depth = 0;
        }

        if inside_forwarding_method {
            // Count braces to track when we exit the method
            for ch in line.chars() {
                match ch {
                    '{' => forwarding_method_brace_depth += 1,
                    '}' => {
                        forwarding_method_brace_depth =
                            forwarding_method_brace_depth.saturating_sub(1);
                        if forwarding_method_brace_depth == 0 {
                            inside_forwarding_method = false;
                        }
                    }
                    _ => {}
                }
            }
        } else if (line.contains("self.app.handle_internal_event(")
            || line.contains("self.app.handle_internal_event_with_render_impact("))
            && !line.trim().starts_with("///")
            && !line.contains("contains(")
        {
            // Internal-event call outside the forwarding method.
            bypass_lines.push(format!("line {}: {}", line_num, line.trim()));
        }
    }

    assert!(
        bypass_lines.is_empty(),
        "Found direct calls to self.app.handle_internal_event outside \
             handle_internal_event_with_forwarding (bypass risk):\n  {}",
        bypass_lines.join("\n  ")
    );
}
