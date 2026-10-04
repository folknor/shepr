use super::*;
use crate::test_support::*;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::server::outbox::{ClientOutbox, RenderLaneReceiver};
use bytes::Bytes;
use shepr_core::geometry::Rect;
use shepr_protocol::MAX_FRAME_SIZE;
use shepr_protocol::command::EndpointCommand;
use shepr_surface::decode::DecodedServerMessage;

mod agent_resume;
mod already_running;
mod api_requests;
mod client_input;
mod clients;
mod clipboard;
mod endpoint_commands;
mod geometry;
mod host_theme;
mod internal_event_drain;
mod locations;
mod navigation;
mod pane_exit;
mod pixel_mouse;
mod projection;
mod retained_render;
mod server_stop;
mod shutdown;
mod surface_delta;
mod surface_interest;
mod window_title;

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

/// Turns window titles on for `server`, as `ui.window_title` does.
pub(crate) fn enable_window_title(server: &mut HeadlessServer, template: &str) {
    server.window_title = crate::ui::WindowTitleSettings::for_test(template);
}

pub(crate) fn outer_terminal_focus(
    server: &HeadlessServer,
    client_id: crate::server::ClientId,
) -> Option<bool> {
    server.clients.get(&client_id).and_then(|client| {
        match client.shell_state().outer_terminal_focus {
            crate::server::clients::OuterFocus::Unreported => None,
            crate::server::clients::OuterFocus::Focused => Some(true),
            crate::server::clients::OuterFocus::Unfocused => Some(false),
        }
    })
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
                    geometry: geometry.host_geometry().expect("valid test geometry"),
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
    let mut app = crate::app::App::new(&config);
    app.set_test_shell(crate::app::exiting_test_command());
    let (app, outputs) = app.into_parts();
    let server_events = mpsc::channel(crate::limits::SERVER_EVENT_CHANNEL_CAPACITY);
    let (api_tx, api_request_rx) = mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
    // Production's listener holds the sender for the server's whole life; a
    // fixture that dropped it would start every test loop with a closed API
    // channel. Tests that need to send swap in a channel of their own.
    std::mem::forget(api_tx);
    let stop_signal = Arc::new(shepr_api::ServerStopSignal::default());

    HeadlessServer::assemble(
        app,
        outputs,
        api_request_rx,
        None,
        stop_signal,
        shepr_test_fixtures::fixed_boot_id(1),
        None,
        server_events,
    )
}

impl HeadlessServer {
    /// Replaces both the app and the outputs it publishes through, as one
    /// harness (`TestApp`): the pair always belongs together.
    pub(crate) fn install_test_app(&mut self, app: crate::app::TestApp) {
        let (app, outputs) = app.into_parts();
        self.app = app;
        self.outputs = outputs;
    }

    /// Turns the server's app into a persisting one, fired through the
    /// server's own `save_finished` signal (`TestApp::persist` for an app
    /// held outside a server).
    pub(crate) fn persist_for_test(&mut self) {
        let save_finished = self.outputs.save_finished_signal();
        self.app.persist_with_signal(save_finished);
    }

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
        let Some(index) = self.app.state().workspaces().position(workspace_id) else {
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
        .state()
        .pane(server.app.state().ws(0).tree().focused())
        .expect("test precondition")
        .public_id()
}

pub(crate) fn shutdown_test_runtimes(server: &mut HeadlessServer) {
    for (_, runtime) in server.app.test_runtimes_mut().drain() {
        drop(runtime);
    }
}

pub(crate) fn read_server_message(bytes: Vec<u8>) -> ServerMessage {
    let mut cursor = std::io::Cursor::new(bytes);
    shepr_protocol::read_message(&mut cursor).expect("decode server message")
}

fn frame_text(frame: &FrameData) -> String {
    frame
        .cells()
        .chunks(usize::from(frame.width()))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
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

fn install_shared_view_test_runtime(server: &mut HeadlessServer) -> shepr_core::layout::PaneId {
    let workspace = shepr_mux::workspace::Workspace::test_new("shared-view");
    let pane_id = workspace.tree().focused();

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 23, b"BASE"),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));
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
            geometry: shepr_core::geometry::HostGeometry::new(
                shepr_core::geometry::GridSize::clamped(surface_cols, surface_rows),
                shepr_core::geometry::HostCell::Unknown
            ),
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
        .pane_runtime(pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(bytes);
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

/// Pairs a render receiver with the decoder that unwraps its surface reuse
/// and delta messages: the server encodes those against the last full
/// surface it sent on that connection, so decoding them here needs the same
/// running baseline a real endpoint client would keep.
struct PaneSurfaceReceiver {
    receiver: RenderLaneReceiver,
    decoder: shepr_surface::decode::Decoder,
}

impl PaneSurfaceReceiver {
    fn new(receiver: RenderLaneReceiver) -> Self {
        Self {
            receiver,
            decoder: shepr_surface::decode::Decoder::default(),
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

pub(crate) fn install_focused_test_runtime(
    server: &mut HeadlessServer,
    terminal_bytes: &[u8],
) -> tokio::sync::mpsc::Receiver<Bytes> {
    let workspace = shepr_mux::workspace::Workspace::test_new("focus-reporting");
    let pane_id = workspace.tree().root();
    let (runtime, input_rx) = shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
        80,
        24,
        0,
        terminal_bytes,
        4,
    );

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.test_state_mut().seed_bookmark_index(Some(0));
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
    let pane_id = workspace.tree().focused();

    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, initial_screen),
    );
    server.app.test_state_mut().seed_bookmark_index(Some(0));

    let (client_tx, client_control_rx, client_rx) = test_client_writer();
    server.insert_test_client(
        1,
        ClientConnection::new(
            (80, 24),
            shepr_core::geometry::HostCell::Unknown,
            1,
            client_tx,
        ),
    );
    server
        .clients
        .set_foreground_client_id(Some(ClientId::test_new(1)));
    assert!(server.claim_unowned_shell_workspace_geometry(
        ClientId::test_new(1),
        client_views::PendingResumes::Start
    ));

    (server, client_control_rx, client_rx, pane_id)
}

fn with_terminal_session_test_server(
    test: impl FnOnce(&mut HeadlessServer, shepr_core::layout::PaneId, String, String),
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("test");
    let pane_id = workspace.tree().root();
    let pane_id_string = pane_id.to_string();
    let public_pane_id = format!("{}:p1", workspace.id());
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.test_runtimes_mut().insert(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
    );

    test(&mut server, pane_id, pane_id_string, public_pane_id);

    drop(server);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

/// A clipboard write from a pane that no client views (its workspace is no
/// client's location): the fallback that sends it to the foreground client.
/// The pane has a live runtime, since admission drops a write from anywhere
/// else before forwarding is reached.
fn unviewed_clipboard_write(server: &mut HeadlessServer) -> AppEvent {
    let workspace = shepr_mux::workspace::Workspace::test_new("unviewed-clipboard");
    let pane_id = workspace.tree().root();
    server.app.test_state_mut().test_push_workspace(workspace);
    server.app.insert_idle_test_runtime(pane_id);
    server.app.from_pane_runtime(
        pane_id,
        shepr_mux::events::RuntimeEvent::ClipboardWrite {
            content: b"test".to_vec(),
        },
    )
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
                    client.render_state.has_settled(),
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
                    client.render_state.has_settled(),
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
