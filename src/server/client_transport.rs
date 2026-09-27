//! Blocking client socket transport for the headless server.
//!
//! This module owns the thin-client handshake, read loop, and writer loop.
//! It converts socket I/O into [`ServerEvent`] values consumed by
//! `HeadlessServer`.

use crate::server::ClientId;
use crate::server::input_wire::WirePaneInput;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SendError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use interprocess::TryClone as _;
use interprocess::local_socket::traits::Stream as _;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use shepr_platform::ipc::LocalStream;
use shepr_protocol::endpoint::EndpointServerWelcome;
use shepr_protocol::{
    self, AttachScrollDirection, AttachScrollSource, ClientMessage, ClientPaneInputEvent,
    MAX_FRAME_SIZE, MAX_INPUT_PAYLOAD, ServerMessage,
};

/// Minimum accepted attached client size.
///
/// Narrow observers must be allowed to drive narrow renders, otherwise the
/// server wraps pane content against a wider width and the client sees the
/// right edge clipped.
const MIN_CLIENT_COLS: u16 = 1;
const MIN_CLIENT_ROWS: u16 = 1;

/// Total time a client gets to deliver its complete handshake frame.
///
/// This is one deadline across every read of the hello, not a per-read idle
/// timeout: `shepr_platform::ipc::DeadlineReader` re-arms the socket receive timeout
/// with only the time left before each read, so a peer trickling bytes cannot
/// hold the handshake thread open. Set to 4 seconds (rather than 5) so the
/// connection is closed within 5 seconds even with OS timer slack, thread
/// scheduling, and cleanup overhead.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);

/// Largest hello frame accepted. Both hello forms are a few hundred bytes; a
/// small cap keeps an unauthenticated peer from making the handshake thread
/// allocate a full `MAX_FRAME_SIZE` buffer.
const MAX_HANDSHAKE_FRAME: usize = 64 * 1024;

// Geometry limits for every client size the server accepts: the shell hello
// and `ClientShellResize`, and the direct-attach `TerminalHello` and `Resize`.
// The cell limit is what one frame can carry (see `MAX_SURFACE_CELLS`), not
// an arbitrary safety number: a grid past it renders frames that can never
// be sent.
const MAX_CLIENT_SHELL_DIMENSION: u16 = shepr_protocol::MAX_SURFACE_DIMENSION;
const MAX_CLIENT_SHELL_CELLS: usize = shepr_protocol::MAX_SURFACE_CELLS;
const MAX_CLIENT_CELL_SIZE_PX: u32 = shepr_protocol::MAX_CELL_SIZE_PX;

fn client_shell_geometry_error(
    surface_size: shepr_protocol::ClientSurfaceSize,
    cell_width_px: u32,
    cell_height_px: u32,
) -> Option<&'static str> {
    if surface_size.cols == 0 || surface_size.rows == 0 {
        return Some("client shell requires a non-empty pane surface");
    }
    if surface_size.cols > MAX_CLIENT_SHELL_DIMENSION
        || surface_size.rows > MAX_CLIENT_SHELL_DIMENSION
        || usize::from(surface_size.cols) * usize::from(surface_size.rows) > MAX_CLIENT_SHELL_CELLS
    {
        return Some("client shell pane surface is larger than one frame can carry");
    }
    if cell_width_px > MAX_CLIENT_CELL_SIZE_PX || cell_height_px > MAX_CLIENT_CELL_SIZE_PX {
        return Some("client shell cell pixel size exceeds the safe geometry limit");
    }
    None
}

/// Direct-attach geometry as the server will use it.
type TerminalGeometry = shepr_core::geometry::HostGeometry;

/// Bounds a direct-attach client's reported geometry.
///
/// The terminal is the client's real window, so an oversized one is clamped
/// rather than refused: the attach renders into the largest grid one frame
/// can carry (rows are cut first, keeping full-width lines) and the rest of
/// the window stays blank. A cell pixel size past the limit is treated as
/// unknown, which also turns off pixel mouse reporting.
fn bound_terminal_geometry(
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    pixel_mouse: bool,
) -> TerminalGeometry {
    let (cols, rows) = clamp_terminal_size(cols, rows);
    let cell =
        shepr_protocol::ProtocolCellSize::from_wire(cell_width_px, cell_height_px, pixel_mouse);
    TerminalGeometry::new(cols, rows, cell.width(), cell.height(), cell.exact)
}

#[derive(serde::Deserialize)]
struct EndpointRequestHead {
    id: shepr_protocol::RequestId,
    method: String,
}

enum DecodedEndpointRequest {
    Dispatch(Box<shepr_api::schema::Request>),
    Error {
        request_id: shepr_protocol::RequestId,
        code: &'static str,
        message: String,
    },
}

fn write_endpoint_rejection(stream: &mut LocalStream, reason: shepr_protocol::HandshakeRefusal) {
    let welcome = EndpointServerWelcome::incompatible(reason);
    let response = ServerMessage::EndpointWelcome(welcome);
    let _ = shepr_protocol::write_message(stream, &response);
}

fn decode_endpoint_request(request: &str) -> serde_json::Result<DecodedEndpointRequest> {
    let head = serde_json::from_str::<EndpointRequestHead>(request)?;
    if !crate::server::client_commands::supports_client_shell_method_name(&head.method) {
        return Ok(DecodedEndpointRequest::Error {
            request_id: head.id,
            code: "unsupported_method",
            message: format!("method {:?} is not available on this machine", head.method),
        });
    }
    Ok(
        match serde_json::from_str::<shepr_api::schema::Request>(request) {
            Ok(request) => DecodedEndpointRequest::Dispatch(Box::new(request)),
            Err(error) => DecodedEndpointRequest::Error {
                request_id: head.id,
                code: "invalid_request",
                message: format!("invalid endpoint request: {error}"),
            },
        },
    )
}
/// Maximum structured input events accepted in one client message.
const MAX_INPUT_EVENT_BATCH: usize = 4096;

/// Channels owned by the server side of a client writer thread.
#[derive(Clone, Debug)]
pub(crate) struct ClientWriter {
    /// Reliable control messages such as shutdown, notifications, and clipboard writes.
    pub(crate) control: ClientControlWriter,
    /// Droppable render messages. Capacity is one so slow clients cannot build lag.
    pub(crate) render: ClientRenderWriter,
}

impl ClientWriter {
    /// Drops render-lane work that has not yet been claimed by the writer.
    pub(crate) fn discard_pending_render(&self) {
        self.render.queue.discard_pending_render();
    }

    #[cfg(test)]
    pub(crate) fn test_close(&self) {
        self.render.queue.close_writer();
    }

    #[cfg(test)]
    pub(crate) fn test_channel(
        control: std::sync::mpsc::Sender<Vec<u8>>,
        render: std::sync::mpsc::SyncSender<Vec<u8>>,
    ) -> Self {
        let queue = ClientWriterQueue::new();
        let drain = Arc::clone(&queue);
        let control_writer = ClientControlWriter::queue(Arc::clone(&queue));
        let mut render_writer = ClientRenderWriter::queue(queue);
        render_writer.test_render = Some(render.clone());
        let writer = Self {
            control: control_writer,
            render: render_writer,
        };
        std::thread::spawn(move || {
            while let Some(item) = drain.recv() {
                let sent = match item {
                    ClientWriteItem::Control(data) => control.send(data).is_ok(),
                    ClientWriteItem::Render(data) => render.send(data).is_ok(),
                };
                if !sent {
                    break;
                }
            }
            drain.close_writer();
        });
        writer
    }
}

#[derive(Debug)]
pub(crate) struct ClientControlWriter {
    queue: Arc<ClientWriterQueue>,
    #[cfg(test)]
    test_render: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}

#[derive(Debug)]
pub(crate) struct ClientRenderWriter {
    queue: Arc<ClientWriterQueue>,
    #[cfg(test)]
    test_render: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}

macro_rules! writer_handle {
    ($type:ty) => {
        impl Clone for $type {
            fn clone(&self) -> Self {
                self.queue.add_sender();
                Self {
                    queue: self.queue.clone(),
                    #[cfg(test)]
                    test_render: self.test_render.clone(),
                }
            }
        }
        impl Drop for $type {
            fn drop(&mut self) {
                self.queue.remove_sender();
            }
        }
    };
}
writer_handle!(ClientControlWriter);
writer_handle!(ClientRenderWriter);

impl ClientControlWriter {
    fn queue(queue: Arc<ClientWriterQueue>) -> Self {
        queue.add_sender();
        Self {
            queue,
            #[cfg(test)]
            test_render: None,
        }
    }

    pub(crate) fn send(&self, data: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        self.queue.send_control(data)
    }
}

impl ClientRenderWriter {
    fn queue(queue: Arc<ClientWriterQueue>) -> Self {
        queue.add_sender();
        Self {
            queue,
            #[cfg(test)]
            test_render: None,
        }
    }

    pub(crate) fn try_send(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        #[cfg(test)]
        if let Some(sender) = &self.test_render {
            return sender.try_send(data);
        }
        self.queue.try_send_render(data)
    }
}

#[derive(Debug)]
struct ClientWriterQueue {
    state: Mutex<ClientWriterQueueState>,
    ready: Condvar,
}

#[derive(Debug, Default)]
struct ClientWriterQueueState {
    control: VecDeque<Vec<u8>>,
    render: Option<Vec<u8>>,
    senders: usize,
    writer_alive: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ClientWriteItem {
    Control(Vec<u8>),
    Render(Vec<u8>),
}

impl ClientWriterQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ClientWriterQueueState {
                writer_alive: true,
                ..ClientWriterQueueState::default()
            }),
            ready: Condvar::new(),
        })
    }

    fn add_sender(&self) {
        let mut state = self.lock_state();
        state.senders = state.senders.saturating_add(1);
    }

    fn remove_sender(&self) {
        let mut state = self.lock_state();
        state.senders = state.senders.saturating_sub(1);
        self.ready.notify_one();
    }

    fn send_control(&self, data: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(SendError(data));
        }
        state.control.push_back(data);
        self.ready.notify_one();
        Ok(())
    }

    fn try_send_render(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(TrySendError::Disconnected(data));
        }
        if state.render.is_some() {
            return Err(TrySendError::Full(data));
        }
        state.render = Some(data);
        self.ready.notify_one();
        Ok(())
    }

    fn discard_pending_render(&self) {
        let mut state = self.lock_state();
        state.render = None;
        self.ready.notify_all();
    }

    fn recv(&self) -> Option<ClientWriteItem> {
        let mut state = self.lock_state();
        loop {
            if let Some(data) = state.control.pop_front() {
                return Some(ClientWriteItem::Control(data));
            }
            if let Some(data) = state.render.take() {
                return Some(ClientWriteItem::Render(data));
            }
            if state.senders == 0 {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn close_writer(&self) {
        let mut state = self.lock_state();
        state.writer_alive = false;
        state.render = None;
        self.ready.notify_all();
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ClientWriterQueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Internal event sent from client transport threads to the main event loop.
#[derive(Debug)]
pub(crate) enum ServerEvent {
    /// A new client completed the handshake.
    ClientConnected {
        client_id: ClientId,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
        writer: ClientWriter,
    },
    /// A client-owned shell completed its dedicated handshake.
    ClientShellConnected {
        client_id: ClientId,
        surface_cols: u16,
        surface_rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
        mouse_capture: bool,
        surface_active: bool,
        writer: ClientWriter,
    },
    /// A client sent an input message.
    ClientInput { client_id: ClientId, data: Vec<u8> },
    /// A fully decoded interactive paste exceeded the text-input limit.
    ClientPasteRejected {
        client_id: ClientId,
        size: usize,
        max: usize,
    },
    /// A client requested direct attach to one terminal.
    ClientAttachTerminal {
        client_id: ClientId,
        terminal_id: shepr_protocol::TerminalId,
        takeover: bool,
    },
    /// A direct terminal attach client requested scrollback movement.
    ClientAttachScroll {
        client_id: ClientId,
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: shepr_protocol::WireModifiers,
    },
    /// A direct terminal attach client delivered one structured mouse event.
    ClientAttachMouse {
        client_id: ClientId,
        kind: shepr_protocol::ClientMouseKind,
        position: shepr_protocol::ClientMousePosition,
        geometry: Option<shepr_protocol::ClientMouseGeometry>,
        modifiers: shepr_protocol::WireModifiers,
        lines: u16,
    },
    /// A client sent a resize message.
    ClientResize {
        client_id: ClientId,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
    },
    /// A client-owned shell recomputed its pane viewport.
    ClientShellResize {
        client_id: ClientId,
        surface_cols: u16,
        surface_rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
    },
    /// A client-owned shell delivered semantic input to one stable pane target.
    ClientShellPaneInput {
        client_id: ClientId,
        pane_id: shepr_protocol::PublicPaneId,
        events: Vec<ClientPaneInputEvent>,
    },
    /// A client-owned shell published one host terminal theme observation.
    ClientShellHostTheme {
        client_id: ClientId,
        update: shepr_protocol::ClientHostThemeUpdate,
    },
    /// A client-owned shell reported whether its outer terminal has focus.
    ClientShellFocus { client_id: ClientId, focused: bool },
    /// The committed shell asks the server to replay presentation effects before input resumes.
    ClientShellPresentationSync { client_id: ClientId, token: String },
    /// A client-owned shell invoked one endpoint operation through this connection.
    ClientShellEndpointRequest {
        client_id: ClientId,
        boot_id: shepr_protocol::BootId,
        request: Box<shepr_api::schema::Request>,
    },
    /// A well-framed endpoint request could not be dispatched by this server.
    ClientShellEndpointRequestError {
        client_id: ClientId,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        code: &'static str,
        message: String,
    },
    /// One chunk of a deferred endpoint operation's final response is ready.
    ClientShellEndpointResponseChunkReady {
        client_id: ClientId,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        final_chunk: bool,
        data: Vec<u8>,
    },
    /// A client detached gracefully.
    ClientDetach { client_id: ClientId },
    /// A client connection was lost.
    ClientDisconnected { client_id: ClientId },
    /// A client writer drained its render slot and can accept another render.
    ClientWriterDrained { client_id: ClientId },
    /// Ctrl+C or external shutdown signal received.
    QuitSignal,
}

/// Clamp client-reported terminal dimensions into the accepted range: at
/// least the minimum viable size, at most `MAX_CLIENT_SHELL_DIMENSION` per
/// side and `MAX_CLIENT_SHELL_CELLS` in total (rows give way first).
pub(crate) fn clamp_terminal_size(cols: u16, rows: u16) -> (u16, u16) {
    let cols = cols.clamp(MIN_CLIENT_COLS, MAX_CLIENT_SHELL_DIMENSION);
    let rows = rows.clamp(MIN_CLIENT_ROWS, MAX_CLIENT_SHELL_DIMENSION);
    let max_rows = MAX_CLIENT_SHELL_CELLS / usize::from(cols);
    let rows = u16::try_from(max_rows).map_or(rows, |max_rows| rows.min(max_rows.max(1)));
    (cols, rows)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEventLimit {
    WithinLimits,
    TooManyEvents,
    PasteTooLarge { size: usize },
    InputPayloadTooLarge { size: usize },
}

fn pane_input_event_limit(events: &[ClientPaneInputEvent]) -> InputEventLimit {
    let mut expanded_events = 0usize;
    let mut paste_bytes = 0usize;
    let mut input_bytes = 0usize;
    for event in events {
        expanded_events = expanded_events.saturating_add(match event {
            ClientPaneInputEvent::Key { repeat_count, .. } => usize::from((*repeat_count).max(1)),
            ClientPaneInputEvent::Mouse {
                kind:
                    shepr_protocol::ClientMouseKind::ScrollUp
                    | shepr_protocol::ClientMouseKind::ScrollDown,
                lines,
                ..
            } => usize::from((*lines).max(1)),
            ClientPaneInputEvent::TextCommit(_)
            | ClientPaneInputEvent::Mouse { .. }
            | ClientPaneInputEvent::Paste(_) => 1,
        });
        // Clients pre-check pastes with the same `text_bytes` accounting.
        if matches!(event, ClientPaneInputEvent::Paste(_)) {
            paste_bytes = paste_bytes.saturating_add(event.text_bytes());
        } else {
            input_bytes = input_bytes.saturating_add(event.text_bytes());
        }
    }

    classify_input_event_size(expanded_events, paste_bytes, input_bytes)
}

fn classify_input_event_size(
    expanded_events: usize,
    paste_bytes: usize,
    input_bytes: usize,
) -> InputEventLimit {
    if expanded_events > MAX_INPUT_EVENT_BATCH {
        return InputEventLimit::TooManyEvents;
    }

    let payload_bytes = paste_bytes.saturating_add(input_bytes);
    if payload_bytes <= MAX_INPUT_PAYLOAD {
        InputEventLimit::WithinLimits
    } else if input_bytes == 0 {
        InputEventLimit::PasteTooLarge {
            size: payload_bytes,
        }
    } else {
        InputEventLimit::InputPayloadTooLarge {
            size: payload_bytes,
        }
    }
}

fn set_client_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    _context: &'static str,
    _client_id: ClientId,
) -> io::Result<()> {
    stream.set_recv_timeout(timeout)
}

/// Handles the client handshake on a blocking thread.
///
/// Reads the `TerminalHello` or endpoint hello, validates its terminal geometry,
/// sends the welcome, and then forwards client messages to the server event channel.
pub(crate) fn handle_client_handshake(
    mut stream: LocalStream,
    client_id: ClientId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
) -> io::Result<()> {
    if should_quit.load(Ordering::Acquire) {
        return Ok(());
    }

    // Reset to blocking mode - the accept loop sets nonblocking but
    // the handshake thread needs blocking I/O for read_message/write_message.
    stream.set_nonblocking(false)?;

    // The build-identity preamble goes out first, before anything is read, so
    // a client of any other build learns which build it reached even though
    // this side hangs up on it below. Probes that connect and close at once
    // (socket liveness checks) make this write fail; that is not an error.
    if let Err(error) = shepr_protocol::preamble::write_preamble(&mut stream) {
        debug!(?client_id, %error, "client left before the build-identity preamble");
        return Ok(());
    }

    // The client's preamble and hello are read against one overall deadline.
    let mut reader = shepr_platform::ipc::DeadlineReader::new(
        &mut stream,
        std::time::Instant::now() + HANDSHAKE_TIMEOUT,
    );
    match shepr_protocol::preamble::read_preamble(&mut reader) {
        Ok(()) => {}
        Err(shepr_protocol::preamble::PreambleError::UnexpectedEof) => {
            debug!(?client_id, "client disconnected before handshake");
            return Ok(());
        }
        Err(shepr_protocol::preamble::PreambleError::Io(error)) => {
            debug!(?client_id, %error, "failed to read client preamble");
            return Ok(());
        }
        Err(error) => {
            // The client reports the mismatch from this server's preamble;
            // nothing it sends after a foreign preamble can be decoded.
            warn!(?client_id, %error, "rejecting client connection");
            return Ok(());
        }
    }
    let hello = shepr_protocol::read_message::<_, ClientMessage>(&mut reader, MAX_HANDSHAKE_FRAME);
    let hello: ClientMessage = match hello {
        Ok(msg) => msg,
        Err(shepr_protocol::FramingError::UnexpectedEof) => {
            debug!(?client_id, "client disconnected before handshake");
            return Ok(());
        }
        Err(shepr_protocol::FramingError::Oversized { claimed, max }) => {
            warn!(?client_id, claimed, max, "oversized handshake from client");
            return Ok(());
        }
        Err(err) => {
            debug!(?client_id, err = %err, "failed to read client hello");
            return Ok(());
        }
    };

    let (
        client_cols,
        client_rows,
        cell_width_px,
        cell_height_px,
        terminal_pixel_mouse,
        shell_options,
    ) = match hello {
        ClientMessage::TerminalHello { geometry } => {
            let geometry = bound_terminal_geometry(
                geometry.cols(),
                geometry.rows(),
                geometry.width(),
                geometry.height(),
                geometry.pixel_mouse,
            );
            (
                geometry.cols(),
                geometry.rows(),
                geometry.cell_width(),
                geometry.cell_height(),
                geometry.exact,
                None,
            )
        }
        ClientMessage::EndpointHello(hello) => {
            let cell = shepr_protocol::ProtocolCellSize::from_wire(
                hello.geometry.width(),
                hello.geometry.height(),
                hello.geometry.pixel_mouse,
            );
            let incompatibility = client_shell_geometry_error(
                hello.geometry.surface_size(),
                hello.geometry.width(),
                hello.geometry.height(),
            )
            .map(|reason| shepr_protocol::HandshakeRefusal::InvalidSurface(reason.to_owned()));
            if let Some(reason) = incompatibility {
                write_endpoint_rejection(&mut stream, reason);
                return Ok(());
            }
            (
                hello.geometry.cols(),
                hello.geometry.rows(),
                cell.width(),
                cell.height(),
                false,
                Some((cell.exact, hello.mouse_capture, hello.surface_active)),
            )
        }
        _ => {
            debug!(?client_id, "first message was not a handshake, closing");
            let welcome = ServerMessage::Welcome {
                error: Some(shepr_protocol::HandshakeRefusal::ExpectedHello),
            };
            let _ = shepr_protocol::write_message(&mut stream, &welcome);
            return Ok(());
        }
    };

    if should_quit.load(Ordering::Acquire) {
        return Ok(());
    }

    let welcome = if shell_options.is_some() {
        ServerMessage::EndpointWelcome(EndpointServerWelcome::compatible())
    } else {
        ServerMessage::Welcome { error: None }
    };
    shepr_protocol::write_message(&mut stream, &welcome)
        .map_err(|e| io::Error::other(e.to_string()))?;

    set_client_recv_timeout(
        &stream,
        None,
        "failed to clear client handshake read timeout",
        client_id,
    )?;

    // Create separate channels for reliable control messages and droppable renders.
    let writer_queue = ClientWriterQueue::new();
    let writer = ClientWriter {
        control: ClientControlWriter::queue(Arc::clone(&writer_queue)),
        render: ClientRenderWriter::queue(Arc::clone(&writer_queue)),
    };

    // Spawn a writer thread that forwards messages from the channels to the stream.
    let write_stream = stream.try_clone()?;
    let writer_event_tx = server_event_tx.clone();
    std::thread::spawn(move || {
        client_writer_loop(write_stream, client_id, &writer_queue, &writer_event_tx);
    });

    if should_quit.load(Ordering::Acquire) {
        send_shutdown_to_unregistered_client(&writer);
        return Ok(());
    }

    // Notify the main loop about the new client.
    let endpoint_control_writer = shell_options.as_ref().map(|_| writer.control.clone());
    let connected = if let Some((pixel_mouse, mouse_capture, surface_active)) = shell_options {
        // The exact-build preamble guarantees support for semantic surfaces.
        ServerEvent::ClientShellConnected {
            client_id,
            surface_cols: client_cols,
            surface_rows: client_rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse,
            mouse_capture,
            surface_active,
            writer,
        }
    } else {
        ServerEvent::ClientConnected {
            client_id,
            cols: client_cols,
            rows: client_rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse: terminal_pixel_mouse,
            writer,
        }
    };
    if let Err(err) = server_event_tx.blocking_send(connected) {
        match err.0 {
            ServerEvent::ClientConnected { writer, .. }
            | ServerEvent::ClientShellConnected { writer, .. } => {
                send_shutdown_to_unregistered_client(&writer);
            }
            _ => {}
        }
    }

    // Enter read loop - read client messages and forward to main loop.
    client_read_loop_with_endpoint_controls(
        stream,
        client_id,
        server_event_tx,
        should_quit,
        endpoint_control_writer.as_ref(),
    )
}

fn send_shutdown_to_unregistered_client(writer: &ClientWriter) {
    if let Ok(framed) = shepr_protocol::encode_frame(&ServerMessage::ServerShutdown {
        reason: Some(shepr_protocol::ShutdownReason::Message(
            "server is shutting down".to_owned(),
        )),
    }) {
        let _ = writer.control.send(framed);
    }
}

/// The client writer loop - prioritizes control messages over render frames.
fn client_writer_loop(
    mut stream: LocalStream,
    client_id: ClientId,
    writer_queue: &Arc<ClientWriterQueue>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) {
    while let Some(item) = writer_queue.recv() {
        let written = match item {
            ClientWriteItem::Control(data) => write_framed_bytes(&mut stream, &data),
            ClientWriteItem::Render(data) => {
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientWriterDrained { client_id });
                write_framed_bytes(&mut stream, &data)
            }
        };
        if !written {
            let _ = server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
            break;
        }
    }
    writer_queue.close_writer();
    debug!("client writer thread exiting");
}

fn write_framed_bytes(stream: &mut LocalStream, data: &[u8]) -> bool {
    let result = shepr_platform::write_client_stream(stream, data);
    if let Err(err) = result {
        debug!(err = %err, "client write failed, closing writer");
        return false;
    }
    if let Err(err) = stream.flush() {
        debug!(err = %err, "client flush failed, closing writer");
        return false;
    }
    true
}

/// The client read loop - reads messages from the client and forwards to the server event channel.
#[cfg(test)]
fn client_read_loop(
    stream: LocalStream,
    client_id: ClientId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
) -> io::Result<()> {
    client_read_loop_with_endpoint_controls(stream, client_id, server_event_tx, should_quit, None)
}

fn client_read_loop_with_endpoint_controls(
    mut stream: LocalStream,
    client_id: ClientId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
    endpoint_control_writer: Option<&ClientControlWriter>,
) -> io::Result<()> {
    while !should_quit.load(Ordering::Acquire) {
        let message = shepr_protocol::read_message(
            &mut shepr_platform::ClientStreamReader(&mut stream),
            MAX_FRAME_SIZE,
        );
        let msg: ClientMessage = match message {
            Ok(msg) => msg,
            Err(shepr_protocol::FramingError::UnexpectedEof) => {
                // Client disconnected.
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
            Err(shepr_protocol::FramingError::Oversized { claimed, max }) => {
                warn!(
                    ?client_id,
                    claimed, max, "oversized message from client, closing"
                );
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
            Err(err) => {
                debug!(?client_id, err = %err, "client read error, closing");
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
        };

        let event = match msg {
            ClientMessage::Input { data } => {
                // Validate input size.
                if data.len() > MAX_INPUT_PAYLOAD {
                    if shepr_termio::input::raw_input::is_complete_text_bracketed_paste(&data) {
                        warn!(
                            ?client_id,
                            size = data.len(),
                            max = MAX_INPUT_PAYLOAD,
                            "oversized bracketed paste from client, rejecting"
                        );
                        ServerEvent::ClientPasteRejected {
                            client_id,
                            size: data.len(),
                            max: MAX_INPUT_PAYLOAD,
                        }
                    } else {
                        warn!(
                            ?client_id,
                            size = data.len(),
                            "oversized input from client, closing"
                        );
                        let _ = server_event_tx
                            .blocking_send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                } else {
                    ServerEvent::ClientInput { client_id, data }
                }
            }
            ClientMessage::Resize { geometry } => {
                let geometry = bound_terminal_geometry(
                    geometry.cols(),
                    geometry.rows(),
                    geometry.width(),
                    geometry.height(),
                    geometry.pixel_mouse,
                );
                ServerEvent::ClientResize {
                    client_id,
                    cols: geometry.cols(),
                    rows: geometry.rows(),
                    cell_width_px: geometry.cell_width(),
                    cell_height_px: geometry.cell_height(),
                    pixel_mouse: geometry.exact,
                }
            }
            ClientMessage::ClientShellResize { geometry } => {
                let surface_size = geometry.surface_size();
                let cell = shepr_protocol::ProtocolCellSize::from_wire(
                    geometry.width(),
                    geometry.height(),
                    geometry.pixel_mouse,
                );
                let (cell_width_px, cell_height_px, pixel_mouse) =
                    (cell.width(), cell.height(), cell.exact);
                if let Some(reason) =
                    client_shell_geometry_error(surface_size, geometry.width(), geometry.height())
                {
                    warn!(?client_id, %reason, "invalid client shell resize, closing");
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                ServerEvent::ClientShellResize {
                    client_id,
                    surface_cols: surface_size.cols,
                    surface_rows: surface_size.rows,
                    cell_width_px,
                    cell_height_px,
                    pixel_mouse,
                }
            }
            ClientMessage::ClientShellHostTheme { update } => {
                if matches!(
                    &update,
                    shepr_protocol::ClientHostThemeUpdate::PaletteColors(colors)
                        if colors.len() > 256
                ) {
                    warn!(
                        ?client_id,
                        "invalid client shell host theme update, closing"
                    );
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                ServerEvent::ClientShellHostTheme { client_id, update }
            }
            ClientMessage::ClientShellFocus { focused } => {
                ServerEvent::ClientShellFocus { client_id, focused }
            }
            ClientMessage::ClientShellPaneInput { pane_id, events } => {
                match pane_input_event_limit(&events) {
                    InputEventLimit::WithinLimits => ServerEvent::ClientShellPaneInput {
                        client_id,
                        pane_id,
                        events,
                    },
                    InputEventLimit::TooManyEvents => {
                        warn!(
                            ?client_id,
                            count = events.len(),
                            "oversized targeted pane input batch, closing"
                        );
                        let _ = server_event_tx
                            .blocking_send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                    InputEventLimit::PasteTooLarge { size } => {
                        warn!(
                            ?client_id,
                            size,
                            max = MAX_INPUT_PAYLOAD,
                            "oversized targeted pane paste, rejecting"
                        );
                        ServerEvent::ClientPasteRejected {
                            client_id,
                            size,
                            max: MAX_INPUT_PAYLOAD,
                        }
                    }
                    InputEventLimit::InputPayloadTooLarge { size } => {
                        warn!(
                            ?client_id,
                            size,
                            max = MAX_INPUT_PAYLOAD,
                            "oversized targeted pane input, closing"
                        );
                        let _ = server_event_tx
                            .blocking_send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                }
            }
            ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                if boot_id.len() > crate::server::client_commands::MAX_ENDPOINT_BOOT_ID_BYTES
                    || request.len() > crate::server::client_commands::MAX_ENDPOINT_COMMAND_BYTES
                {
                    warn!(
                        ?client_id,
                        boot_id_size = boot_id.len(),
                        request_size = request.len(),
                        "oversized client shell endpoint command, closing"
                    );
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                let decoded = match decode_endpoint_request(&request) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        warn!(?client_id, %error, "invalid endpoint request envelope, closing");
                        let _ = server_event_tx
                            .blocking_send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                };
                let request_id = match &decoded {
                    DecodedEndpointRequest::Dispatch(request) => request.id.as_str(),
                    DecodedEndpointRequest::Error { request_id, .. } => request_id,
                };
                if request_id.len() > crate::server::client_commands::MAX_ENDPOINT_REQUEST_ID_BYTES
                {
                    warn!(
                        ?client_id,
                        "oversized client shell endpoint request id, closing"
                    );
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                match decoded {
                    DecodedEndpointRequest::Dispatch(request) => {
                        ServerEvent::ClientShellEndpointRequest {
                            client_id,
                            boot_id,
                            request,
                        }
                    }
                    DecodedEndpointRequest::Error {
                        request_id,
                        code,
                        message,
                    } => ServerEvent::ClientShellEndpointRequestError {
                        client_id,
                        boot_id,
                        request_id,
                        code,
                        message,
                    },
                }
            }
            ClientMessage::PresentationSync(data) => ServerEvent::ClientShellPresentationSync {
                client_id,
                token: data,
            },
            ClientMessage::HealthPing(data) => {
                let response = ServerMessage::HealthPong(data);
                let Some(writer) = endpoint_control_writer else {
                    continue;
                };
                let Ok(framed) = shepr_protocol::encode_frame(&response) else {
                    break;
                };
                if writer.send(framed).is_err() {
                    break;
                }
                continue;
            }
            ClientMessage::Detach => {
                let _ = server_event_tx.blocking_send(ServerEvent::ClientDetach { client_id });
                break;
            }
            ClientMessage::AttachTerminal {
                terminal_id,
                takeover,
            } => ServerEvent::ClientAttachTerminal {
                client_id,
                terminal_id,
                takeover,
            },
            ClientMessage::AttachScroll {
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            } => ServerEvent::ClientAttachScroll {
                client_id,
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            },
            ClientMessage::AttachMouse {
                kind,
                position,
                geometry,
                modifiers,
                lines,
            } => ServerEvent::ClientAttachMouse {
                client_id,
                kind,
                position,
                geometry,
                modifiers,
                lines,
            },
            ClientMessage::TerminalHello { .. } | ClientMessage::EndpointHello(_) => {
                // Duplicate handshake - ignore.
                continue;
            }
        };

        if server_event_tx.blocking_send(event).is_err() {
            break; // Main loop gone.
        }
    }

    debug!(?client_id, "client read thread exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::path::PathBuf;

    struct TestSocketPath(PathBuf);

    impl Drop for TestSocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A socket path in a scratch directory kept until the test process
    /// exits; `TestSocketPath` removes the socket itself. The callers' names
    /// are long, so they stay out of the path to keep it within `sun_path`.
    fn unique_test_path(_name: &str) -> std::path::PathBuf {
        crate::test_support::ScratchDir::new("ct")
            .keep_until_exit()
            .join("s.sock")
    }

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, TestSocketPath) {
        let path = unique_test_path(name);
        let _ = std::fs::remove_file(&path);
        let listener = shepr_platform::ipc::bind_local_listener(&path).expect("test precondition");
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        (client, server, TestSocketPath(path))
    }

    fn endpoint_hello(surface_cols: u16, surface_rows: u16) -> ClientMessage {
        let hello = shepr_protocol::endpoint::EndpointClientHello {
            geometry: shepr_protocol::TerminalGeometry::new(
                surface_cols,
                surface_rows,
                8,
                16,
                true,
            ),
            mouse_capture: true,
            surface_active: true,
        };
        ClientMessage::EndpointHello(hello)
    }

    /// Plays the client side of the opening: sends this build's preamble and
    /// `hello`, then consumes the server's preamble.
    fn open_as_client(client_stream: &mut LocalStream, hello: &ClientMessage) {
        shepr_protocol::preamble::write_preamble(client_stream).expect("write client preamble");
        shepr_protocol::write_message(client_stream, hello).expect("write hello");
        shepr_protocol::preamble::read_preamble(client_stream).expect("server preamble");
    }

    fn endpoint_welcome(message: ServerMessage) -> EndpointServerWelcome {
        let ServerMessage::EndpointWelcome(welcome) = message else {
            panic!("expected endpoint welcome");
        };
        welcome
    }

    fn recv_server_event(receiver: &mut mpsc::Receiver<ServerEvent>, context: &str) -> ServerEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match receiver.try_recv() {
                Ok(event) => return event,
                Err(mpsc::error::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => panic!("{context}: {err}"),
            }
        }
    }

    fn bracketed_paste_with_total_len(total_len: usize) -> Vec<u8> {
        const DELIMITER_BYTES: usize = b"\x1b[200~".len() + b"\x1b[201~".len();
        assert!(total_len >= DELIMITER_BYTES);
        let mut data = Vec::with_capacity(total_len);
        data.extend_from_slice(b"\x1b[200~");
        data.resize(total_len - b"\x1b[201~".len(), b'x');
        data.extend_from_slice(b"\x1b[201~");
        data
    }

    fn test_queue_writer() -> (ClientWriter, Arc<ClientWriterQueue>) {
        let queue = ClientWriterQueue::new();
        (
            ClientWriter {
                control: ClientControlWriter::queue(Arc::clone(&queue)),
                render: ClientRenderWriter::queue(Arc::clone(&queue)),
            },
            queue,
        )
    }

    fn frame_server_message(message: &ServerMessage) -> Vec<u8> {
        shepr_protocol::encode_frame(message).expect("frame server message")
    }

    #[test]
    fn client_writer_queue_keeps_render_slot_bounded() {
        let (writer, _queue) = test_queue_writer();
        let first = frame_server_message(&ServerMessage::WindowTitle {
            title: Some("first".into()),
        });
        let second = frame_server_message(&ServerMessage::WindowTitle {
            title: Some("second".into()),
        });

        writer.render.try_send(first).expect("first render fits");
        assert!(matches!(
            writer.render.try_send(second),
            Err(TrySendError::Full(_))
        ));
    }

    #[test]
    fn client_writer_prioritizes_control_and_reports_render_drain() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-writer-priority");
        let (writer, queue) = test_queue_writer();
        writer
            .render
            .try_send(frame_server_message(&ServerMessage::WindowTitle {
                title: Some("render".into()),
            }))
            .expect("queue render");
        writer
            .control
            .send(frame_server_message(&ServerMessage::WindowTitle {
                title: Some("control".into()),
            }))
            .expect("queue control");

        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let handle = std::thread::spawn(move || {
            client_writer_loop(
                server_stream,
                ClientId::test_new(9),
                &queue,
                &server_event_tx,
            );
        });

        match shepr_protocol::read_message(&mut client_stream, MAX_FRAME_SIZE)
            .expect("read control")
        {
            ServerMessage::WindowTitle { title } => assert_eq!(title.as_deref(), Some("control")),
            other => panic!("expected control message first, got {other:?}"),
        }
        match shepr_protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read render")
        {
            ServerMessage::WindowTitle { title } => assert_eq!(title.as_deref(), Some("render")),
            other => panic!("expected render message second, got {other:?}"),
        }
        match server_event_rx
            .blocking_recv()
            .expect("writer drained render slot")
        {
            ServerEvent::ClientWriterDrained { client_id } => assert_eq!(client_id, 9),
            other => panic!("expected writer drained event, got {other:?}"),
        }

        drop(writer);
        handle.join().expect("writer exits after senders drop");
    }

    #[test]
    fn client_writer_exits_when_all_writer_handles_drop() {
        let (_client_stream, server_stream, _path) = local_stream_pair("client-writer-drop");
        let (writer, queue) = test_queue_writer();
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(
                server_stream,
                ClientId::test_new(11),
                &queue,
                &server_event_tx,
            );
            let _ = done_tx.send(());
        });

        drop(writer);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("writer exits without polling after senders drop");
    }

    #[test]
    fn client_writer_clone_keeps_loop_alive_until_final_drop() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-writer-clone-drop");
        let (writer, queue) = test_queue_writer();
        let cloned_writer = writer.clone();
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(
                server_stream,
                ClientId::test_new(12),
                &queue,
                &server_event_tx,
            );
            let _ = done_tx.send(());
        });

        drop(writer);
        cloned_writer
            .control
            .send(frame_server_message(&ServerMessage::WindowTitle {
                title: Some("cloned".into()),
            }))
            .expect("cloned writer still sends after original drops");
        match shepr_protocol::read_message(&mut client_stream, MAX_FRAME_SIZE)
            .expect("read control from cloned writer")
        {
            ServerMessage::WindowTitle { title } => assert_eq!(title.as_deref(), Some("cloned")),
            other => panic!("expected cloned control message, got {other:?}"),
        }
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "writer exited while cloned handles were still alive"
        );

        drop(cloned_writer);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("writer exits after final cloned writer drops");
    }

    #[test]
    fn client_writer_closes_queue_after_socket_write_failure() {
        let (client_stream, server_stream, _path) =
            local_stream_pair("client-writer-socket-failure");
        server_stream
            .set_send_timeout(Some(Duration::from_millis(100)))
            .expect("set test send timeout");
        let (writer, queue) = test_queue_writer();
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(
                server_stream,
                ClientId::test_new(13),
                &queue,
                &server_event_tx,
            );
            let _ = done_tx.send(());
        });

        drop(client_stream);
        writer
            .control
            .send(vec![b'x'; 1024 * 1024])
            .expect("message is accepted before the writer observes socket failure");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer exits after socket write failure");

        assert!(matches!(writer.control.send(vec![b'y']), Err(SendError(_))));
        assert!(matches!(
            writer.render.try_send(vec![b'z']),
            Err(TrySendError::Disconnected(_))
        ));
    }

    #[test]
    fn observer_write_timeout_resets_when_sending_makes_progress() {
        use std::io::Read as _;

        let (mut client, mut server, _path) = local_stream_pair("slow-observer");
        server
            .set_send_timeout(Some(Duration::from_millis(100)))
            .expect("test precondition");
        server.set_nonblocking(true).expect("test precondition");
        let worker = std::thread::spawn(move || {
            assert!(write_framed_bytes(&mut server, &vec![b'x'; 1024 * 1024]));
        });
        client
            .set_recv_timeout(Some(Duration::from_secs(3)))
            .expect("test precondition");
        let mut received = 0;
        let mut buffer = [0; 16 * 1024];
        while received < 1024 * 1024 {
            let count = client.read(&mut buffer).expect("test precondition");
            assert_ne!(count, 0, "observer disconnected while making progress");
            received += count;
            std::thread::sleep(Duration::from_millis(5));
        }
        worker.join().expect("test precondition");
    }

    #[test]
    fn clamp_terminal_size_zero_zero() {
        assert_eq!(
            clamp_terminal_size(0, 0),
            (MIN_CLIENT_COLS, MIN_CLIENT_ROWS)
        );
    }

    #[test]
    fn clamp_terminal_size_one_one() {
        assert_eq!(clamp_terminal_size(1, 1), (1, 1));
    }

    #[test]
    fn clamp_terminal_size_preserves_narrow_client_size() {
        assert_eq!(clamp_terminal_size(40, 12), (40, 12));
    }

    #[test]
    fn clamp_terminal_size_valid() {
        assert_eq!(clamp_terminal_size(120, 40), (120, 40));
    }

    #[test]
    fn clamp_terminal_size_exact_minimum() {
        assert_eq!(
            clamp_terminal_size(MIN_CLIENT_COLS, MIN_CLIENT_ROWS),
            (MIN_CLIENT_COLS, MIN_CLIENT_ROWS)
        );
    }

    #[test]
    fn clamp_terminal_size_bounds_the_grid_to_one_frame() {
        assert_eq!(
            clamp_terminal_size(u16::MAX, u16::MAX),
            (
                MAX_CLIENT_SHELL_DIMENSION,
                u16::try_from(MAX_CLIENT_SHELL_CELLS / usize::from(MAX_CLIENT_SHELL_DIMENSION))
                    .expect("test precondition"),
            )
        );
        for (cols, rows) in [(u16::MAX, 1), (1, u16::MAX), (1000, 1000), (512, 256)] {
            let (cols, rows) = clamp_terminal_size(cols, rows);
            assert!(cols >= MIN_CLIENT_COLS && rows >= MIN_CLIENT_ROWS);
            assert!(cols <= MAX_CLIENT_SHELL_DIMENSION && rows <= MAX_CLIENT_SHELL_DIMENSION);
            assert!(usize::from(cols) * usize::from(rows) <= MAX_CLIENT_SHELL_CELLS);
        }
        // Full width is kept; rows give way.
        assert_eq!(clamp_terminal_size(1000, 1000).0, 1000);
    }

    #[test]
    fn terminal_geometry_drops_implausible_pixel_sizes() {
        let geometry = bound_terminal_geometry(80, 24, 8, 16, true);
        assert_eq!(geometry, TerminalGeometry::new(80, 24, 8, 16, true));
        let geometry = bound_terminal_geometry(80, 24, u32::MAX, 16, true);
        assert_eq!((geometry.cell_width(), geometry.cell_height()), (0, 0));
        assert!(!geometry.exact);
    }

    #[test]
    fn oversized_terminal_hello_is_clamped_to_one_frame() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-handshake-oversized-terminal");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            handle_client_handshake(
                server_stream,
                ClientId::test_new(44),
                &server_event_tx,
                &handshake_quit,
            )
        });

        open_as_client(
            &mut client_stream,
            &ClientMessage::TerminalHello {
                geometry: shepr_protocol::TerminalGeometry::new(
                    u16::MAX,
                    u16::MAX,
                    u32::MAX,
                    u32::MAX,
                    true,
                ),
            },
        );
        let _welcome: ServerMessage =
            shepr_protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        match recv_server_event(&mut server_event_rx, "oversized terminal connect") {
            ServerEvent::ClientConnected {
                cols,
                rows,
                cell_width_px,
                pixel_mouse,
                writer,
                ..
            } => {
                assert!(usize::from(cols) * usize::from(rows) <= MAX_CLIENT_SHELL_CELLS);
                assert!(cols <= MAX_CLIENT_SHELL_DIMENSION);
                assert_eq!(cell_width_px, 0);
                assert!(!pixel_mouse);
                drop(writer);
            }
            other => panic!("expected ClientConnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn oversized_terminal_resize_is_clamped_to_one_frame() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-oversized-resize");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::Resize {
                geometry: shepr_protocol::TerminalGeometry::new(u16::MAX, u16::MAX, 8, 16, true),
            },
        )
        .expect("test precondition");
        match recv_server_event(&mut server_event_rx, "oversized resize") {
            ServerEvent::ClientResize {
                cols,
                rows,
                pixel_mouse,
                ..
            } => {
                assert!(usize::from(cols) * usize::from(rows) <= MAX_CLIENT_SHELL_CELLS);
                assert!(pixel_mouse);
            }
            other => panic!("expected ClientResize, got {other:?}"),
        }

        shepr_protocol::write_message(&mut client_stream, &ClientMessage::Detach)
            .expect("write detach");
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "detach event"),
            ServerEvent::ClientDetach { client_id } if client_id == ClientId::test_new(7)
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn foreign_build_preamble_gets_the_server_identity_and_no_session() {
        use std::io::Read as _;

        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-handshake-foreign-build");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            handle_client_handshake(
                server_stream,
                ClientId::test_new(45),
                &server_event_tx,
                &handshake_quit,
            )
        });

        // A client of another build: right magic, different identity.
        let mut preamble = shepr_protocol::preamble::local_preamble();
        let last = preamble.len() - 1;
        preamble[last] = if preamble[last] == b'0' { b'1' } else { b'0' };
        client_stream
            .write_all(&preamble)
            .expect("test precondition");
        shepr_protocol::write_message(&mut client_stream, &endpoint_hello(80, 24))
            .expect("test precondition");

        // The server still announced itself, then hung up without a welcome.
        shepr_protocol::preamble::read_preamble(&mut client_stream).expect("server preamble");
        let mut rest = Vec::new();
        client_stream
            .set_recv_timeout(Some(Duration::from_secs(2)))
            .expect("test precondition");
        let _ = client_stream.read_to_end(&mut rest);
        assert!(rest.is_empty(), "no welcome after a foreign preamble");
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
        assert!(server_event_rx.try_recv().is_err());
    }

    #[test]
    fn client_shell_geometry_rejects_unsafe_dimensions_and_cell_sizes() {
        assert!(
            client_shell_geometry_error(
                shepr_protocol::ClientSurfaceSize { cols: 80, rows: 24 },
                8,
                16,
            )
            .is_none()
        );
        assert!(
            client_shell_geometry_error(
                shepr_protocol::ClientSurfaceSize {
                    cols: MAX_CLIENT_SHELL_DIMENSION,
                    rows: MAX_CLIENT_SHELL_DIMENSION,
                },
                8,
                16,
            )
            .is_some()
        );
        assert!(
            client_shell_geometry_error(
                shepr_protocol::ClientSurfaceSize { cols: 80, rows: 24 },
                MAX_CLIENT_CELL_SIZE_PX + 1,
                16,
            )
            .is_some()
        );
    }

    #[test]
    fn unknown_endpoint_method_returns_correlated_error() {
        let decoded = decode_endpoint_request(
            r#"{"id":"req-1","method":"plugin.future","params":{"value":1}}"#,
        )
        .expect("test precondition");
        assert!(matches!(
            decoded,
            DecodedEndpointRequest::Error {
                request_id,
                code: "unsupported_method",
                ..
            } if request_id == "req-1"
        ));
    }

    #[test]
    fn malformed_known_endpoint_method_returns_correlated_error() {
        let decoded =
            decode_endpoint_request(r#"{"id":"req-2","method":"workspace.focus","params":{}}"#)
                .expect("test precondition");
        assert!(matches!(
            decoded,
            DecodedEndpointRequest::Error {
                request_id,
                code: "invalid_request",
                ..
            } if request_id == "req-2"
        ));
    }

    #[test]
    fn direct_terminal_hello_selects_terminal_ansi_stream() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-handshake-ansi");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            handle_client_handshake(
                server_stream,
                ClientId::test_new(42),
                &server_event_tx,
                &handshake_quit,
            )
        });

        open_as_client(
            &mut client_stream,
            &ClientMessage::TerminalHello {
                geometry: shepr_protocol::TerminalGeometry::new(100, 30, 8, 16, true),
            },
        );

        let welcome: ServerMessage =
            shepr_protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        match welcome {
            ServerMessage::Welcome { error } => {
                assert_eq!(error, None);
            }
            other => panic!("expected Welcome, got {other:?}"),
        }

        match server_event_rx
            .blocking_recv()
            .expect("client connected event")
        {
            ServerEvent::ClientConnected {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
                writer,
            } => {
                assert_eq!(client_id, 42);
                assert_eq!((cols, rows), (100, 30));
                assert_eq!((cell_width_px, cell_height_px), (8, 16));
                assert!(pixel_mouse);
                drop(writer);
            }
            other => panic!("expected ClientConnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn dedicated_client_shell_handshake_uses_surface_viewport() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-shell-handshake");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            handle_client_handshake(
                server_stream,
                ClientId::test_new(43),
                &server_event_tx,
                &handshake_quit,
            )
        });

        open_as_client(&mut client_stream, &endpoint_hello(80, 29));

        let welcome: ServerMessage =
            shepr_protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        let welcome = endpoint_welcome(welcome);
        assert!(welcome.error.is_none());
        match server_event_rx
            .blocking_recv()
            .expect("client shell connected event")
        {
            ServerEvent::ClientShellConnected {
                client_id,
                surface_cols,
                surface_rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
                mouse_capture,
                surface_active,
                writer,
            } => {
                assert_eq!(client_id, 43);
                assert_eq!((surface_cols, surface_rows), (80, 29));
                assert_eq!((cell_width_px, cell_height_px), (8, 16));
                assert!(pixel_mouse);
                assert!(mouse_capture);
                assert!(surface_active);
                drop(writer);
            }
            other => panic!("expected ClientShellConnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn client_shell_validation_rejects_empty_surface() {
        assert!(shepr_core::geometry::GridSize::new(0, 29).is_none());
        assert_eq!(
            client_shell_geometry_error(
                shepr_protocol::ClientSurfaceSize { cols: 0, rows: 29 },
                8,
                16
            ),
            Some("client shell requires a non-empty pane surface"),
        );
    }

    #[test]
    fn client_read_loop_stops_after_detach() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-detach");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        let mut messages = Vec::new();
        shepr_protocol::write_message(&mut messages, &ClientMessage::Detach)
            .expect("test precondition");
        shepr_protocol::write_message(
            &mut messages,
            &ClientMessage::ClientShellFocus { focused: true },
        )
        .expect("test precondition");
        client_stream
            .write_all(&messages)
            .expect("write detach and trailing message");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "detach event"),
            ServerEvent::ClientDetach { client_id } if client_id == ClientId::test_new(7)
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
        assert!(server_event_rx.try_recv().is_err());
    }

    #[test]
    fn client_read_loop_consumes_health_ping_before_detach() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-health-ping");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::HealthPing(String::new()),
        )
        .expect("test precondition");
        shepr_protocol::write_message(&mut client_stream, &ClientMessage::Detach)
            .expect("test precondition");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "detach after health ping"),
            ServerEvent::ClientDetach { client_id } if client_id == ClientId::test_new(7)
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_closes_on_unsafe_shell_resize() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-unsafe-resize");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellResize {
                geometry: shepr_protocol::TerminalGeometry::new(
                    MAX_CLIENT_SHELL_DIMENSION,
                    MAX_CLIENT_SHELL_DIMENSION,
                    8,
                    16,
                    false,
                ),
            },
        )
        .expect("test precondition");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "unsafe resize disconnect"),
            ServerEvent::ClientDisconnected { client_id } if client_id == ClientId::test_new(7)
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_rejects_oversized_bracketed_paste_without_disconnect() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-oversized");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD),
            },
        )
        .expect("write maximum-size bracketed paste");

        match recv_server_event(&mut server_event_rx, "maximum-size paste event") {
            ServerEvent::ClientInput { client_id, data } => {
                assert_eq!(client_id, 7);
                assert_eq!(data.len(), MAX_INPUT_PAYLOAD);
            }
            other => panic!("expected maximum-size ClientInput, got {other:?}"),
        }

        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD + 1),
            },
        )
        .expect("write oversized bracketed paste");

        match recv_server_event(&mut server_event_rx, "oversized paste rejection") {
            ServerEvent::ClientPasteRejected {
                client_id,
                size,
                max,
            } => {
                assert_eq!(client_id, 7);
                assert_eq!(size, MAX_INPUT_PAYLOAD + 1);
                assert_eq!(max, MAX_INPUT_PAYLOAD);
            }
            ServerEvent::ClientDisconnected { .. } => {
                panic!("oversized input must be rejected without disconnecting the client")
            }
            other => panic!("expected ClientPasteRejected, got {other:?}"),
        }

        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: b"still connected".to_vec(),
            },
        )
        .expect("write valid input after rejection");

        match recv_server_event(&mut server_event_rx, "valid input after rejection") {
            ServerEvent::ClientInput { client_id, data } => {
                assert_eq!(client_id, 7);
                assert_eq!(data, b"still connected");
            }
            other => panic!("expected ClientInput after rejection, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_disconnects_oversized_non_paste_input() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-oversized-non-paste");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: vec![b'x'; MAX_INPUT_PAYLOAD + 1],
            },
        )
        .expect("write oversized non-paste input");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "oversized non-paste disconnect"),
            ServerEvent::ClientDisconnected { client_id } if client_id == ClientId::test_new(7)
        ));

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_rejects_marker_wrapped_invalid_utf8_without_disconnect() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-invalid-utf8-paste");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });
        let mut data = bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD + 1);
        let marker_len = b"\x1b[200~".len();
        let expected_size = data.len();
        data[marker_len] = 0xff;

        shepr_protocol::write_message(&mut client_stream, &ClientMessage::Input { data })
            .expect("write marker-wrapped invalid UTF-8 input");

        // Only the bracketed-paste framing decides whether oversized input is
        // recoverable; a paste whose body is not valid UTF-8 is still a
        // paste, and must be forwarded as raw bytes elsewhere rather than
        // disconnecting the client.
        match recv_server_event(&mut server_event_rx, "invalid UTF-8 paste rejection") {
            ServerEvent::ClientPasteRejected {
                client_id,
                size,
                max,
            } => {
                assert_eq!(client_id, 7);
                assert_eq!(size, expected_size);
                assert_eq!(max, MAX_INPUT_PAYLOAD);
            }
            other => panic!("expected ClientPasteRejected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_uses_authoritative_shell_resize_surface() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-resize");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellResize {
                geometry: shepr_protocol::TerminalGeometry::new(60, 15, 8, 16, true),
            },
        )
        .expect("write shell resize");
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "shell resize"),
            ServerEvent::ClientShellResize {
                client_id,
                surface_cols: 60,
                surface_rows: 15,
                cell_width_px: 8,
                cell_height_px: 16,
                pixel_mouse: true,
            } if client_id == ClientId::test_new(7)
        ));

        shepr_protocol::write_message(&mut client_stream, &ClientMessage::Detach)
            .expect("write detach");
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "detach event"),
            ServerEvent::ClientDetach { client_id } if client_id == ClientId::test_new(7)
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_keeps_single_host_theme_updates_ordered_and_palette_bounded() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-host-theme");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        let colors = (0..=u8::MAX)
            .map(|index| {
                (
                    index,
                    shepr_protocol::ClientHostColor {
                        r: index,
                        g: 0,
                        b: 0,
                    },
                )
            })
            .collect();
        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellHostTheme {
                update: shepr_protocol::ClientHostThemeUpdate::PaletteColors(colors),
            },
        )
        .expect("write bounded palette update");
        shepr_protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellHostTheme {
                update: shepr_protocol::ClientHostThemeUpdate::Appearance(
                    shepr_protocol::ClientHostAppearance::Dark,
                ),
            },
        )
        .expect("write ordered appearance update");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "bounded palette update"),
            ServerEvent::ClientShellHostTheme {
                client_id,
                update: shepr_protocol::ClientHostThemeUpdate::PaletteColors(colors),
            } if client_id == ClientId::test_new(7) && colors.len() == 256
        ));
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "ordered appearance update"),
            ServerEvent::ClientShellHostTheme {
                client_id,
                update: shepr_protocol::ClientHostThemeUpdate::Appearance(
                    shepr_protocol::ClientHostAppearance::Dark
                ),
            } if client_id == ClientId::test_new(7)
        ));

        let colors = (0..=u8::MAX)
            .map(|index| {
                (
                    index,
                    shepr_protocol::ClientHostColor {
                        r: index,
                        g: 0,
                        b: 0,
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut payload = shepr_protocol::codec::to_vec(&ClientMessage::ClientShellHostTheme {
            update: shepr_protocol::ClientHostThemeUpdate::PaletteColors(colors.clone()),
        })
        .expect("encode the largest valid palette");
        // Raise the positional collection count and append one valid entry to
        // make a malformed frame the bounded serializer correctly refuses.
        let count_offset = payload.len() - 2 - colors.len() * 4;
        assert_eq!(&payload[count_offset..count_offset + 2], &[0x80, 0x02]);
        payload[count_offset..count_offset + 2].copy_from_slice(&[0x81, 0x02]);
        payload.extend_from_slice(&[0, 0, 0, 0]);
        use std::io::Write as _;
        let payload_len = u32::try_from(payload.len()).expect("palette frame fits u32");
        client_stream
            .write_all(&payload_len.to_le_bytes())
            .expect("write oversized palette frame length");
        client_stream
            .write_all(&payload)
            .expect("write oversized palette frame");
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "oversized palette disconnect"),
            ServerEvent::ClientDisconnected { client_id } if client_id == ClientId::test_new(7)
        ));

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn pane_input_limits_charge_scroll_repeats() {
        let oversized_scroll = ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::ScrollUp,
            position: shepr_protocol::ClientMousePosition::Cell { column: 0, row: 0 },
            geometry: None,
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: u16::try_from(MAX_INPUT_EVENT_BATCH + 1).unwrap_or(u16::MAX),
        };
        assert_eq!(
            pane_input_event_limit(&[oversized_scroll]),
            InputEventLimit::TooManyEvents
        );
    }

    #[test]
    fn handshake_timeout_is_within_five_second_deadline() {
        // The handshake timeout must be short enough that
        // the connection is guaranteed to close within 5 seconds even with
        // OS overhead (thread scheduling, timer slack, cleanup).
        assert!(
            HANDSHAKE_TIMEOUT < Duration::from_secs(5),
            "HANDSHAKE_TIMEOUT ({HANDSHAKE_TIMEOUT:?}) must be less than 5 seconds to guarantee \
             connection close within the 5-second deadline"
        );
    }
}
