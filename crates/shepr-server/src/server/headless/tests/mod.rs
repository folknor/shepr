use super::*;
use crate::test_support::*;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::server::outbox::{ClientOutbox, RenderLaneReceiver};
use bytes::Bytes;
use shepr_protocol::MAX_FRAME_SIZE;
use shepr_protocol::command::EndpointCommand;
use shepr_protocol::surface_reuse::DecodedServerMessage;

/// The pane entries a surface update's metadata carries, whichever variant it is.
fn meta_panes(meta: &Option<shepr_protocol::SurfaceMeta>) -> &[shepr_protocol::PaneSurfacePane] {
    match meta.as_ref().expect("metadata") {
        shepr_protocol::SurfaceMeta::Projection(meta) => &meta.panes,
        shepr_protocol::SurfaceMeta::Patch(meta) => &meta.panes,
    }
}

pub(crate) fn handle_server_event(
    server: &mut HeadlessServer,
    event: crate::server::client_transport::ServerEvent,
) -> bool {
    let changed = server.test_handle_server_event(event);
    // The loop flushes endpoint replies at the end of its pass. The cross-crate
    // endpoint move tests drive renders themselves, so replies leave here at once.
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    changed
}

pub(crate) fn render_now(server: &mut HeadlessServer) {
    server.render_now();
}

pub(crate) fn outer_terminal_focus(
    server: &HeadlessServer,
    client_id: crate::server::ClientId,
) -> Option<bool> {
    server
        .clients
        .get(&client_id)
        .and_then(|client| client.shell_state().outer_terminal_focus)
}

pub(crate) fn client_is_viewed(
    server: &HeadlessServer,
    client_id: crate::server::ClientId,
) -> bool {
    server
        .clients
        .get(&client_id)
        .is_some_and(|client| client.shell_state().is_surface_active())
}

pub(crate) fn dispatch_lifecycle_messages(
    server: &mut HeadlessServer,
    client_id: crate::server::ClientId,
    messages: Vec<shepr_protocol::ClientMessage>,
) {
    for message in messages {
        let event = match message {
            shepr_protocol::ClientMessage::ClientShellResize { geometry } => {
                crate::server::client_transport::ServerEvent::ShellResize {
                    client_id,
                    cell_width_px: geometry.width(),
                    cell_height_px: geometry.height(),
                    surface_cols: geometry.cols(),
                    surface_rows: geometry.rows(),
                    pixel_mouse: geometry.pixel_mouse,
                }
            }
            shepr_protocol::ClientMessage::ClientShellFocus { focused } => {
                crate::server::client_transport::ServerEvent::ShellFocus { client_id, focused }
            }
            shepr_protocol::ClientMessage::ClientShellEndpointRequest {
                boot_id,
                request_id,
                command,
            } => crate::server::client_transport::ServerEvent::ShellEndpointRequest {
                client_id,
                boot_id,
                request_id,
                command: Box::new(command),
            },
            shepr_protocol::ClientMessage::ClientShellHostTheme { update } => {
                crate::server::client_transport::ServerEvent::ShellHostTheme { client_id, update }
            }
            shepr_protocol::ClientMessage::ReplayHostEffects => {
                crate::server::client_transport::ServerEvent::ShellReplayHostEffects { client_id }
            }
            other => panic!("unhandled lifecycle message: {other:?}"),
        };
        server.test_handle_server_event(event);
    }
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
}

#[cfg(test)]
#[path = "already_running.rs"]
mod already_running_tests;
#[cfg(test)]
#[path = "internal_event_drain.rs"]
mod internal_event_drain_tests;
#[cfg(test)]
#[path = "locations.rs"]
mod locations_tests;
#[cfg(test)]
#[path = "pane_exit.rs"]
mod pane_exit_tests;
#[path = "server_stop.rs"]
mod server_stop_tests;
#[cfg(test)]
#[path = "surface_delta.rs"]
mod surface_delta_tests;
#[cfg(test)]
#[path = "surface_interest.rs"]
mod surface_interest_tests;

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
    let config = shepr_config::ServerConfig::default();
    let mut app = crate::app::App::new(&config, crate::app::AppPolicy::Test);
    app.set_test_shell(crate::app::exiting_test_command());
    let (server_event_tx, server_event_rx) = mpsc::channel(64);
    let (api_tx, api_request_rx) = mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
    // Production's listener holds the sender for the server's whole life; a
    // fixture that dropped it would start every test loop with a closed API
    // channel. Tests that need to send swap in a channel of their own.
    std::mem::forget(api_tx);
    let stop_signal = Arc::new(shepr_api::ServerStopSignal::default());

    HeadlessServer {
        app,
        view_epoch: ViewEpoch::INITIAL,
        api_server: None,
        clients: ClientRegistry::default(),
        client_view_keys: HashMap::new(),
        client_shell_boot_id: shepr_test_fixtures::fixed_boot_id(1),
        shell_session_cache: None,
        shell_session_generation: crate::server::clients::ShellSessionGeneration::default(),
        focused_panes: HashSet::new(),
        immediate_pty_sources_dirty: true,
        host_input_modes_dirty: true,
        retained_surface_fallback_reason: None,
        retained_surface_fallbacks_reported: HashSet::new(),
        lifecycle: ShutdownLifecycle::new(stop_signal),
        server_event_rx,
        server_event_tx,
        api_request_rx,
        api_request_open: true,
        shutdown_unregistered_clients: HashMap::new(),
        shutdown_flushes: Vec::new(),
        pending_checkpointed_pane_exits: std::collections::VecDeque::new(),
        replaying_checkpointed_pane_exit: None,
        outbox_wake: Arc::new(tokio::sync::Notify::new()),
        workers: worker::EndpointWorkers::new(),
    }
}

impl HeadlessServer {
    /// Adds a client fixture the way a connection would: it starts at the
    /// session's bookmark, and the locations are then settled, which lands a
    /// client with no bookmark to start from on the first workspace.
    pub(crate) fn insert_test_client(
        &mut self,
        client_id: impl Into<ClientId>,
        mut client: ClientConnection,
    ) {
        client.shell.location = self.initial_client_location();
        self.clients.insert(client_id, client);
        self.reconcile_client_shell_locations();
        self.refresh_client_view_keys();
    }

    /// Puts a client's location on `workspace_id` without a command: no
    /// navigation effect ran, so the session's bookmark stays where it is.
    pub(crate) fn place_test_client_on_workspace(
        &mut self,
        client_id: ClientId,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> bool {
        let Some(index) = self.app.state.workspace_index(workspace_id) else {
            return false;
        };
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let moved = client
            .shell_state_mut()
            .location
            .navigate(*workspace_id, index);
        if moved {
            self.refresh_client_view_keys();
        }
        true
    }
}

/// The public id of the first workspace's focused pane.
pub(crate) fn focused_test_pane(server: &HeadlessServer) -> shepr_protocol::PublicPaneId {
    server
        .app
        .public_pane_id(0, server.app.state.workspaces[0].focused_pane_id())
        .expect("test precondition")
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
fn server_message_encoding_splits_payloads_over_the_frame_cap() {
    let small = shepr_protocol::encode_message(&ServerMessage::ClientShellError {
        kind: shepr_protocol::NoticeKind::PaneInputDropped {
            pane_id: shepr_protocol::PublicPaneId::new(
                &crate::test_support::test_workspace_id("w1"),
                shepr_protocol::PanePublicNumber::new(1).expect("nonzero test number"),
            ),
            events: 1,
        },
    })
    .expect("small message frames");
    assert!(matches!(
        read_server_message(small),
        ServerMessage::ClientShellError { kind: shepr_protocol::NoticeKind::PaneInputDropped { pane_id, events: 1 } } if pane_id == "w1:p1".parse::<shepr_protocol::PublicPaneId>().expect("id")
    ));

    // Clipboard data past one frame crosses as a continued frame and a final
    // one, and reads back whole.
    let data = vec![b'x'; MAX_FRAME_SIZE + 1];
    let large = shepr_protocol::encode_message(&ServerMessage::Clipboard { data: data.clone() })
        .expect("large message frames");
    let first_prefix = u32::from_le_bytes(large[..4].try_into().expect("test precondition"));
    assert_ne!(first_prefix & (1 << 31), 0, "the first frame is continued");
    assert!(matches!(
        read_server_message(large),
        ServerMessage::Clipboard { data: read } if read == data
    ));
}

#[tokio::test]
async fn default_headless_size_lays_out_workspaces_without_clients() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);

    assert_eq!(
        server.app.state.settings.headless_size,
        shepr_core::geometry::GridSize::clamped(
            shepr_config::DEFAULT_HEADLESS_COLS,
            shepr_config::DEFAULT_HEADLESS_ROWS
        )
    );
    assert_eq!(server.app.state.workspace_area(0), None);
    server.render_now();
    let headless = server.app.state.settings.headless_rect();
    assert_eq!(server.app.state.workspace_area(0), Some(headless));
    assert_eq!(
        server.app.state.pane_geometry_for_workspace(0).area,
        headless
    );
    let layout = crate::ui::compute_surface_for(
        &server.app.state,
        &server.app.terminal_runtimes,
        server.app.public_workspace_id(0),
        headless,
    );
    let pane = layout.pane_infos.first().expect("test pane geometry");
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
    server.app.state.settings.headless_size = shepr_core::geometry::GridSize::clamped(72, 18);
    let (_control, _render) = connect_test_shell(&mut server, 7, 112, 36);
    let client_size = server.app.test_runtime(pane_id).current_size();

    assert!(server.test_handle_server_event(ServerEvent::Disconnected {
        client_id: ClientId::test_new(7),
    }));

    let target = server
        .app
        .public_workspace_id(0)
        .expect("test workspace target");
    let layout = crate::ui::compute_surface_for(
        &server.app.state,
        &server.app.terminal_runtimes,
        Some(target),
        server.app.state.settings.headless_rect(),
    );
    let pane = layout.pane_infos.first().expect("test pane geometry");
    let headless_pane_size = (pane.inner_rect.height, pane.inner_rect.width);

    assert_eq!(
        server.app.state.workspace_area(0),
        Some(server.app.state.settings.headless_rect())
    );
    assert_ne!(client_size, headless_pane_size);
    assert_eq!(
        server.app.test_runtime(pane_id).current_size(),
        headless_pane_size
    );
    shutdown_test_runtimes(&mut server);
}

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
async fn headless_api_reads_latest_title() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("one")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let pane_id = server.app.state.workspaces[0].root_pane();
    let terminal_id = server.app.state.workspaces[0].panes()[&pane_id]
        .attached_terminal_id
        .clone();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .ownership_mut()
        .set_detected_agent_process_at(
            shepr_agent::detect::Agent::Claude,
            std::time::Instant::now(),
        );
    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes(b"\x1b]0;\xe2\xa0\x8b task\x07");
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);
    server.app.render_dirty.request_terminal_title(pane_id);

    let first = headless_agent_list(&mut server)
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
    let second = headless_agent_list(&mut server)
        .pop()
        .expect("test precondition");
    assert_eq!(second.terminal_title.as_deref(), Some("⠙ task"));
    assert_eq!(second.terminal_title_stripped.as_deref(), Some("task"));
}

fn headless_agent_list(server: &mut HeadlessServer) -> Vec<crate::app::SnapshotAgent> {
    server.app.sync_pending_terminal_titles();
    server.app.session_snapshot().agents
}

#[tokio::test]
async fn a_closed_api_channel_stops_being_selected() {
    let mut server = test_headless_server();
    let (api_tx, api_request_rx) = mpsc::channel(1);
    drop(api_tx);
    server.api_request_rx = api_request_rx;

    // Wakeups left from setup are consumed first; a closed channel still
    // selected would answer every wait at once and never let one go idle.
    let mut went_idle = false;
    for _ in 0..16 {
        let wait = server.next_loop_event(None);
        match tokio::time::timeout(Duration::from_millis(50), wait).await {
            Ok(_) => {}
            Err(_) => {
                went_idle = true;
                break;
            }
        }
    }
    assert!(!server.api_request_open, "the closed channel was noticed");
    assert!(
        went_idle,
        "the loop waits instead of spinning on the closed channel"
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn server_stop_interrupts_server_event_backlog() {
    let mut server = test_headless_server();
    for client_id in 1..=64 {
        server
            .server_event_tx
            .try_send(ServerEvent::Disconnected {
                client_id: client_id.into(),
            })
            .expect("test precondition");
    }

    server.lifecycle.stop_signal().request();

    assert!(!server.test_drain_server_events());
    assert!(server.server_event_rx.try_recv().is_ok());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn server_event_drain_is_bounded_and_keeps_remaining_events_in_order() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        42,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            42,
            writer,
        ),
    );

    let event_count = crate::limits::SERVER_EVENT_DRAIN_LIMIT + 2;
    let (server_event_tx, server_event_rx) =
        tokio::sync::mpsc::channel(crate::limits::SERVER_EVENT_DRAIN_LIMIT + 2);
    server.server_event_tx = server_event_tx;
    server.server_event_rx = server_event_rx;
    for index in 0..event_count {
        server
            .server_event_tx
            .try_send(ServerEvent::PasteRejected {
                client_id: ClientId::test_new(42),
                size: index + 1,
            })
            .expect("test precondition");
    }

    assert!(!server.test_drain_server_events());
    assert_eq!(server.server_event_rx.len(), 2);
    for expected_size in 1..=crate::limits::SERVER_EVENT_DRAIN_LIMIT {
        let ServerMessage::ClientShellError {
            kind: shepr_protocol::NoticeKind::LimitExceeded(error),
        } = read_server_message(
            control_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("first server event batch is reported"),
        )
        else {
            panic!("expected paste rejection notice");
        };
        assert_eq!(error.actual, expected_size);
        assert_eq!(error.limit.max(), shepr_protocol::MAX_INPUT_PAYLOAD);
    }

    assert!(!server.test_drain_server_events());
    for expected_size in (crate::limits::SERVER_EVENT_DRAIN_LIMIT + 1)..=event_count {
        let ServerMessage::ClientShellError {
            kind: shepr_protocol::NoticeKind::LimitExceeded(error),
        } = read_server_message(
            control_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("remaining server events are reported on the next pass"),
        )
        else {
            panic!("expected paste rejection notice");
        };
        assert_eq!(error.actual, expected_size);
        assert_eq!(error.limit.max(), shepr_protocol::MAX_INPUT_PAYLOAD);
    }
    assert_eq!(server.server_event_rx.len(), 0);
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
            request: shepr_api::schema::AppRequest {
                id: id.into(),
                method: shepr_api::schema::AppMethod::DetectCapture(
                    shepr_api::schema::PaneTarget {
                        pane_id: "w1:p1".into(),
                    },
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
    let (api_tx, api_rx) = tokio::sync::mpsc::channel(1);
    server.api_request_rx = api_rx;

    let (queued, queued_rx) = shutdown_test_request("queued");
    assert!(api_tx.try_send(queued).is_ok());

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown completes");

    assert_server_unavailable(&queued_rx, "queued");
    // A request dispatched after cleanup fails at the sender, which the API
    // thread turns into `server_unavailable` at once.
    let (late, _late_rx) = shutdown_test_request("late");
    assert!(api_tx.try_send(late).is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn an_endpoint_request_queued_at_shutdown_is_answered() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("stopping")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (control, _render) = connect_matching_test_shell(&mut server, 44);
    server
        .server_event_tx
        .try_send(ServerEvent::ShellEndpointRequest {
            client_id: ClientId::test_new(44),
            boot_id: server.client_shell_boot_id.clone(),
            request_id: "at-stop".into(),
            command: Box::new(EndpointCommand::PaneFocus(
                shepr_protocol::command::PaneTarget {
                    pane_id: shepr_test_fixtures::id("w1:p1"),
                },
            )),
        })
        .expect("test precondition");

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown completes");

    let mut messages = Vec::new();
    loop {
        let message = read_server_message(
            control
                .recv_timeout(Duration::from_millis(500))
                .expect("the queued refusal and shutdown notice are flushed"),
        );
        let shutdown = matches!(&message, ServerMessage::ServerShutdown { .. });
        messages.push(message);
        if shutdown {
            break;
        }
    }
    let refusal_index = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::ClientShellEndpointResponse {
                    request_id,
                    result: Err(shepr_protocol::command::EndpointError::ShuttingDown),
                    ..
                } if request_id == "at-stop"
            )
        })
        .expect("the queued command is answered, not left to its timeout");
    let shutdown_index = messages
        .iter()
        .position(|message| matches!(message, ServerMessage::ServerShutdown { .. }))
        .expect("shutdown notice is delivered");
    assert!(
        refusal_index < shutdown_index,
        "refusal must precede shutdown"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_queued_new_client_gets_its_endpoint_refusal_before_shutdown() {
    let mut server = test_headless_server();
    let client_id = ClientId::test_new(45);
    let boot_id = server.client_shell_boot_id.clone();
    let (writer, control, _render) = test_client_writer();
    assert!(
        server
            .server_event_tx
            .try_send(ServerEvent::ShellConnected {
                client_id,
                surface_cols: 80,
                surface_rows: 23,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
                mouse_capture: false,
                surface_active: true,
                outbox: writer,
            })
            .is_ok()
    );
    assert!(
        server
            .server_event_tx
            .try_send(ServerEvent::ShellEndpointRequest {
                client_id,
                boot_id,
                request_id: "new-client-command".into(),
                command: Box::new(EndpointCommand::PaneFocus(
                    shepr_protocol::command::PaneTarget {
                        pane_id: shepr_test_fixtures::id("w1:p1"),
                    },
                )),
            })
            .is_ok()
    );

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown settles the queued connection and command");

    let mut messages = Vec::new();
    loop {
        let message = read_server_message(
            control
                .recv_timeout(Duration::from_millis(500))
                .expect("the queued refusal and shutdown notice are flushed"),
        );
        let shutdown = matches!(&message, ServerMessage::ServerShutdown { .. });
        messages.push(message);
        if shutdown {
            break;
        }
    }
    let refusal_index = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::ClientShellEndpointResponse {
                    request_id,
                    result: Err(shepr_protocol::command::EndpointError::ShuttingDown),
                    ..
                } if request_id == "new-client-command"
            )
        })
        .expect("the queued command is refused");
    let shutdown_index = messages
        .iter()
        .position(|message| matches!(message, ServerMessage::ServerShutdown { .. }))
        .expect("the late client receives shutdown");
    assert!(
        refusal_index < shutdown_index,
        "refusal must precede shutdown"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_dequeued_new_client_waits_for_queued_commands_before_shutdown() {
    let mut server = test_headless_server();
    let client_id = ClientId::test_new(46);
    let (writer, control, _render) = test_client_writer();
    server
        .shutdown_unregistered_clients
        .insert(client_id, writer);
    assert!(
        server
            .server_event_tx
            .try_send(ServerEvent::ShellEndpointRequest {
                client_id,
                boot_id: server.client_shell_boot_id.clone(),
                request_id: "dequeued-client-command".into(),
                command: Box::new(EndpointCommand::PaneFocus(
                    shepr_protocol::command::PaneTarget {
                        pane_id: shepr_test_fixtures::id("w1:p1"),
                    },
                )),
            })
            .is_ok()
    );

    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown settles a selected connection before notifying it");

    assert!(matches!(
        read_server_message(
            control
                .recv_timeout(Duration::from_millis(500))
                .expect("the endpoint refusal is flushed first")
        ),
        ServerMessage::ClientShellEndpointResponse {
            request_id,
            result: Err(shepr_protocol::command::EndpointError::ShuttingDown),
            ..
        } if request_id == "dequeued-client-command"
    ));
    assert!(matches!(
        read_server_message(
            control
                .recv_timeout(Duration::from_millis(500))
                .expect("the shutdown notice follows the refusal")
        ),
        ServerMessage::ServerShutdown { .. }
    ));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn api_request_selected_during_shutdown_is_answered() {
    let mut server = test_headless_server();
    server.initiate_shutdown();
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
        request: shepr_api::schema::AppRequest {
            id: "headless_capture_after_events".into(),
            method: shepr_api::schema::AppMethod::DetectCapture(shepr_api::schema::PaneTarget {
                pane_id: "w9:p9".into(),
            }),
        },
        respond_to,
    });
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("test precondition");
    let response: serde_json::Value =
        serde_json::from_str(&crate::test_support::test_json(&response))
            .expect("test precondition");

    assert_eq!(response["error"]["code"], "pane_not_found");
    assert!(server.app.event_rx.try_recv().is_err());
}

#[test]
fn api_request_drain_is_bounded_and_keeps_remaining_requests_in_order() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("bounded-api")];
    let request_count = crate::limits::API_REQUEST_DRAIN_LIMIT + 2;
    let (api_tx, api_rx) = tokio::sync::mpsc::channel(request_count);
    server.api_request_rx = api_rx;
    let mut responses = Vec::with_capacity(request_count);
    for index in 0..request_count {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            api_tx
                .try_send(shepr_api::ApiRequestMessage {
                    request: shepr_api::schema::AppRequest {
                        id: format!("bounded-{index}"),
                        method: shepr_api::schema::AppMethod::DetectCapture(
                            shepr_api::schema::PaneTarget {
                                pane_id: format!("w999:p{index}"),
                            },
                        ),
                    },
                    respond_to,
                })
                .is_ok()
        );
        responses.push(response_rx);
    }

    server.drain_api_requests_with_shutdown_check();
    assert_eq!(
        server.api_request_rx.len(),
        request_count - crate::limits::API_REQUEST_DRAIN_LIMIT
    );
    for (index, response_rx) in responses
        .iter()
        .take(crate::limits::API_REQUEST_DRAIN_LIMIT)
        .enumerate()
    {
        let error = response_rx
            .try_recv()
            .expect("first API request batch is answered")
            .expect_err("test pane does not exist");
        assert_eq!(
            error.into_message(),
            format!("pane w999:p{index} not found")
        );
    }
    for response_rx in responses
        .iter()
        .skip(crate::limits::API_REQUEST_DRAIN_LIMIT)
    {
        assert!(response_rx.try_recv().is_err());
    }

    server.drain_api_requests_with_shutdown_check();
    for (index, response_rx) in responses
        .iter()
        .enumerate()
        .skip(crate::limits::API_REQUEST_DRAIN_LIMIT)
    {
        let error = response_rx
            .try_recv()
            .expect("remaining API request is answered on the next pass")
            .expect_err("test pane does not exist");
        assert_eq!(
            error.into_message(),
            format!("pane w999:p{index} not found")
        );
    }
    assert_eq!(server.api_request_rx.len(), 0);
    shutdown_test_runtimes(&mut server);
}

fn window_title_test_server() -> (HeadlessServer, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("herd")];
    server.app.state.set_bookmark_index(Some(0));

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
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
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("herd")];
    server.app.state.set_bookmark_index(Some(0));
    server.app.configure_window_title("{workspace}");

    // The server renders before the first client attaches. Nothing was
    // delivered, so the first client is written to when it arrives.
    server.sync_window_title();

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
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
    server.app.configure_window_title("{workspace}");
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
            shepr_termio::host_term::cell_size::HostCellSize::default(),
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
    server.app.configure_window_title("{workspace}");

    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd".to_string()))
    );

    // An unchanged title must not re-emit an OSC on every render.
    server.sync_window_title();
    assert!(no_window_title(&control_rx));

    server.app.state.workspaces[0].custom_name = Some("build".into());
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
    server.app.configure_window_title("{terminal_title}");
    server.app.state.ensure_test_terminals();
    let pane_id = server.app.state.workspaces[0].root_pane();
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
fn a_client_without_a_writer_does_not_cache_the_window_title() {
    let (mut server, _control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}");

    // Production never keeps a writer-less client (a detach removes it), but
    // the targeted send must still report a writer-less entry as undelivered.
    if let Some(client) = server.clients.get_mut(&1) {
        client.outbox = ClientOutbox::detached();
    }
    assert!(!server.send_to_client(
        ClientId::test_new(1),
        &ServerMessage::WindowTitle {
            title: Some("probe".into()),
        }
    ));
    server.sync_window_title();
    assert!(server.clients[&1].outbox.told_window_title().is_none());

    // Attaching again has to deliver the title rather than skip it as sent.
    let (client_tx, control_rx, _render_rx) = test_client_writer();
    if let Some(client) = server.clients.get_mut(&1) {
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
    server.insert_test_client(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
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
    let survivor_pane = survivor.focused_pane_id();
    let disconnected = shepr_mux::workspace::Workspace::test_new("disconnected");
    let disconnected_pane = disconnected.focused_pane_id();
    server.app.state.workspaces = vec![survivor, disconnected];
    server.app.state.set_bookmark_index(Some(1));
    server.app.state.ensure_test_terminals();
    let survivor_terminal = server.app.state.workspaces[0]
        .terminal_id(survivor_pane)
        .expect("survivor terminal")
        .clone();
    let terminal = server
        .app
        .state
        .terminals
        .get_mut(&survivor_terminal)
        .expect("survivor terminal state");
    terminal.set_manual_label("client-pane".into());
    terminal.set_terminal_title(Some("CLIENT OSC".into()));
    server
        .app
        .configure_window_title("{workspace}/{pane}/{terminal_title}");

    let (survivor_control, _) = connect_matching_test_shell(&mut server, 1);
    let (disconnected_control, _) = connect_matching_test_shell(&mut server, 2);
    let survivor_workspace_id = server
        .app
        .public_workspace_id(0)
        .expect("survivor workspace id");
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
    assert_eq!(server.app.state.bookmark_index(), Some(1));
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(1)),
        Some(survivor_workspace_id)
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
        Some(Some("survivor/client-pane/UPDATED OSC".into()))
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
    ClientOutbox,
    std::sync::mpsc::Receiver<Vec<u8>>,
    RenderLaneReceiver,
) {
    ClientOutbox::test_pair()
}

/// Gives an already inserted fixture client a way to send frames, so it
/// presents a surface and takes part in the PTY size rule. The receivers must
/// stay alive for as long as the client is used.
fn attach_test_writer(
    server: &mut HeadlessServer,
    client_id: u64,
) -> (std::sync::mpsc::Receiver<Vec<u8>>, RenderLaneReceiver) {
    let (writer, control, render) = test_client_writer();
    server
        .clients
        .get_mut(&ClientId::test_new(client_id))
        .expect("fixture client is registered")
        .outbox = writer;
    (control, render)
}

#[tokio::test]
async fn client_shell_attach_seeds_workspace() {
    let mut server = test_headless_server();
    server.app.state.workspaces.clear();
    server.app.state.set_bookmark_index(None);
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: ClientId::test_new(6),
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );

    assert_eq!(server.app.state.workspaces.len(), 1);
    // The connecting client is the automatic creation's trigger: the workspace
    // is sized for it and it controls it, and the client views it. No
    // navigation effect ran, so the session's bookmark is untouched.
    let created = server.app.state.workspaces[0].id;
    let client_id = ClientId::test_new(6);
    assert_eq!(
        server
            .app
            .state
            .workspace_spawn_geometry(0)
            .map(|geometry| geometry.area),
        Some(Rect::new(0, 0, 80, 23))
    );
    assert_eq!(
        server.clients.geometry_controller(&created),
        Some(client_id)
    );
    assert_eq!(server.shell_target_for_client(client_id), Some(created));
    assert_eq!(server.app.state.bookmark, None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_snapshot_presents_unknown_agent_as_idle() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("endpoint");
    let pane_id = workspace.root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
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
    server.test_handle_server_event(ServerEvent::ShellConnected {
        client_id: ClientId::test_new(78),
        surface_cols: 80,
        surface_rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        mouse_capture: false,
        surface_active: false,
        outbox: writer,
    });

    let snapshot = client_shell_snapshot(&control_rx);
    assert_eq!(
        snapshot.agents[0].agent_status,
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
    server.app.state.set_bookmark_index(Some(0));
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(41);
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let _initial_snapshot = client_shell_snapshot(&control_rx);
    let boot_id = server.client_shell_boot_id.clone();
    let workspace_id = server.app.state.workspaces[0].id;
    // A rename is a UI mutation, so each accepted request asks for a render.
    // The second arrives before the first was answered and simply runs
    // after it.
    for (request_id, label) in [("client-shell:1", "first"), ("client-shell:2", "renamed")] {
        let command = Box::new(EndpointCommand::WorkspaceRename(
            shepr_protocol::command::WorkspaceRenameParams {
                workspace_id,
                label: Some(label.into()),
            },
        ));
        assert!(
            server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
                client_id,
                boot_id: boot_id.clone(),
                request_id: request_id.into(),
                command,
            })
        );
    }
    assert!(
        control_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "a reply waits for the render its command asked for"
    );

    // The loop's order: render, then flush the held replies.
    server.render_now();
    server.release_endpoint_replies(ReleaseMode::WithinBudget);

    let ServerMessage::EndpointSnapshot(snapshot) =
        read_server_message(control_rx.recv().expect("renamed projection"))
    else {
        panic!("the projection the commands changed goes out before their replies");
    };
    assert_eq!(snapshot.workspaces[0].label, "renamed");
    for expected in ["client-shell:1", "client-shell:2"] {
        match read_server_message(control_rx.recv().expect("endpoint response")) {
            ServerMessage::ClientShellEndpointResponse {
                boot_id: response_boot_id,
                request_id,
                result,
            } => {
                assert_eq!(response_boot_id, boot_id);
                assert_eq!(request_id, expected, "replies leave in command order");
                assert!(matches!(
                    result,
                    Ok(shepr_protocol::command::EndpointReply::WorkspaceInfo { .. })
                ));
            }
            other => panic!("expected client shell endpoint response, got {other:?}"),
        }
    }
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn immediate_endpoint_replies_stay_after_earlier_commands() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("ordered")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (control, _render) = connect_matching_test_shell(&mut server, 40);
    let _initial_snapshot = client_shell_snapshot(&control);
    let client_id = ClientId::test_new(40);
    let current_boot = server.client_shell_boot_id.clone();
    let workspace_id = server.app.state.workspaces[0].id;

    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: current_boot.clone(),
        request_id: "held".into(),
        command: Box::new(EndpointCommand::WorkspaceRename(
            shepr_protocol::command::WorkspaceRenameParams {
                workspace_id,
                label: Some("updated".into()),
            },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: shepr_test_fixtures::fixed_boot_id(2),
        request_id: "stale".into(),
        command: Box::new(EndpointCommand::PaneClear(
            shepr_protocol::command::PaneTarget {
                pane_id: shepr_test_fixtures::id("w1:p1"),
            },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: current_boot.clone(),
        request_id: "deactivate".into(),
        command: Box::new(EndpointCommand::ClientShellSurfaceSet(
            shepr_protocol::command::ClientShellSurfaceSetParams { active: false },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: current_boot.clone(),
        request_id: "inactive".into(),
        command: Box::new(EndpointCommand::PaneClear(
            shepr_protocol::command::PaneTarget {
                pane_id: shepr_test_fixtures::id("w1:p1"),
            },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: current_boot.clone(),
        request_id: "activate".into(),
        command: Box::new(EndpointCommand::ClientShellSurfaceSet(
            shepr_protocol::command::ClientShellSurfaceSetParams { active: true },
        )),
    });

    server.render_now();
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let mut replies = Vec::new();
    while replies.len() < 5 {
        if let ServerMessage::ClientShellEndpointResponse {
            request_id, result, ..
        } = read_server_message(
            control
                .recv_timeout(Duration::from_secs(1))
                .expect("ordered endpoint replies"),
        ) {
            replies.push((request_id, result));
        }
    }
    assert_eq!(
        replies
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["held", "stale", "deactivate", "inactive", "activate"]
    );
    assert!(matches!(
        &replies[1].1,
        Err(shepr_protocol::command::EndpointError::StaleBoot)
    ));
    assert!(matches!(
        &replies[2].1,
        Ok(shepr_protocol::command::EndpointReply::ClientShellSurfaceSet { active: false, .. })
    ));
    assert!(matches!(
        &replies[3].1,
        Err(shepr_protocol::command::EndpointError::SurfaceInactive)
    ));
    assert!(matches!(
        &replies[4].1,
        Ok(shepr_protocol::command::EndpointReply::ClientShellSurfaceSet { active: true, .. })
    ));
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn endpoint_requests_dirty_immediate_sources_for_view_changes_only() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    server
        .app
        .state
        .workspaces
        .push(shepr_mux::workspace::Workspace::test_new("second"));
    server.app.state.ensure_test_terminals();
    let (control, _render) = connect_matching_test_shell(&mut server, 45);
    let _initial_snapshot = client_shell_snapshot(&control);
    server.immediate_pty_sources_dirty = false;
    let boot_id = server.client_shell_boot_id.clone();
    let client_id = ClientId::test_new(45);

    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: boot_id.clone(),
        request_id: "scroll".into(),
        command: Box::new(EndpointCommand::PaneScroll(
            shepr_protocol::command::PaneScrollParams {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                offset_from_bottom: 0,
            },
        )),
    });
    assert!(
        !server.immediate_pty_sources_dirty,
        "pane scrolling changes its surface but not the set of immediate PTY sources"
    );

    let second_workspace = server.app.state.workspaces[1].id;
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id,
        request_id: "focus-workspace".into(),
        command: Box::new(EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget {
                workspace_id: second_workspace,
            },
        )),
    });
    assert!(
        server.immediate_pty_sources_dirty,
        "moving a client to another workspace changes the immediate PTY sources"
    );
    server.immediate_pty_sources_dirty = false;
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: server.client_shell_boot_id.clone(),
        request_id: "deactivate-surface".into(),
        command: Box::new(EndpointCommand::ClientShellSurfaceSet(
            shepr_protocol::command::ClientShellSurfaceSetParams { active: false },
        )),
    });
    assert!(
        server.immediate_pty_sources_dirty,
        "removing an active surface changes which client views contribute sources"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn slow_checkout_root_worker_does_not_hold_other_clients() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("checkout")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (control_a, _render_a) = connect_matching_test_shell(&mut server, 51);
    let (control_b, _render_b) = connect_matching_test_shell(&mut server, 52);
    let _initial_a = client_shell_snapshot(&control_a);
    let _initial_b = client_shell_snapshot(&control_b);
    let client_a = ClientId::test_new(51);
    let client_b = ClientId::test_new(52);
    let boot_id = server.client_shell_boot_id.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
    server.workers.set_runner(std::sync::Arc::new(move |_| {
        started_tx.send(()).map_err(|error| error.to_string())?;
        let released = release_rx
            .lock()
            .ok()
            .is_some_and(|receiver| receiver.recv_timeout(Duration::from_secs(3)).is_ok());
        if !released {
            return Err("slow worker test gate timed out".to_owned());
        }
        Ok(Some("/checkout".to_owned()))
    }));

    let started = std::time::Instant::now();
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id: client_a,
        boot_id: boot_id.clone(),
        request_id: "slow-checkout".into(),
        command: Box::new(EndpointCommand::WorkspaceCheckoutRoot(
            shepr_protocol::command::WorkspaceCheckoutRootParams { cwd: "/".into() },
        )),
    });
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the request handler must return while Git work is still pending"
    );
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("checkout worker should have started");

    let workspace_id = server.app.state.workspaces[0].id;
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id: client_a,
        boot_id: boot_id.clone(),
        request_id: "after-slow".into(),
        command: Box::new(EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget { workspace_id },
        )),
    });
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id: client_b,
        boot_id,
        request_id: "other-client".into(),
        command: Box::new(EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget { workspace_id },
        )),
    });
    server.render_now();
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let mut other_client_replied = false;
    while !other_client_replied {
        if let ServerMessage::ClientShellEndpointResponse { request_id, .. } = read_server_message(
            control_b
                .recv_timeout(Duration::from_secs(1))
                .expect("the other client's command should complete"),
        ) {
            other_client_replied = request_id == "other-client";
        }
    }
    assert!(
        server.clients[&client_a]
            .outbox
            .held_reply_message(0)
            .is_none(),
        "the slow client's reserved reply should remain held"
    );
    assert!(
        server.clients[&client_a]
            .outbox
            .held_reply_message(1)
            .is_some()
    );
    release_tx
        .send(())
        .expect("checkout worker should still wait");

    let completion = tokio::time::timeout(Duration::from_secs(1), server.workers.recv())
        .await
        .expect("checkout completion should wake the loop")
        .expect("worker channel should stay open");
    assert!(!server.handle_worker_completion(completion));
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let mut client_a_replies = Vec::new();
    while client_a_replies.len() < 2 {
        if let ServerMessage::ClientShellEndpointResponse {
            request_id, result, ..
        } = read_server_message(
            control_a
                .recv_timeout(Duration::from_secs(1))
                .expect("slow client's ordered responses"),
        ) {
            client_a_replies.push((request_id, result));
        }
    }
    assert_eq!(
        client_a_replies
            .iter()
            .map(|(request_id, _)| request_id.as_str())
            .collect::<Vec<_>>(),
        ["slow-checkout", "after-slow"]
    );
    assert!(matches!(
        &client_a_replies[0].1,
        Ok(shepr_protocol::command::EndpointReply::WorkspaceCheckoutRoot {
            root: Some(root),
            ..
        }) if root == "/checkout"
    ));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pending_endpoint_replies_leave_with_their_client_and_resolve_at_shutdown() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("pending")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (control_a, _render_a) = connect_matching_test_shell(&mut server, 61);
    let (control_b, _render_b) = connect_matching_test_shell(&mut server, 62);
    let _initial_a = client_shell_snapshot(&control_a);
    let _initial_b = client_shell_snapshot(&control_b);
    let client_a = ClientId::test_new(61);
    let client_b = ClientId::test_new(62);
    let boot_id = server.client_shell_boot_id.clone();
    let refusal = |request_id: &str| {
        crate::server::client_commands::error_message(
            boot_id.clone(),
            request_id.into(),
            shepr_protocol::command::EndpointError::ShuttingDown,
        )
    };

    // A client that leaves takes its pending slot with it, and the worker
    // result that arrives for it afterwards finds nothing to fill.
    let gone = server
        .reserve_endpoint_reply(client_a, &refusal("gone"))
        .expect("reserve");
    server.remove_client(client_a);
    assert!(!server.clients.contains_key(&client_a));
    server.complete_endpoint_reply(gone, &refusal("late"));
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );

    // At shutdown a pending slot is answered with its refusal, and a reply
    // queued behind it still leaves after it.
    server.reserve_endpoint_reply(client_b, &refusal("pending"));
    server.queue_endpoint_reply(
        client_b,
        &crate::server::client_commands::response_message(
            boot_id.clone(),
            "after".into(),
            Ok(shepr_protocol::command::EndpointReply::Done),
        ),
    );
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    assert_eq!(
        server.clients[&client_b].outbox.held_reply_count(),
        2,
        "the pending slot holds the reply behind it"
    );
    server.initiate_shutdown();
    server
        .complete_shutdown()
        .await
        .expect("shutdown settles endpoint replies");
    let mut replies = Vec::new();
    while replies.len() < 2 {
        if let ServerMessage::ClientShellEndpointResponse {
            request_id, result, ..
        } = read_server_message(
            control_b
                .recv_timeout(Duration::from_secs(1))
                .expect("both replies reach the client"),
        ) {
            replies.push((request_id, result));
        }
    }
    assert_eq!(
        replies
            .iter()
            .map(|(request_id, _)| request_id.as_str())
            .collect::<Vec<_>>(),
        ["pending", "after"]
    );
    assert!(matches!(
        &replies[0].1,
        Err(shepr_protocol::command::EndpointError::ShuttingDown)
    ));
    assert!(matches!(
        read_server_message(
            control_b
                .recv_timeout(Duration::from_secs(1))
                .expect("shutdown notice follows held replies")
        ),
        ServerMessage::ServerShutdown { .. }
    ));
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn an_endpoint_error_reply_is_held_until_the_flush() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("no-render")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(42);
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let _initial_snapshot = client_shell_snapshot(&control_rx);

    // Focusing a pane that does not exist fails; its error is held like
    // any other reply until the loop flushes.
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: server.client_shell_boot_id.clone(),
        request_id: "missing-pane".into(),
        command: Box::new(EndpointCommand::PaneFocus(
            shepr_protocol::command::PaneTarget {
                pane_id: shepr_test_fixtures::id("w999:p999"),
            },
        )),
    });
    assert_eq!(server.clients[&client_id].outbox.held_reply_count(), 1);
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    let ServerMessage::ClientShellEndpointResponse {
        request_id,
        result: Err(_),
        ..
    } = read_server_message(control_rx.recv().expect("error response"))
    else {
        panic!("expected an endpoint error response");
    };
    assert_eq!(request_id, "missing-pane");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn an_endpoint_reply_for_a_departed_client_is_dropped() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("departed")];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = ClientId::test_new(43);
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let _initial_snapshot = client_shell_snapshot(&control_rx);
    let workspace_id = server.app.state.workspaces[0].id;
    server.test_handle_server_event(ServerEvent::ShellEndpointRequest {
        client_id,
        boot_id: server.client_shell_boot_id.clone(),
        request_id: "then-left".into(),
        command: Box::new(EndpointCommand::WorkspaceRename(
            shepr_protocol::command::WorkspaceRenameParams {
                workspace_id,
                label: Some("gone".into()),
            },
        )),
    });
    assert!(server.test_handle_server_event(ServerEvent::Detached { client_id }));
    server.release_endpoint_replies(ReleaseMode::WithinBudget);
    assert!(
        server
            .clients
            .iter()
            .all(|(_, client)| client.outbox.held_reply_count() == 0)
    );
    assert!(!server.clients.contains_key(&client_id));
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
    server.app.state.set_bookmark_index(Some(0));

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: ClientId::test_new(7),
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 10,
            cell_height_px: 20,
            pixel_mouse: true,
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let snapshot = client_shell_snapshot(&control_rx);
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].label, "shell-only-label");
    server.render_now();
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
    assert!(server.try_render_patches(&sources));
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
            assert_eq!(meta_panes(&patch.meta).len(), 1);
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
    assert!(server.try_render_patches(&sources));
    match read_server_message(render_rx.recv().expect("metadata-only pane surface patch")) {
        ServerMessage::SurfaceUpdate(patch) => {
            assert!(patch.spans.is_empty(), "mouse modes only change metadata");
            let panes = meta_panes(&patch.meta);
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
    server.render_now();
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
    server.app.state.set_bookmark_index(Some(0));
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
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: client_id.into(),
            surface_cols,
            surface_rows,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
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
    let _ = client_shell_snapshot(&control);
    server.render_now();
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
            server.clients[&7].shell_state().session_generation,
            generation
        );
    };

    server.render_now();
    unchanged(&server);
    assert!(control.try_recv().is_err());

    let pane_id = server
        .app
        .public_pane_id(0, server.app.state.workspaces[0].root_pane())
        .expect("pane id");
    // Scrolling a pane already at the bottom changes nothing, and the claim it
    // makes records no new geometry, so it asks for no render.
    assert!(!command_through_server(
        &mut server,
        7,
        EndpointCommand::PaneScroll(shepr_protocol::command::PaneScrollParams {
            pane_id,
            offset_from_bottom: 0,
        }),
    ));
    server.render_now();
    unchanged(&server);
    assert!(control.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

/// Runs one client-shell command for `client_id` the way the headless loop
/// does and returns whether it asked for a render.
fn command_through_server(
    server: &mut HeadlessServer,
    client_id: u64,
    command: EndpointCommand,
) -> bool {
    let epoch_before = server.view_epoch;
    let location_before = server.clients[&ClientId::test_new(client_id)]
        .shell_state()
        .location
        .generation();
    let result = server.handle_client_shell_command(ClientId::test_new(client_id), command);
    assert!(result.is_ok());
    server.view_epoch != epoch_before
        || server.clients[&ClientId::test_new(client_id)]
            .shell_state()
            .location
            .generation()
            != location_before
}

/// Renders and returns the one replacement the change must produce.
fn next_projection(
    server: &mut HeadlessServer,
    control: &std::sync::mpsc::Receiver<Vec<u8>>,
    previous: &mut shepr_protocol::ProjectionRevision,
) -> Box<shepr_protocol::ClientShellSnapshot> {
    server.render_now();
    let snapshot = client_shell_snapshot(control);
    assert!(snapshot.revision > *previous);
    *previous = snapshot.revision;
    snapshot
}

#[tokio::test]
async fn workspace_rename_reprojects_without_copying_connection_config() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let first = client_shell_snapshot(&control);
    server.render_now();
    assert!(control.try_recv().is_err());

    let outcome = server.app.handle_endpoint_command_with_render(
        EndpointCommand::WorkspaceRename(shepr_protocol::command::WorkspaceRenameParams {
            workspace_id: first.workspaces[0].workspace_id,
            label: Some("renamed".into()),
        }),
        &crate::app::EndpointContext::without_geometry(),
    );
    assert!(outcome.view_changed());
    server.render_now();
    let renamed = client_shell_snapshot(&control);
    assert_eq!(renamed.workspaces[0].label, "renamed");
    assert!(renamed.revision > first.revision);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn cwd_report_and_slow_probe_refresh_shell_projection() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    server.app.state.ensure_test_terminals();
    let pane_id = server.app.state.workspaces[0].root_pane();
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let _ = client_shell_snapshot(&control);
    server.render_now();
    assert!(control.try_recv().is_err());

    let scratch = ScratchDir::new("headless-cwd");
    let cwd = scratch.path().to_path_buf();
    let report = server.app.from_pane_runtime(
        pane_id,
        shepr_mux::events::AppEvent::TerminalCwdReported {
            pane_id,
            cwd: shepr_mux::UsableCwd::new(cwd.clone()).expect("socket directory is usable"),
        },
    );
    server.app.handle_internal_event(report);
    server.render_now();
    let reported = client_shell_snapshot(&control);
    assert_eq!(
        reported.panes[0].cwd.as_deref(),
        Some(cwd.to_str().expect("cwd utf8"))
    );

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
    server.render_now();
    assert!(control.try_recv().is_err());

    // A change no event reports (standing in for a shell's /proc cwd) is
    // found by the timer and reaches the client with the next render.
    server.app.state.workspaces[0].custom_name = Some("silent".into());
    server.render_now();
    assert!(control.try_recv().is_err(), "no event reported the change");
    age_cache(&mut server);
    assert!(server.refresh_shell_projection_sources());
    assert_eq!(
        server
            .shell_session_cache
            .as_ref()
            .expect("session cache")
            .timer_projections
            .len(),
        1,
        "the changed client's projection is retained for the render pass"
    );
    server.render_now();
    assert!(
        server
            .shell_session_cache
            .as_ref()
            .expect("session cache")
            .timer_projections
            .is_empty()
    );
    assert_eq!(
        client_shell_snapshot(&control).workspaces[0].label,
        "silent"
    );

    // The timer only runs while a shell client is connected.
    assert!(server.test_handle_server_event(ServerEvent::Disconnected {
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
    use shepr_protocol::command::{
        PaneInputSetParams, PaneRenameParams, PaneRightClickTarget, WorkspaceRenameParams,
    };

    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let pane_id = server.app.state.workspaces[0].root_pane();
    server.app.state.ensure_test_terminals();
    let public_pane_id = server.app.public_pane_id(0, pane_id).expect("pane id");
    let pane = |snapshot: &shepr_protocol::ClientShellSnapshot| {
        snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .cloned()
            .expect("projected pane")
    };
    let workspace_id = server.app.public_workspace_id(0).expect("workspace id");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let mut previous = client_shell_snapshot(&control).revision;
    server.render_now();
    assert!(control.try_recv().is_err());

    assert!(command_through_server(
        &mut server,
        7,
        EndpointCommand::PaneRename(PaneRenameParams {
            pane_id: public_pane_id,
            label: Some("manual".into()),
        }),
    ));
    let renamed = next_projection(&mut server, &control, &mut previous);
    assert_eq!(pane(&renamed).label.as_deref(), Some("manual"));

    assert!(command_through_server(
        &mut server,
        7,
        EndpointCommand::WorkspaceRename(WorkspaceRenameParams {
            workspace_id,
            label: Some("named-workspace".into()),
        }),
    ));
    let workspace = next_projection(&mut server, &control, &mut previous);
    assert_eq!(workspace.workspaces[0].label, "named-workspace");

    assert!(command_through_server(
        &mut server,
        7,
        EndpointCommand::PaneInputSet(PaneInputSetParams {
            pane_id: public_pane_id,
            right_click: PaneRightClickTarget::Pane,
        }),
    ));
    assert!(pane(&next_projection(&mut server, &control, &mut previous)).right_click_passthrough);

    let working = server.app.from_pane_runtime(
        pane_id,
        AppEvent::StateChanged {
            pane_id,
            agent: Some(shepr_agent::detect::Agent::Pi),
            state: shepr_agent::detect::AgentState::Working,
            visible_blocker: false,
            process_exited: false,
            observed_at: Instant::now(),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(working));
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

    let workspace_state_id = server.app.state.workspaces[0].id;
    let cwd = server.app.state.workspaces[0].identity_cwd.clone();
    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
            results: vec![shepr_mux::git::WorkspaceGitStatus {
                workspace_id: workspace_state_id,
                resolved_identity_cwd: cwd.clone(),
                status_cache_key: cwd,
                auto_label: "focus-reporting".into(),
                branch: shepr_mux::git::WorkspaceBranch::Named("feature".into()),
                ahead_behind: None,
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
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_reconnecting_shell_is_seeded_again_and_gets_later_changes() {
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"BASE");
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let _ = client_shell_snapshot(&control);
    server.render_now();
    assert!(server.test_handle_server_event(ServerEvent::Disconnected {
        client_id: ClientId::test_new(7),
    }));

    // The shared cache outlives the connection; the new one is seeded fresh
    // and still receives subsequent changes.
    let (control, _render) = connect_matching_test_shell(&mut server, 8);
    let seed = client_shell_snapshot(&control);
    let mut previous = seed.revision;
    server.render_now();
    assert!(control.try_recv().is_err());
    assert!(command_through_server(
        &mut server,
        8,
        EndpointCommand::WorkspaceRename(shepr_protocol::command::WorkspaceRenameParams {
            workspace_id: seed.workspaces[0].workspace_id,
            label: Some("after-reconnect".into()),
        }),
    ));
    assert_eq!(
        next_projection(&mut server, &control, &mut previous).workspaces[0].label,
        "after-reconnect"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn every_client_of_a_partly_restored_boot_gets_the_notice_in_its_seed() {
    let mut server = test_headless_server();
    let notice = shepr_protocol::SessionRestoreNotice {
        loss: shepr_protocol::SessionRestoreLoss::Workspaces {
            dropped: std::num::NonZeroUsize::MIN,
            panes_pruned: true,
        },
        backup_dir: "/state/shepr/session-backups".to_owned(),
    };
    server.app.restore_notice = Some(notice.clone());

    for client_id in [7, 8] {
        let (control, _render) = connect_matching_test_shell(&mut server, client_id);
        // The notice is part of the snapshot, so it is keyed to this boot and
        // repeated on every projection rather than sent once beside it.
        let seed = client_shell_snapshot(&control);
        assert_eq!(
            seed.restore_notice,
            Some(notice.clone()),
            "client {client_id}"
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_fully_restored_boot_sends_no_restore_notice() {
    let mut server = test_headless_server();
    let (control, _render) = connect_matching_test_shell(&mut server, 7);
    let seed = client_shell_snapshot(&control);
    assert_eq!(seed.restore_notice, None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_new_shell_seed_uses_the_shared_session_cache_for_cwd() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("cached-cwd-first");
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("cached-cwd-second");
    let second_workspace_id = second.id;
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));

    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(first_pane)
        .expect("first pane terminal")
        .clone();
    let older_cwd = shepr_test_support::ScratchDir::new("seed-cwd-older");
    let newer_cwd = shepr_test_support::ScratchDir::new("seed-cwd-newer");
    let older_cwd_text = older_cwd.path().to_str().expect("older cwd utf8");
    let newer_cwd_text = newer_cwd.path().to_str().expect("newer cwd utf8");
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("first pane terminal state")
        .set_cwd(
            shepr_mux::UsableCwd::new(older_cwd.path().to_path_buf()).expect("older cwd is usable"),
        );
    let public_pane_id = server
        .app
        .public_pane_id(0, first_pane)
        .expect("first pane public id");

    let (first_control, _first_render) = connect_matching_test_shell(&mut server, 7);
    let first_seed = client_shell_snapshot(&first_control);
    assert_eq!(
        first_seed
            .panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .expect("first pane snapshot")
            .cwd
            .as_deref(),
        Some(older_cwd_text)
    );
    server.render_now();
    assert!(first_control.try_recv().is_err());

    let cache_revision = server
        .shell_session_cache
        .as_ref()
        .expect("session cache")
        .revision;
    assert_eq!(cache_revision, server.app.state.shell_projection_revision);

    // Model an unreported cwd source moving forward without an application
    // revision, like the foreground cwd read from `/proc` between timer runs.
    // The live session now reads a newer cwd while the shared cache still has
    // the older value.
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("first pane terminal state")
        .set_cwd(
            shepr_mux::UsableCwd::new(newer_cwd.path().to_path_buf()).expect("newer cwd is usable"),
        );
    assert_eq!(
        server.app.session_snapshot().panes[0].cwd.as_deref(),
        Some(newer_cwd_text)
    );
    assert_eq!(
        server
            .shell_session_cache
            .as_ref()
            .expect("session cache")
            .session
            .panes[0]
            .cwd
            .as_deref(),
        Some(older_cwd_text)
    );

    let (control, _render) = connect_matching_test_shell(&mut server, 8);
    let seed = client_shell_snapshot(&control);
    assert_eq!(
        seed.panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .expect("seed pane snapshot")
            .cwd
            .as_deref(),
        Some(older_cwd_text)
    );

    assert!(server.place_test_client_on_workspace(ClientId::test_new(8), &second_workspace_id));
    server.render_now();
    let location_projection = client_shell_snapshot(&control);
    assert_eq!(
        location_projection
            .panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .expect("projected pane snapshot")
            .cwd
            .as_deref(),
        Some(older_cwd_text),
        "a location projection must not move cwd backwards from its seed"
    );

    assert!(server.refresh_shell_projection_sources());
    let mut previous = location_projection.revision;
    let refreshed = next_projection(&mut server, &control, &mut previous);
    assert_eq!(
        refreshed
            .panes
            .iter()
            .find(|pane| pane.pane_id == public_pane_id)
            .expect("refreshed pane snapshot")
            .cwd
            .as_deref(),
        Some(newer_cwd_text)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn create_default_workspace_invalidates_the_shell_projection() {
    let mut server = test_headless_server();
    let revision = server.app.state.shell_projection_revision;
    assert!(server.app.state.workspaces.is_empty());
    let geometry = server.app.headless_spawn_geometry();
    assert!(server.app.create_default_workspace(geometry));
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

    fn recv(&mut self, context: &str) -> DecodedServerMessage {
        let message = read_server_message(
            self.receiver
                .recv()
                .unwrap_or_else(|error| panic!("{context}: {error}")),
        );
        self.decoder
            .decode(message.clone())
            .unwrap_or(DecodedServerMessage::Wire(message))
    }
}

fn recv_pane_surface(
    receiver: &mut PaneSurfaceReceiver,
    context: &str,
) -> shepr_protocol::PaneSurfaceFrame {
    match receiver.recv(context) {
        DecodedServerMessage::Wire(ServerMessage::PaneSurface(surface)) => surface,
        DecodedServerMessage::PaneSurfacePatch(_) => {
            receiver.decoder.current_surface().expect("baseline")
        }
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
    let (control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    server.render_now();
    let before = recv_pane_surface(&mut render, "baseline");
    assert!(frame_text(&before.frame).contains("BASE"));
    let projection_before = server.clients[&7].shell_state().projection_revision.get();
    // Drain the attach and baseline control traffic.
    while control.recv_timeout(Duration::from_millis(50)).is_ok() {}

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
    server.render_now();
    assert!(render.try_recv().is_err(), "partial frame was published");
    // Only the pane surface waits for the synchronized update: the projection
    // still goes out, so a reply flushed after this render cannot overtake it.
    let ServerMessage::EndpointSnapshot(snapshot) = read_server_message(
        control
            .recv_timeout(Duration::from_secs(1))
            .expect("the projection is sent while the surface waits"),
    ) else {
        panic!("expected the changed projection");
    };
    assert_eq!(snapshot.workspaces[0].label, "renamed during frame");
    assert!(snapshot.revision.get() > projection_before);
    assert_eq!(
        server.clients[&7].shell_state().projection_revision,
        snapshot.revision
    );

    write_shared_test_pane(&mut server, pane_id, b"\rCOMPLETE\x1b[?2026l");
    assert!(!server.try_render_patches(&HashSet::from([pane_id])));
    server.render_now();
    let after = recv_pane_surface(&mut render, "completed frame");
    assert!(frame_text(&after.frame).contains("COMPLETE"));
    assert!(after.projection_revision > projection_before);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn sibling_retained_output_waits_for_synchronized_pane_to_finish() {
    let mut server = test_headless_server();
    let mut workspace = shepr_mux::workspace::Workspace::test_new("synchronized-split");
    let first = workspace.root_pane();
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
    server.app.state.set_bookmark_index(Some(0));
    let (_control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    server.render_now();
    let _ = recv_pane_surface(&mut render, "split baseline");

    write_shared_test_pane(&mut server, first, b"\x1b[?2026h\rPARTIAL");
    write_shared_test_pane(&mut server, second, b"\rUPDATED");
    assert!(!server.try_render_patches(&HashSet::from([second])));
    server.render_now();
    assert!(
        render.try_recv().is_err(),
        "sibling published partial frame"
    );

    write_shared_test_pane(&mut server, first, b"\rCOMPLETE\x1b[?2026l");
    assert!(!server.try_render_patches(&HashSet::from([first])));
    server.render_now();
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
    let hidden = workspace.root_pane();
    let visible = workspace.test_split(shepr_core::layout::Direction::Vertical);
    assert!(workspace.set_zoomed(true));

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(
        hidden,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"HIDDEN"),
    );
    server.app.insert_test_runtime(
        visible,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"VISIBLE"),
    );
    server.app.state.set_bookmark_index(Some(0));
    let (_control, render) = connect_matching_test_shell(&mut server, 7);
    let mut render = PaneSurfaceReceiver::new(render);
    write_shared_test_pane(&mut server, hidden, b"\x1b[?2026h\rPARTIAL");
    server.render_now();
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
    server.render_now();
    let _ = recv_pane_surface(&mut render, "initial surface");

    let (release, writer, revision) = {
        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rAAAA\x1b[?1003h");
        let revision = runtime.read().content_seq();
        let (release, writer) =
            runtime.test_contend_during_dirty_collection(b"\rBBBB\x1b[?1003l".to_vec());
        (release, writer, revision)
    };
    let retained = server.try_render_patches(&HashSet::from([pane_id]));
    release.send(()).expect("release waiting writer");
    let writer_took_core = writer.join().expect("writer completed");

    assert!(
        retained,
        "a waiting writer must not invalidate the collected snapshot"
    );
    assert!(
        !writer_took_core,
        "writer must wait for the collection to release the terminal core"
    );
    let patch = recv_pane_surface_patch(&mut render, "snapshot before waiting write");
    assert_eq!(meta_panes(&patch.meta)[0].content_revision, revision);
    assert!(revision.is_multiple_of(2));
    assert!(meta_panes(&patch.meta)[0].mouse_reporting);
    let surface = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("surface");
    assert!(frame_text(&surface.frame).contains("AAAA"));
    assert!(!frame_text(&surface.frame).contains("BBBB"));

    assert!(server.try_render_patches(&HashSet::from([pane_id])));
    let next = recv_pane_surface_patch(&mut render, "waiting write remains dirty");
    assert_eq!(meta_panes(&next.meta)[0].content_revision, revision + 2);
    assert!(!meta_panes(&next.meta)[0].mouse_reporting);
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
async fn different_size_shells_receive_geometry_specific_patches_from_one_dirty_collection() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (large_control, large_render) = connect_test_shell(&mut server, 7, 80, 23);
    let mut large_render = PaneSurfaceReceiver::new(large_render);
    let (small_control, small_render) = connect_test_shell(&mut server, 8, 68, 17);
    let mut small_render = PaneSurfaceReceiver::new(small_render);
    let _ = large_control.recv().expect("large snapshot");
    let _ = small_control.recv().expect("small snapshot");
    server.render_now();
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
    assert!(server.try_render_patches(&HashSet::from([pane_id])));

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
        meta_panes(&large_patch.meta)[0].inner_rect,
        meta_panes(&small_patch.meta)[0].inner_rect
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
    assert!(!server.try_render_patches(&HashSet::from([pane_id])));
    server.render_now();
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
    assert!(!server.try_render_patches(&HashSet::from([pane_id])));
    server.render_now();
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
async fn retained_patches_only_reach_shells_viewing_the_dirty_workspace() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("divergent-retained");
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("divergent-second");
    let second_pane = second.root_pane();

    server.app.state.workspaces = vec![first, second];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.state.set_bookmark_index(Some(0));
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");

    let (first_control, first_render) = connect_matching_test_shell(&mut server, 7);
    let mut first_render = PaneSurfaceReceiver::new(first_render);
    let (second_control, second_render) = connect_matching_test_shell(&mut server, 8);
    let mut second_render = PaneSurfaceReceiver::new(second_render);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(8), &second_workspace_id));
    // Both shells are the same size, so the claim moves the controller without
    // changing any recorded geometry.
    let _ = server.claim_shell_workspace_geometry(ClientId::test_new(8), false);
    assert_eq!(
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(8))
    );
    assert!(
        server.pty_sources_visible_to_any_render_target(&HashSet::from([first_pane, second_pane,]))
    );
    server.render_now();
    let first_surface = recv_pane_surface(&mut first_render, "first baseline");
    let second_surface = recv_pane_surface(&mut second_render, "second baseline");
    assert_eq!(
        (first_surface.frame.width, first_surface.frame.height),
        (80, 23)
    );
    assert_eq!(
        (second_surface.frame.width, second_surface.frame.height),
        (80, 23)
    );
    assert!(frame_text(&first_surface.frame).contains("FIRST"));
    assert!(frame_text(&second_surface.frame).contains("SECOND"));

    server
        .app
        .test_runtime(first_pane)
        .test_process_pty_bytes(b"\rFIRST_PATCH");
    assert!(server.try_render_patches(&HashSet::from([first_pane])));
    let first_patch = recv_pane_surface_patch(&mut first_render, "first patch");
    assert_eq!(meta_panes(&first_patch.meta).len(), 1);
    assert!(second_render.try_recv().is_err());

    server
        .app
        .test_runtime(second_pane)
        .test_process_pty_bytes(b"\rSECOND_PATCH");
    assert!(server.try_render_patches(&HashSet::from([second_pane])));
    let second_patch = recv_pane_surface_patch(&mut second_render, "second patch");
    assert_eq!(meta_panes(&second_patch.meta).len(), 1);
    assert!(first_render.try_recv().is_err());

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn late_retained_fallback_promotes_its_client_and_commits_no_patch_for_it() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (_first_control, first_render) = connect_matching_test_shell(&mut server, 7);
    let mut first_render = PaneSurfaceReceiver::new(first_render);
    let (_second_control, second_render) = connect_matching_test_shell(&mut server, 8);
    let mut second_render = PaneSurfaceReceiver::new(second_render);
    server.render_now();
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
    let outcome = server.render_patches(&[7.into(), 8.into()], &HashSet::from([pane_id]));
    assert_eq!(outcome.sent, vec![ClientId::test_new(7)]);
    assert_eq!(outcome.promote, vec![ClientId::test_new(8)]);
    assert!(first_render.try_recv().is_ok());
    assert!(second_render.try_recv().is_err());
    assert_ne!(
        server.clients[&7].render_state.last_pane_surface(),
        Some(&before[0])
    );
    assert_eq!(
        server.clients[&8].render_state.last_pane_surface(),
        Some(&before[1])
    );
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
    server.render_now();
    let _ = responsive_render
        .recv()
        .expect("responsive initial surface");
    let _ = slow_render.recv("slow initial surface");

    let sources = HashSet::from([pane_id]);
    write_shared_test_pane(&mut server, pane_id, b"\rONE");
    assert!(server.try_render_patches(&sources));
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
    assert!(server.try_render_patches(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive second patch")),
        ServerMessage::SurfaceUpdate(_)
    ));
    assert!(server.clients[&8].render_state.surface_debt());
    assert_eq!(
        server.clients[&8].render_state.last_pane_surface(),
        Some(&slow_baseline),
        "queue-full must not advance cells, metadata, cursor, or revision"
    );

    write_shared_test_pane(&mut server, pane_id, b"\rTHREE");
    assert!(server.try_render_patches(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive third patch")),
        ServerMessage::SurfaceUpdate(_)
    ));

    assert!(matches!(
        slow_render.recv("slow queued first patch"),
        DecodedServerMessage::PaneSurfacePatch(_)
    ));
    server.render_now();
    assert!(matches!(
        slow_render.recv("slow full recovery surface"),
        DecodedServerMessage::Wire(ServerMessage::PaneSurface(_))
            | DecodedServerMessage::PaneSurfacePatch(_)
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
    server.render_now();
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
    server.render_now();
    let _ = responsive_render
        .recv()
        .expect("responsive full replacement");
    assert!(server.clients[&8].render_state.surface_debt());

    write_shared_test_pane(&mut server, pane_id, b"\rPATCH");
    assert!(server.try_render_patches(&HashSet::from([pane_id])));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive retained patch")),
        ServerMessage::SurfaceUpdate(_)
    ));

    let _ = slow_render.recv().expect("slow queued initial surface");
    server.render_now();
    assert!(matches!(
        read_server_message(slow_render.recv().expect("slow full recovery surface")),
        ServerMessage::PaneSurface(_)
    ));

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_client_command_neither_drags_the_bookmark_nor_moves_the_clients_location() {
    use shepr_protocol::command::{PaneSelectionReadParams, PaneTextPoint};

    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("bookmark-focus-cache");
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("bookmark-focus-second");
    let second_pane = second.root_pane();
    server.app.state.workspaces = vec![first, second];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    server.app.insert_test_runtime(
        second_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let first_workspace_id = server
        .app
        .public_workspace_id(0)
        .expect("test precondition");
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");
    let first_pane_id = server
        .app
        .public_pane_id(0, first_pane)
        .expect("test precondition");

    let (control, _render) = connect_matching_test_shell(&mut server, 70);
    let initial = client_shell_snapshot(&control);
    assert_eq!(
        initial.focused_workspace_id.as_ref(),
        Some(&first_workspace_id)
    );

    // The bookmark moving on (another client's navigation, say) leaves this
    // connection's location behind: it is the client's own.
    server.app.state.set_bookmark_index(Some(1));
    server.app.state.mark_shell_projection_dirty();
    server.render_now();
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(70)),
        Some(first_workspace_id)
    );
    assert_eq!(
        server.app.state.bookmark.as_ref(),
        Some(&second_workspace_id)
    );

    let epoch_before = server.view_epoch;
    let result = server.handle_client_shell_command(
        ClientId::test_new(70),
        EndpointCommand::PaneSelectionRead(PaneSelectionReadParams {
            pane_id: first_pane_id,
            anchor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
        }),
    );
    let changed = server.view_epoch != epoch_before;
    // The pane holds no text, so the read itself is refused. No request acts
    // on the bookmark, so nothing about it or the client's view moves.
    assert_eq!(
        result,
        Err(shepr_protocol::command::EndpointError::Rejected(
            "selection text is unavailable".into()
        ))
    );
    assert!(!changed, "a refused read changes nothing to render");
    assert_eq!(
        server.app.state.bookmark.as_ref(),
        Some(&second_workspace_id)
    );
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(70)),
        Some(first_workspace_id)
    );

    server.render_now();
    let shell = server.clients[&70].shell_state();
    assert_eq!(shell.session_generation, server.shell_session_generation);
    assert_eq!(
        shell
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.focused_workspace_id.as_ref()),
        Some(&first_workspace_id),
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
    let first = shepr_mux::workspace::Workspace::test_new("independent-focus");
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("independent-focus-second");
    let second_pane = second.root_pane();
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

    server.app.state.workspaces = vec![first, second];
    server.app.insert_test_runtime(first_pane, first_runtime);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.set_bookmark_index(Some(0));
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(62), &second_workspace_id));
    server
        .clients
        .get_mut(&61)
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&62)
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = Some(true);
    // Each focused viewer's pane gains focus once.
    server.sync_pane_focus();
    assert_eq!(
        first_input.try_recv().expect("first viewer focus gained"),
        Bytes::from_static(b"\x1b[I")
    );
    assert_eq!(
        second_input.try_recv().expect("second viewer focus gained"),
        Bytes::from_static(b"\x1b[I")
    );

    let result = server.handle_client_shell_command(
        ClientId::test_new(62),
        shepr_protocol::command::EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget {
                workspace_id: second_workspace_id,
            },
        ),
    );
    assert!(result.is_ok());
    assert!(first_input.try_recv().is_err());
    assert!(second_input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_local_navigation_leaves_the_other_clients_focus_alone() {
    use shepr_protocol::command::{PaneTarget, WorkspaceCloseParams, WorkspaceTarget};

    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("focus-events");
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("focus-events-second");
    let second_pane = second.root_pane();
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let first_workspace_id = server
        .app
        .public_workspace_id(0)
        .expect("test precondition");
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");
    let first_pane_id = server
        .app
        .public_pane_id(0, first_pane)
        .expect("test precondition");
    let second_pane_id = server
        .app
        .public_pane_id(1, second_pane)
        .expect("test precondition");

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    let first_target = WorkspaceTarget {
        workspace_id: first_workspace_id,
    };
    let second_target = WorkspaceTarget {
        workspace_id: second_workspace_id,
    };
    let cases = [
        (61, EndpointCommand::WorkspaceFocus(second_target.clone())),
        (62, EndpointCommand::WorkspaceFocus(second_target.clone())),
        (61, EndpointCommand::WorkspaceFocus(second_target.clone())),
        (61, EndpointCommand::WorkspaceFocus(first_target)),
        (
            62,
            EndpointCommand::PaneFocus(PaneTarget {
                pane_id: second_pane_id,
            }),
        ),
        (
            62,
            EndpointCommand::PaneFocus(PaneTarget {
                pane_id: first_pane_id,
            }),
        ),
        (61, EndpointCommand::WorkspaceFocus(second_target.clone())),
        (
            61,
            EndpointCommand::WorkspaceClose(WorkspaceCloseParams {
                workspace_id: second_target.workspace_id,
            }),
        ),
    ];
    for (client_id, command) in cases {
        let other_client = ClientId::test_new(if client_id == 61 { 62 } else { 61 });
        let other_focus = server.shell_focus_target(other_client);
        let result = server.handle_client_shell_command(client_id.into(), command);
        assert!(result.is_ok(), "client {client_id}");

        assert_eq!(
            server.shell_focus_target(other_client),
            other_focus,
            "client {client_id}"
        );
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn navigation_moves_pane_focus_between_workspaces_once() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("navigation-focus-events");
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("navigation-focus-second");
    let second_pane = second.root_pane();
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

    server.app.state.workspaces = vec![first, second];
    server.app.insert_test_runtime(first_pane, first_runtime);
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.set_bookmark_index(Some(0));
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("second workspace id");

    let (first_control, _) = connect_matching_test_shell(&mut server, 63);
    let (second_control, _) = connect_matching_test_shell(&mut server, 64);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(64), &second_workspace_id));
    server
        .clients
        .get_mut(&63)
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&64)
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = Some(true);
    server.sync_pane_focus();
    for input in [&mut first_input, &mut second_input] {
        assert_eq!(
            input.try_recv().expect("focused viewer gained focus"),
            Bytes::from_static(b"\x1b[I")
        );
    }

    // The first viewer joins the second one's workspace: its old pane loses
    // focus, and the pane both now view, already focused, gains nothing again.
    let result = server.handle_client_shell_command(
        ClientId::test_new(63),
        shepr_protocol::command::EndpointCommand::WorkspaceFocus(
            shepr_protocol::command::WorkspaceTarget {
                workspace_id: second_workspace_id,
            },
        ),
    );
    assert!(result.is_ok());

    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(63)),
        Some(second_workspace_id)
    );
    assert_eq!(
        first_input
            .try_recv()
            .expect("previous workspace focus lost"),
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
    let first_pane = workspace.root_pane();
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
    server.app.state.set_bookmark_index(Some(0));
    let workspace_id = server.app.public_workspace_id(0).expect("workspace id");
    let first_public = server
        .app
        .public_pane_id(0, first_pane)
        .expect("first pane id");
    let second_public = server
        .app
        .public_pane_id(0, second_pane)
        .expect("second pane id");

    let (control, _) = connect_test_shell(&mut server, 65, 100, 30);
    let _ = control.recv().expect("snapshot");
    let before = server.app.test_runtime(first_pane).current_size();

    let epoch_before = server.view_epoch;
    let result = server.handle_client_shell_command(
        ClientId::test_new(65),
        shepr_protocol::command::EndpointCommand::LayoutSetSplitRatio(
            shepr_protocol::command::LayoutSetSplitRatioParams {
                workspace_id,
                first_panes: vec![first_public],
                second_panes: vec![second_public],
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
    let first_pane = workspace.root_pane();
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
    server.app.state.set_bookmark_index(Some(0));
    let second_pane_id = server
        .app
        .public_pane_id(0, second_pane)
        .expect("test precondition");

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
        Some((grown.1, grown.0))
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
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("geometry-controller-second");
    let second_pane = second.root_pane();
    let third = shepr_mux::workspace::Workspace::test_new("geometry-controller-third");
    let third_pane = third.root_pane();
    server.app.state.workspaces = vec![first, second, third];
    for pane_id in [first_pane, second_pane, third_pane] {
        server.app.insert_test_runtime(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.state.set_bookmark_index(Some(0));
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");
    let third_workspace_id = server
        .app
        .public_workspace_id(2)
        .expect("test precondition");

    let (first_control, _) = connect_test_shell(&mut server, 67, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 68, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    assert!(server.place_test_client_on_workspace(ClientId::test_new(67), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(ClientId::test_new(67), false));
    assert!(server.place_test_client_on_workspace(ClientId::test_new(67), &third_workspace_id));
    // The workspace is already sized for this client, so only the controller
    // moves and no geometry changes.
    let _ = server.claim_shell_workspace_geometry(ClientId::test_new(67), false);
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
        .outer_terminal_focus = Some(true);
    let stale_size = server.app.test_runtime(second_pane).current_size();

    assert!(server.reapply_controlled_shell_workspace_geometry(false));

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
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("controller-disconnect-second");
    let second_pane = second.root_pane();
    server.app.state.workspaces = vec![first, second];
    for pane_id in [first_pane, second_pane] {
        server.app.insert_test_runtime(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.state.set_bookmark_index(Some(0));
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");

    let (first_control, _) = connect_test_shell(&mut server, 31, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 32, 70, 20);
    let (third_control, _) = connect_test_shell(&mut server, 33, 60, 16);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    let _ = third_control.recv().expect("third snapshot");

    assert!(server.place_test_client_on_workspace(ClientId::test_new(32), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(ClientId::test_new(32), false));
    let remaining_viewer_size = server.app.test_runtime(second_pane).current_size();
    assert!(server.place_test_client_on_workspace(ClientId::test_new(31), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(ClientId::test_new(31), false));
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
    let first_pane = first.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("independent-geometry-second");
    let second_pane = second.root_pane();

    let (second_runtime, mut second_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"SECOND_WORKSPACE",
            4,
        );

    server.app.state.workspaces = vec![first, second];
    server.app.insert_test_runtime(
        first_pane,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"FIRST_WORKSPACE"),
    );
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.set_bookmark_index(Some(0));
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");
    let second_pane_id = server
        .app
        .public_pane_id(1, second_pane)
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

    assert!(server.place_test_client_on_workspace(ClientId::test_new(22), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(ClientId::test_new(22), false));
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

    assert!(server.place_test_client_on_workspace(ClientId::test_new(21), &second_workspace_id));
    assert!(server.claim_shell_workspace_geometry(ClientId::test_new(21), false));
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
async fn workspace_focus_moves_only_its_client() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("first");
    let second = shepr_mux::workspace::Workspace::test_new("second");
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let first_workspace_id = server.app.state.workspaces[0].id;
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");

    let (first_control, _) = connect_test_shell(&mut server, 41, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 42, 80, 24);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    assert!(command_through_server(
        &mut server,
        41,
        EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
            workspace_id: second_workspace_id,
        }),
    ));

    let first_location = &server.clients[&41].shell_state().location;
    let second_location = &server.clients[&42].shell_state().location;
    assert_eq!(
        first_location.focused_workspace_id(),
        Some(&second_workspace_id)
    );
    assert_eq!(
        second_location.focused_workspace_id(),
        Some(&first_workspace_id)
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_focus_replaces_a_diverged_client_shell_projection() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("first");
    let first_pane = first.root_pane();

    let second = shepr_mux::workspace::Workspace::test_new("second");
    let second_pane = second.root_pane();

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
    server.app.state.set_bookmark_index(Some(0));
    let first_workspace_id = server
        .app
        .public_workspace_id(0)
        .expect("test precondition");
    let first_pane_id = server
        .app
        .public_pane_id(0, first_pane)
        .expect("test precondition");
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");

    let (control_rx, render_rx) = connect_test_shell(&mut server, 9, 80, 23);
    let mut render_rx = PaneSurfaceReceiver::new(render_rx);
    let _ = client_shell_snapshot(&control_rx);
    assert!(server.place_test_client_on_workspace(ClientId::test_new(9), &second_workspace_id));
    // The sole shell already sized every workspace, so the claim only records
    // the controller.
    let _ = server.claim_shell_workspace_geometry(ClientId::test_new(9), false);
    assert_eq!(
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(9))
    );
    server.render_now();
    let diverged = client_shell_snapshot(&control_rx);
    assert_eq!(
        diverged.focused_workspace_id.as_ref(),
        server.app.public_workspace_id(1).as_ref()
    );
    let diverged_surface = recv_pane_surface(&mut render_rx, "diverged surface");
    assert!(frame_text(&diverged_surface.frame).contains("SECOND_WORKSPACE"));

    let result = server.handle_client_shell_command(
        ClientId::test_new(9),
        EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget {
            pane_id: first_pane_id,
        }),
    );
    let Ok(shepr_protocol::command::EndpointReply::PaneInfo { pane }) = result else {
        panic!("expected pane info");
    };
    assert_eq!(pane.pane_id, first_pane_id);
    assert_eq!(server.app.state.bookmark_index(), Some(0));
    let location = &server.clients[&9].shell_state().location;
    assert_eq!(location.focused_workspace_id(), Some(&first_workspace_id));

    server.render_now();
    let replacement = client_shell_snapshot(&control_rx);
    assert_eq!(
        replacement.focused_workspace_id.as_ref(),
        Some(&first_workspace_id)
    );
    let replacement_surface = recv_pane_surface(&mut render_rx, "pane focus replacement surface");
    assert!(frame_text(&replacement_surface.frame).contains("FIRST_AGENT"));
    assert!(!frame_text(&replacement_surface.frame).contains("SECOND_WORKSPACE"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn workspace_focus_replaces_the_client_shell_projection() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("first");
    let second = shepr_mux::workspace::Workspace::test_new("second");
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let second_id = server.app.session_snapshot().workspaces[1].workspace_id;

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.test_handle_server_event(ServerEvent::ShellConnected {
            client_id: ClientId::test_new(9),
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: true,
            outbox: writer,
        })
    );
    let initial_revision = client_shell_snapshot(&control_rx).revision;

    assert!(command_through_server(
        &mut server,
        9,
        EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
            workspace_id: second_id,
        }),
    ));
    assert_eq!(server.app.state.bookmark_index(), Some(1));
    server.render_now();

    let replacement = client_shell_snapshot(&control_rx);
    assert!(replacement.revision > initial_revision);
    assert_eq!(replacement.focused_workspace_id.as_ref(), Some(&second_id));
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
    let pane_id = focused_test_pane(&server);
    server.insert_test_client(
        11,
        ClientConnection::with_shell(
            ClientShellState::with_surface_active(true),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
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
    let pane_id = focused_test_pane(&server);

    let (workspace_index, runtime_pane_id) = server
        .app
        .resolve_pane_id(&pane_id)
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
    let visible = shepr_mux::workspace::Workspace::test_new("visible-input");
    let hidden = shepr_mux::workspace::Workspace::test_new("hidden-input");
    let hidden_pane = hidden.root_pane();
    let (runtime, mut input_rx) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[>3u",
            4,
        );

    server.app.state.workspaces = vec![visible, hidden];
    server.app.insert_test_runtime(hidden_pane, runtime);
    server.app.state.set_bookmark_index(Some(0));
    let pane_id = server
        .app
        .public_pane_id(1, hidden_pane)
        .expect("test precondition");
    server.insert_test_client(
        11,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    let _writer_lanes = attach_test_writer(&mut server, 11);
    let key = |kind| shepr_protocol::ClientPaneInputEvent::Key {
        code: shepr_protocol::ClientKeyCode::Char('x'),
        modifiers: shepr_protocol::WireModifiers::NONE,
        kind,
        repeat_count: 1,
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
    let pane_id = workspace.root_pane();
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

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.state.set_bookmark_index(Some(0));
    let public_pane_id = server
        .app
        .public_pane_id(0, pane_id)
        .expect("test precondition");
    server.insert_test_client(
        11,
        ClientConnection::with_shell(
            ClientShellState::with_surface_active(true),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
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
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
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
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    let _writer_lanes = attach_test_writer(&mut server, 11);
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(11)));
    assert!(server.claim_unowned_shell_workspace_geometry(ClientId::test_new(11), false));

    let render_impact = server.test_handle_server_event(ServerEvent::ShellPaneInput {
        client_id: ClientId::test_new(11),
        pane_id,
        events: vec![shepr_protocol::ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            geometry: None,
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
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
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
            geometry: None,
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
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            writer,
        ),
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

pub(crate) fn install_focused_test_runtime(
    server: &mut HeadlessServer,
    terminal_bytes: &[u8],
) -> tokio::sync::mpsc::Receiver<Bytes> {
    let workspace = shepr_mux::workspace::Workspace::test_new("focus-reporting");
    let pane_id = workspace.root_pane();
    let (runtime, input_rx) = shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
        80,
        24,
        0,
        terminal_bytes,
        4,
    );

    server.app.state.workspaces = vec![workspace];
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.state.set_bookmark_index(Some(0));
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
    server.app.state.set_bookmark_index(Some(0));

    let (client_tx, client_control_rx, client_rx) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            client_tx,
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(1)));
    assert!(server.claim_unowned_shell_workspace_geometry(ClientId::test_new(1), true));

    (server, client_control_rx, client_rx, pane_id)
}

#[test]
fn client_shell_host_theme_follows_foreground_client() {
    let mut server = test_headless_server();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            crate::server::outbox::ClientOutbox::detached(),
        ),
    );
    server.insert_test_client(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
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
        !server.test_handle_server_event(ServerEvent::ShellHostTheme {
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
    assert!(server.sync_host_theme_from_foreground());
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

#[test]
fn resizing_a_background_shell_does_not_change_foreground_or_host_theme() {
    let mut server = test_headless_server();
    let (first_writer, first_control, _first_render) = test_client_writer();
    let (second_writer, second_control, _second_render) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            first_writer,
        ),
    );
    server.insert_test_client(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
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
        server.app.state.host_terminal_theme.background,
        Some(first_background.into())
    );

    assert!(server.test_handle_server_event(ServerEvent::ShellResize {
        client_id: ClientId::test_new(2),
        surface_cols: 100,
        surface_rows: 30,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    assert_eq!(
        server.clients.foreground_client_id(),
        Some(ClientId::test_new(1))
    );
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(first_background.into())
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
    let pane_id = workspace.root_pane();
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
    let workspace_id = workspace.id;
    let cwd = workspace.identity_cwd.clone();
    workspace.admit_git_status(
        shepr_mux::git::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd.clone(),
            auto_label: "cached".into(),
            branch: shepr_mux::git::WorkspaceBranch::OutsideRepository,
            ahead_behind: None,
        },
        Some(&cwd),
    );
    server.app.state.workspaces.push(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        results: vec![shepr_mux::git::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            auto_label: "cached".into(),
            branch: shepr_mux::git::WorkspaceBranch::OutsideRepository,
            ahead_behind: None,
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
    let workspace_id = workspace.id;
    let cwd = workspace.identity_cwd.clone();
    server.app.state.workspaces.push(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        results: vec![shepr_mux::git::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            auto_label: "one".into(),
            branch: shepr_mux::git::WorkspaceBranch::Named("changed".into()),
            ahead_behind: None,
        }],
        cache_updates: Vec::new(),
    });

    assert!(changed);
}

#[tokio::test]
async fn host_shutdown_warning_freezes_saves_before_applying_events_and_thaws_on_cancel() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("host-shutdown");
    let pane_id = workspace.root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(true, Ordering::Release);
    // The test policy never saves, so the checkpoint writes nothing and the
    // real session file is untouched.
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Frozen);
    assert_eq!(server.lifecycle.frozen_session_policy(), Some(false));
    // Pretend saving was on before the warning, so the thaw has to restore it.
    server.lifecycle.set_frozen_session_policy_for_test(true);
    assert!(!server.app.policy.persists_session());
    assert!(server.app.session_saver.autosave_deadline().is_none());

    // The server keeps running and applies pane deaths; only the disk is frozen.
    server.app.insert_idle_test_runtime(pane_id);
    let died = server.app.from_pane_runtime(
        pane_id,
        AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Exited,
            ended_at: std::time::Instant::now(),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(died));
    assert!(server.app.find_pane(pane_id).is_none());
    assert!(!server.app.policy.persists_session());

    // Cancellation reported through the flag thaws and re-saves current state.
    server.app.state.session_dirty = false;
    server
        .lifecycle
        .host_shutdown_request_flag()
        .store(false, Ordering::Release);
    server.lifecycle.sync_host_shutdown_freeze(&mut server.app);
    assert_eq!(server.lifecycle.phase(), ShutdownPhase::Running);
    assert!(server.app.policy.persists_session());
    assert!(server.app.state.session_dirty);
    // Not stopping: the warning alone never ends the server.
    assert!(!server.lifecycle.stop_requested());
    server.app.policy = crate::app::AppPolicy::Suspended;
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn a_surface_larger_than_one_frame_crosses_in_parts() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("oversized");
    let pane_id = workspace.root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    // One displayed grapheme large enough that the encoded surface is past
    // one frame: the surface is split, not refused.
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

    server.render_now();
    let bytes = render_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("the large surface was queued");
    assert!(bytes.len() > MAX_FRAME_SIZE);
    let first_prefix = u32::from_le_bytes(bytes[..4].try_into().expect("test precondition"));
    assert_ne!(first_prefix & (1 << 31), 0, "the first frame is continued");
    let ServerMessage::PaneSurface(surface) = read_server_message(bytes) else {
        panic!("expected a full pane surface");
    };
    assert!(
        surface
            .frame
            .cells
            .iter()
            .any(|cell| cell.symbol.len() > MAX_FRAME_SIZE),
        "the large grapheme arrives whole"
    );
    assert!(
        std::iter::from_fn(|| control.recv_timeout(Duration::from_millis(200)).ok())
            .map(read_server_message)
            .all(|message| !matches!(message, ServerMessage::ClientShellError { .. })),
        "a split surface is not reported as too large"
    );
    assert!(
        !server
            .clients
            .get(&91)
            .expect("client stays connected")
            .oversized_surface_reported
    );
    assert!(
        shepr_protocol::NoticeKind::LimitExceeded(shepr_protocol::LimitExceeded::new(
            shepr_protocol::Limit::new(
                shepr_protocol::LimitKind::SurfaceMessageBytes,
                shepr_protocol::MAX_MESSAGE_SIZE,
            ),
            3_000_000,
        ))
        .to_string()
        .contains("too large")
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn signal_quit_drain_keeps_dying_panes_in_the_layout() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("signal-quit");
    let pane_id = workspace.root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    // From the pane's live runtime, so admission passes it and only the
    // signal quit keeps the pane.
    server.app.insert_idle_test_runtime(pane_id);
    let died = server.app.from_pane_runtime(
        pane_id,
        AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Exited,
            ended_at: std::time::Instant::now(),
        },
    );
    server
        .app
        .event_tx
        .try_send(died)
        .expect("test precondition");
    server
        .lifecycle
        .signal_quit_request_flag()
        .set(std::time::Instant::now())
        .expect("the first signal");
    server.lifecycle.stop_signal().request();

    // The quit-path drain still consumes the queue ...
    server.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_CHANNEL_CAPACITY);
    assert!(server.app.event_rx.try_recv().is_err());
    // ... but the pane stays in the layout the final save captures.
    assert!(server.app.find_pane(pane_id).is_some());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_death_reconciles_each_client_view_and_focus() {
    let mut server = test_headless_server();
    let doomed = shepr_mux::workspace::Workspace::test_new("pane-death-views");
    let dead_pane = doomed.root_pane();
    let second = shepr_mux::workspace::Workspace::test_new("pane-death-views-second");
    let second_pane = second.root_pane();
    let (second_runtime, mut second_input) =
        shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );

    server.app.state.workspaces = vec![doomed, second];
    server.app.insert_test_runtime(second_pane, second_runtime);
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("second workspace id");

    let (first_control, _) = connect_test_shell(&mut server, 71, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 72, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.place_test_client_on_workspace(ClientId::test_new(72), &second_workspace_id));
    server
        .clients
        .get_mut(&71)
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = Some(true);
    server
        .clients
        .get_mut(&72)
        .expect("test precondition")
        .shell_state_mut()
        .outer_terminal_focus = Some(false);

    server.app.insert_idle_test_runtime(dead_pane);
    let died = server.app.from_pane_runtime(
        dead_pane,
        AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: shepr_platform::ChildExitReason::Exited,
            ended_at: std::time::Instant::now(),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(died));

    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(71)),
        Some(second_workspace_id)
    );
    assert_eq!(
        server.shell_target_for_client(ClientId::test_new(72)),
        Some(second_workspace_id)
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
        server.clients.geometry_controller(&second_workspace_id),
        Some(ClientId::test_new(71))
    );
    let before_resize = server.app.test_runtime(second_pane).current_size();
    assert!(server.test_handle_server_event(ServerEvent::ShellResize {
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
    let first_pane = workspace.root_pane();
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
    server.app.state.set_bookmark_index(Some(0));

    let (control, _) = connect_test_shell(&mut server, 73, 185, 46);
    let _ = control.recv().expect("snapshot");
    let shrunk = server.app.test_runtime(first_pane).current_size();
    assert!(shrunk.0 < 46);

    let died = server.app.from_pane_runtime(
        dead_pane,
        AppEvent::PaneDied {
            pane_id: dead_pane,
            exit_reason: shepr_platform::ChildExitReason::Exited,
            ended_at: std::time::Instant::now(),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(died));

    let runtime = &server.app.test_runtime(first_pane);
    let grown = runtime.current_size();
    assert!(grown.0 > shrunk.0);
    assert_eq!(
        runtime.read().terminal_dimensions(),
        Some((grown.1, grown.0))
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
    let (mut runtime, mut input_rx) =
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
    let (mut runtime, mut input_rx) =
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
            geometry: None,
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
            geometry: None,
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

#[tokio::test]
async fn headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client() {
    let mut server = test_headless_server();
    // Keep a shell reading its PTY so the resume command cannot race the
    // default test shell, which exits at once, before the input is queued.
    server
        .app
        .set_test_shell(shepr_test_support::fixture::idle_shell());
    let workspace = shepr_mux::workspace::Workspace::test_new("restored");
    let pane_id = workspace.root_pane();
    let terminal_id = workspace
        .terminal_id(pane_id)
        .cloned()
        .expect("test precondition");
    server.app.state.workspaces = vec![workspace];
    server.app.state.set_bookmark_index(Some(0));
    server.app.state.ensure_test_terminals();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .plan_agent_resume(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            vec![crate::app::exiting_test_command().into()],
        ));

    server.render_now();
    assert_eq!(
        server.app.state.workspace_area(0),
        Some(server.app.state.settings.headless_rect())
    );

    let now = Instant::now();
    assert!(!server.handle_scheduled_tasks_headless(now));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
    let deadline = server
        .app
        .pending_agent_resume_wakeup()
        .expect("clientless resume should wait briefly for a host theme");

    assert!(server.handle_scheduled_tasks_headless(deadline));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_some());
    settle_resume_launch(&mut server, &terminal_id).await;
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
    server
        .app
        .set_test_shell(shepr_test_support::fixture::idle_shell());
    let workspace = shepr_mux::workspace::Workspace::test_new("restored");
    let pane_id = workspace.root_pane();
    let terminal_id = workspace
        .terminal_id(pane_id)
        .cloned()
        .expect("test precondition");
    server.app.state.workspaces = vec![workspace];
    server.app.state.set_bookmark_index(Some(0));
    server.app.state.ensure_test_terminals();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .plan_agent_resume(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            vec![crate::app::exiting_test_command().into()],
        ));
    server.render_now();

    let now = Instant::now();
    assert!(!server.handle_scheduled_tasks_headless(now));
    let deadline = server
        .app
        .pending_agent_resume_wakeup()
        .expect("clientless resume should arm the theme wait");
    for step in 1..5 {
        let tick = now + Duration::from_millis(step * 100);
        assert!(
            tick < deadline,
            "test ticks must stay inside the theme wait"
        );
        assert!(!server.handle_scheduled_tasks_headless(tick));
        assert_eq!(server.app.pending_agent_resume_wakeup(), Some(deadline));
    }

    assert!(server.handle_scheduled_tasks_headless(deadline));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_some());
    shutdown_test_runtimes(&mut server);
}

/// Hands the app its queued runtime events, as the headless loop does, until
/// the resume's shell launch settled and typed its command (the plan is
/// consumed then, not at dispatch).
async fn settle_resume_launch(
    server: &mut HeadlessServer,
    terminal_id: &shepr_protocol::TerminalId,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while server.app.state.terminals[terminal_id]
        .agent_resume()
        .is_pending()
    {
        let event = tokio::time::timeout_at(deadline, server.app.event_rx.recv())
            .await
            .expect("the resume launch settles")
            .expect("the event channel stays open");
        server.app.handle_internal_event(event);
    }
}

#[test]
fn client_shell_streams_focused_pane_report_all_demand() {
    with_terminal_session_test_server(|server, terminal_id, _terminal_id_string, _pane_id| {
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.insert_test_client(
            1,
            ClientConnection::new(
                (80, 24),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                1,
                client_tx,
            ),
        );
        server.app.state.set_bookmark_index(Some(0));
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
    let pane_id = focused_test_pane(&server);
    for client_id in [1, 2] {
        server.insert_test_client(
            client_id,
            ClientConnection::new(
                (80, 24),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
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
        repeat_count: 1,
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
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            writer,
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
        server.app.state.set_bookmark_index(Some(0));
        server.insert_test_client(
            1,
            ClientConnection::new(
                (80, 24),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                1,
                crate::server::outbox::ClientOutbox::detached(),
            ),
        );
        server.insert_test_client(
            2,
            ClientConnection::new(
                (100, 30),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                2,
                crate::server::outbox::ClientOutbox::detached(),
            ),
        );
        let _first_lanes = attach_test_writer(server, 1);
        let _second_lanes = attach_test_writer(server, 2);
        server
            .clients
            .set_foreground_client_id(Some(ClientId::test_new(2)));
        assert!(server.claim_unowned_shell_workspace_geometry(ClientId::test_new(2), true));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("focused runtime")
                .current_size(),
            (30, 99)
        );

        assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
            client_id: ClientId::test_new(1),
            focused: true,
        }));
        assert_eq!(
            server.clients.foreground_client_id(),
            Some(ClientId::test_new(1))
        );
        assert_eq!(
            outer_terminal_focus(server, ClientId::test_new(1)),
            Some(true)
        );
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

        assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
            client_id: ClientId::test_new(2),
            focused: true,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "second viewer duplicated focus gain"
        );
        assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
            client_id: ClientId::test_new(1),
            focused: false,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "remaining viewer lost pane focus"
        );
        assert!(server.test_handle_server_event(ServerEvent::ShellFocus {
            client_id: ClientId::test_new(2),
            focused: false,
        }));
        assert_eq!(
            outer_terminal_focus(server, ClientId::test_new(2)),
            Some(false)
        );
        assert_eq!(
            input_rx.try_recv().expect("last viewer focus lost input"),
            Bytes::from_static(b"\x1b[O")
        );
    });
}

/// A clipboard write from a pane that no client views (its workspace is no
/// client's location): the fallback that sends it to the foreground client.
/// The pane has a live runtime, since admission drops a write from anywhere
/// else before forwarding is reached.
fn unviewed_clipboard_write(server: &mut HeadlessServer) -> AppEvent {
    let workspace = shepr_mux::workspace::Workspace::test_new("unviewed-clipboard");
    let pane_id = workspace.root_pane();
    server.app.state.workspaces.push(workspace);
    server.app.state.ensure_test_terminals();
    server.app.insert_idle_test_runtime(pane_id);
    server.app.from_pane_runtime(
        pane_id,
        AppEvent::ClipboardWrite {
            pane_id,
            content: b"test".to_vec(),
        },
    )
}

#[tokio::test]
async fn clipboard_write_goes_to_the_clients_viewing_the_writing_pane() {
    let mut server = test_headless_server();
    let first = shepr_mux::workspace::Workspace::test_new("clipboard-viewers");
    let second = shepr_mux::workspace::Workspace::test_new("clipboard-viewers-second");
    let second_pane = second.root_pane();
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));
    let second_workspace_id = server
        .app
        .public_workspace_id(1)
        .expect("test precondition");

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
        AppEvent::ClipboardWrite {
            pane_id: second_pane,
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
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            background_tx,
        ),
    );
    server.insert_test_client(
        2,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
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
            shepr_termio::host_term::cell_size::HostCellSize::default(),
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
        server.clients.contains_key(&1),
        "closure is latched at the reap"
    );
    assert!(server.reap_closed_clients());
    assert!(!server.clients.contains_key(&1));
}

#[tokio::test]
async fn unchanged_internal_events_leave_projection_and_sources_clean() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("event-effects");
    let pane_id = workspace.root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(pane_id)
        .expect("terminal")
        .clone();
    let cwd = server.app.state.terminals[&terminal_id].cwd().to_path_buf();
    // Every event comes from the pane's live runtime, so an unchanged result
    // below is the reducer finding nothing new, not admission dropping it.
    server.app.insert_idle_test_runtime(pane_id);
    let from_runtime =
        |server: &HeadlessServer, event: AppEvent| server.app.from_pane_runtime(pane_id, event);
    let working = |server: &HeadlessServer| {
        from_runtime(
            server,
            AppEvent::StateChanged {
                pane_id,
                agent: Some(shepr_agent::detect::Agent::Codex),
                state: shepr_agent::detect::AgentState::Working,
                visible_blocker: false,
                process_exited: false,
                observed_at: server.app.clock.now,
            },
        )
    };
    server.immediate_pty_sources_dirty = false;
    server.host_input_modes_dirty = false;
    let before = server.app.state.shell_projection_revision;
    let same_cwd = from_runtime(
        &server,
        AppEvent::TerminalCwdReported {
            pane_id,
            cwd: shepr_mux::UsableCwd::new(cwd).expect("absolute cwd"),
        },
    );
    assert!(!server.handle_internal_event_with_forwarding(same_cwd));
    assert_eq!(server.app.state.shell_projection_revision, before);
    assert!(!server.immediate_pty_sources_dirty);
    assert!(!server.host_input_modes_dirty);

    // The same events do invalidate once they change what a client sees.
    let moved = shepr_test_support::ScratchDir::new("event-effects-cwd");
    let moved_cwd = from_runtime(
        &server,
        AppEvent::TerminalCwdReported {
            pane_id,
            cwd: shepr_mux::UsableCwd::new(moved.path().to_path_buf()).expect("absolute cwd"),
        },
    );
    assert!(server.handle_internal_event_with_forwarding(moved_cwd));
    assert_ne!(server.app.state.shell_projection_revision, before);
    let before = server.app.state.shell_projection_revision;
    let event = working(&server);
    assert!(server.handle_internal_event_with_forwarding(event));
    assert_ne!(server.app.state.shell_projection_revision, before);
    let before = server.app.state.shell_projection_revision;
    let event = working(&server);
    assert!(!server.handle_internal_event_with_forwarding(event));
    assert_eq!(server.app.state.shell_projection_revision, before);
    assert!(!server.immediate_pty_sources_dirty);
    assert!(!server.host_input_modes_dirty);
}

#[tokio::test]
async fn missing_pane_exit_has_no_invalidation() {
    let mut server = test_headless_server();
    server.immediate_pty_sources_dirty = false;
    server.host_input_modes_dirty = false;
    let before = server.app.state.shell_projection_revision;
    // A late exit from the runtime of a pane already gone from the layout: the
    // runtime went with the pane, so admission finds no producer for it.
    let pane_id = shepr_core::layout::PaneId::alloc();
    assert!(
        !server.handle_internal_event_with_forwarding(AppEvent::Runtime {
            pane_id,
            generation: shepr_mux::events::RuntimeGeneration::alloc(),
            event: Box::new(AppEvent::PaneDied {
                pane_id,
                exit_reason: shepr_platform::ChildExitReason::Exited,
                ended_at: std::time::Instant::now(),
            }),
        })
    );
    assert_eq!(server.app.state.shell_projection_revision, before);
    assert!(!server.immediate_pty_sources_dirty);
    assert!(!server.host_input_modes_dirty);
}

#[tokio::test]
async fn a_failed_health_pong_leaves_no_ghost_client() {
    let mut server = test_headless_server();
    let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 1, 1);
    let reader = outbox.control_sender();
    let client_id = ClientId::test_new(1);
    let workspace = shepr_mux::workspace::Workspace::test_new("health");
    let workspace_id = workspace.id;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.insert_test_client(
        client_id,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            outbox,
        ),
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
    assert_eq!(server.app_client_count(), 0);
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
                shepr_termio::host_term::cell_size::HostCellSize::default(),
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
        server.app.state.host_terminal_theme.background,
        Some(colors[1].into())
    );
    let epoch = server.view_epoch;
    server.clients[&2].outbox.close();
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
        server.app.state.host_terminal_theme.background,
        Some(colors[0].into())
    );
    assert_ne!(server.view_epoch, epoch);
    assert!(!server.reap_closed_clients());
}

#[test]
fn a_stopping_server_reaps_closed_clients_without_reapplying_geometry() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new("stopping")];
    server.app.state.ensure_test_terminals();
    let area = Rect::new(0, 0, 17, 9);
    server.app.state.test_record_all_workspace_areas(area);
    let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 16, 1024);
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            outbox,
        ),
    );
    server.lifecycle.begin_stopping();
    server.clients[&1].outbox.close();
    assert!(server.reap_closed_clients());
    assert_eq!(server.app.state.workspace_area(0), Some(area));
}

impl HeadlessServer {
    fn test_handle_server_event(&mut self, event: ServerEvent) -> bool {
        let before = self.view_epoch;
        let clients_before = self.clients.iter().count();
        let views_before = self
            .clients
            .iter()
            .map(|(&id, client)| {
                (
                    id,
                    client.shell_state().location.generation(),
                    client.terminal_size,
                    client.shell_state().outer_terminal_focus,
                    client.render_state.is_settled_at(ViewEpoch::ZERO),
                )
            })
            .collect::<Vec<_>>();
        self.handle_server_event(event);
        let views_after = self
            .clients
            .iter()
            .map(|(&id, client)| {
                (
                    id,
                    client.shell_state().location.generation(),
                    client.terminal_size,
                    client.shell_state().outer_terminal_focus,
                    client.render_state.is_settled_at(ViewEpoch::ZERO),
                )
            })
            .collect::<Vec<_>>();
        self.view_epoch != before
            || clients_before != self.clients.iter().count()
            || views_before != views_after
    }
    fn test_drain_server_events(&mut self) -> bool {
        let before = self.view_epoch;
        self.drain_server_events();
        self.view_epoch != before
    }
}

#[test]
fn a_completion_for_a_departed_client_is_dropped() {
    let mut server = test_headless_server();
    for id in [1, 2] {
        let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 16, 1 << 20);
        server.insert_test_client(
            id,
            ClientConnection::new(
                (80, 24),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                id,
                outbox,
            ),
        );
    }
    let refusal = ServerMessage::WindowTitle {
        title: Some("refusal".into()),
    };
    let departing = server
        .reserve_endpoint_reply(ClientId::test_new(1), &refusal)
        .expect("reserved for the departing client");
    let survivor = server
        .reserve_endpoint_reply(ClientId::test_new(2), &refusal)
        .expect("reserved for the survivor");
    // Sequences are per outbox, so only the client id tells the two apart.
    assert_eq!(departing.seq, survivor.seq);
    server.remove_client(ClientId::test_new(1));
    server.complete_endpoint_reply(
        departing,
        &ServerMessage::WindowTitle {
            title: Some("late".into()),
        },
    );
    assert!(!server.clients.contains_key(&ClientId::test_new(1)));
    assert_eq!(server.clients[&2].outbox.held_reply_count(), 1);
    assert_eq!(
        server.clients[&2].outbox.held_reply_message(0),
        None,
        "the survivor's reply is still pending"
    );
}

#[test]
fn a_reaped_client_marks_the_view_changed() {
    let mut server = test_headless_server();
    let outbox = ClientOutbox::test_buffered(Arc::clone(&server.outbox_wake), 16, 1024);
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            outbox,
        ),
    );
    let epoch = server.view_epoch;
    server.clients[&1].outbox.close();
    assert_eq!(server.view_epoch, epoch);
    assert!(server.reap_closed_clients());
    assert_ne!(server.view_epoch, epoch);
    let settled = server.view_epoch;
    assert!(!server.reap_closed_clients());
    assert_eq!(server.view_epoch, settled);
}
