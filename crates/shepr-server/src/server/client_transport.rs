//! Blocking client socket transport for the headless server.
//!
//! This module owns the thin-client handshake, read loop, and writer loop.
//! It converts socket I/O into [`ServerEvent`] values consumed by
//! `HeadlessServer`.

use crate::server::ClientId;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::Shutdown;
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
    self, ClientMessage, ClientPaneInputEvent, MAX_INPUT_EVENT_BATCH, MAX_INPUT_PAYLOAD,
    ServerMessage,
};

use crate::limits::{
    CLIENT_CONTROL_QUEUE_MAX_BYTES, CLIENT_CONTROL_QUEUE_MAX_ITEMS, CLIENT_WRITE_STALL_TIMEOUT,
    HANDSHAKE_TIMEOUT, UNREGISTERED_SHUTDOWN_FLUSH_TIMEOUT,
};

/// Why a client shell's geometry is refused, if it is. The limits are the
/// protocol's own (`MAX_SURFACE_DIMENSION`, `MAX_SURFACE_CELLS`,
/// `MAX_CELL_SIZE_PX`); a same-build client clamps to them before asking.
fn client_shell_geometry_error(
    surface_size: shepr_protocol::ClientSurfaceSize,
    cell_width_px: u32,
    cell_height_px: u32,
) -> Option<&'static str> {
    if surface_size.cols == 0 || surface_size.rows == 0 {
        return Some("client shell requires a non-empty pane surface");
    }
    if surface_size.cols > shepr_protocol::MAX_SURFACE_DIMENSION
        || surface_size.rows > shepr_protocol::MAX_SURFACE_DIMENSION
        || usize::from(surface_size.cols) * usize::from(surface_size.rows)
            > shepr_protocol::MAX_SURFACE_CELLS
    {
        return Some("client shell pane surface exceeds the surface size limit");
    }
    if cell_width_px > shepr_protocol::MAX_CELL_SIZE_PX
        || cell_height_px > shepr_protocol::MAX_CELL_SIZE_PX
    {
        return Some("client shell cell pixel size exceeds the safe geometry limit");
    }
    None
}

fn write_endpoint_rejection(
    stream: &mut LocalStream,
    client_id: ClientId,
    reason: shepr_protocol::HandshakeRefusal,
) {
    let welcome = EndpointServerWelcome::refused(reason);
    let response = ServerMessage::EndpointWelcome(welcome);
    match shepr_protocol::encode_message(&response) {
        Ok(framed) => {
            if let Err(error) = write_framed_bytes(stream, &framed, CLIENT_WRITE_STALL_TIMEOUT) {
                debug!(?client_id, %error, "client left before its handshake refusal was written");
            }
        }
        Err(error) => {
            debug!(?client_id, %error, "failed to encode client handshake refusal");
        }
    }
}

/// Forwards the last event a client transport thread produces before it
/// exits (disconnect, detach). A send fails only once the server loop has
/// dropped its receiver, which happens when the loop has exited and taken
/// every client's state with it, so there is nothing left to tell.
fn send_final_client_event(
    server_event_tx: &mpsc::Sender<ServerEvent>,
    client_id: ClientId,
    event: ServerEvent,
) {
    if server_event_tx.blocking_send(event).is_err() {
        debug!(
            ?client_id,
            "server loop gone before the client's final event"
        );
    }
}

fn send_client_disconnected(server_event_tx: &mpsc::Sender<ServerEvent>, client_id: ClientId) {
    send_final_client_event(
        server_event_tx,
        client_id,
        ServerEvent::ClientDisconnected { client_id },
    );
}

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

    /// Shuts down both socket directions and wakes the connection's transport
    /// threads, even if the reader still holds a writer handle clone.
    pub(crate) fn close(&self) {
        self.control.queue.close_connection();
    }

    /// Adds a barrier after the control messages already queued for this
    /// client. The receiver completes after the writer flushes that prefix.
    pub(crate) fn flush(&self) -> tokio::sync::oneshot::Receiver<()> {
        self.control.flush()
    }
}

#[derive(Debug)]
pub(crate) struct ClientControlWriter {
    queue: Arc<ClientWriterQueue>,
}

#[derive(Debug)]
pub(crate) struct ClientRenderWriter {
    queue: Arc<ClientWriterQueue>,
}

macro_rules! writer_handle {
    ($type:ty) => {
        impl Clone for $type {
            fn clone(&self) -> Self {
                self.queue.add_sender();
                Self {
                    queue: self.queue.clone(),
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
        Self { queue }
    }

    pub(crate) fn send(&self, data: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        self.queue.send_control(data)
    }

    fn flush(&self) -> tokio::sync::oneshot::Receiver<()> {
        self.queue.send_flush()
    }
}

impl ClientRenderWriter {
    fn queue(queue: Arc<ClientWriterQueue>) -> Self {
        queue.add_sender();
        Self { queue }
    }

    pub(crate) fn try_send(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        self.queue.try_send_render(data)
    }
}

#[derive(Debug)]
struct ClientWriterQueue {
    state: Mutex<ClientWriterQueueState>,
    ready: Condvar,
    shutdown_stream: Mutex<Option<LocalStream>>,
    max_control_items: usize,
    max_control_bytes: usize,
}

#[derive(Debug, Default)]
struct ClientWriterQueueState {
    control: VecDeque<ClientControlItem>,
    /// Queued and in-flight control items share the same bound.
    control_items: usize,
    /// Queued and in-flight control bytes share the same bound.
    control_bytes: usize,
    render: Option<Vec<u8>>,
    senders: usize,
    writer_alive: bool,
}

#[derive(Debug)]
enum ClientWriteItem {
    Control(Vec<u8>),
    Render(Vec<u8>),
    Flush(tokio::sync::oneshot::Sender<()>),
}

#[derive(Debug)]
enum ClientControlItem {
    Data(Vec<u8>),
    Flush(tokio::sync::oneshot::Sender<()>),
}

impl ClientWriterQueue {
    fn new_for_connection(shutdown_stream: LocalStream) -> Arc<Self> {
        Self::with_limits(
            Some(shutdown_stream),
            CLIENT_CONTROL_QUEUE_MAX_ITEMS,
            CLIENT_CONTROL_QUEUE_MAX_BYTES,
        )
    }

    fn with_limits(
        shutdown_stream: Option<LocalStream>,
        max_control_items: usize,
        max_control_bytes: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ClientWriterQueueState {
                writer_alive: true,
                ..ClientWriterQueueState::default()
            }),
            ready: Condvar::new(),
            shutdown_stream: Mutex::new(shutdown_stream),
            max_control_items,
            max_control_bytes,
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
        // Control items share the byte budget while queued and in flight.
        // A reply that fits an empty queue still closes a client if earlier
        // control traffic leaves too little room: `ResponseTooLarge` means a
        // wire-limit violation, while sending past the remaining budget would
        // let a slow reader exceed the backlog bound.
        if state.control_items >= self.max_control_items
            || data.len() > self.max_control_bytes.saturating_sub(state.control_bytes)
        {
            drop(state);
            self.close_connection();
            return Err(SendError(data));
        }
        state.control_items += 1;
        state.control_bytes += data.len();
        state.control.push_back(ClientControlItem::Data(data));
        self.ready.notify_one();
        Ok(())
    }

    fn send_flush(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (ack, receiver) = tokio::sync::oneshot::channel();
        let mut state = self.lock_state();
        if !state.writer_alive {
            return receiver;
        }
        if state.control_items >= self.max_control_items {
            drop(state);
            self.close_connection();
            return receiver;
        }
        state.control_items += 1;
        state.control.push_back(ClientControlItem::Flush(ack));
        self.ready.notify_one();
        receiver
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
            if let Some(item) = state.control.pop_front() {
                return Some(match item {
                    ClientControlItem::Data(data) => ClientWriteItem::Control(data),
                    ClientControlItem::Flush(ack) => ClientWriteItem::Flush(ack),
                });
            }
            if let Some(data) = state.render.take() {
                return Some(ClientWriteItem::Render(data));
            }
            if state.senders == 0 || !state.writer_alive {
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
        state.control.clear();
        state.control_items = 0;
        state.control_bytes = 0;
        self.ready.notify_all();
    }

    fn finish_control_item(&self, bytes: usize) {
        let mut state = self.lock_state();
        state.control_items = state.control_items.saturating_sub(1);
        state.control_bytes = state.control_bytes.saturating_sub(bytes);
    }

    fn close_connection(&self) {
        self.close_writer();
        let stream = self
            .shutdown_stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(stream) = stream
            && let Err(error) = shutdown_client_connection(&stream)
            && error.kind() != io::ErrorKind::NotConnected
        {
            debug!(error = %error, "failed to shut down client connection");
        }
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
    /// A fully decoded interactive paste exceeded the text-input limit.
    ClientPasteRejected {
        client_id: ClientId,
        size: usize,
        max: usize,
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
        request_id: shepr_protocol::RequestId,
        command: Box<shepr_protocol::command::EndpointCommand>,
    },
    /// A client detached gracefully.
    ClientDetach { client_id: ClientId },
    /// A client connection was lost.
    ClientDisconnected { client_id: ClientId },
    /// A client writer drained its render slot and can accept another render.
    ClientWriterDrained { client_id: ClientId },
    /// The logind monitor observed a host shutdown warning or cancellation and
    /// woke the server loop to synchronize its shutdown state.
    HostShutdownWake,
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
        expanded_events = expanded_events.saturating_add(event.expanded_event_count());
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

/// Handles the client handshake on a blocking thread.
///
/// Reads the endpoint hello, validates its surface geometry, sends the welcome
/// accepting the connection, and then forwards client messages to the server event channel. Any other
/// first message is refused.
pub(crate) fn handle_client_handshake(
    mut stream: LocalStream,
    client_id: ClientId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<shepr_api::ServerStopSignal>,
) -> io::Result<()> {
    if should_quit.is_requested() {
        return Ok(());
    }

    // The server listener uses accept-only nonblocking mode, so accepted
    // streams are already blocking. Keep the handshake reads in that mode;
    // framed writes opt into nonblocking mode when their bounded writer starts.

    // The build-identity preamble goes out first, before anything is read, so
    // a client of any other build learns which build it reached even though
    // this side hangs up on it below. Probes that connect and close at once
    // (socket liveness checks) make this write fail; that is not an error.
    if let Err(error) = shepr_protocol::preamble::write_preamble(&mut stream) {
        debug!(
            ?client_id,
            %error,
            "client left before the build-identity preamble"
        );
        return Ok(());
    }

    // The client's preamble and hello are read against one overall deadline.
    let mut reader = shepr_platform::ipc::DeadlineReader::new(
        &mut stream,
        // clock-io-ok: the deadline bounds real socket reads of the handshake.
        std::time::Instant::now() + HANDSHAKE_TIMEOUT,
    );
    match shepr_protocol::preamble::read_preamble(&mut reader) {
        Ok(()) => {}
        Err(shepr_protocol::preamble::PreambleError::UnexpectedEof) => {
            debug!(?client_id, "client disconnected before handshake");
            return Ok(());
        }
        Err(shepr_protocol::preamble::PreambleError::Io(error)) => {
            debug!(
                ?client_id,
                %error,
                "failed to read client preamble"
            );
            return Ok(());
        }
        Err(error) => {
            // The client reports the mismatch from this server's preamble;
            // nothing it sends after a foreign preamble can be decoded.
            warn!(
                ?client_id,
                %error,
                "rejecting client connection"
            );
            return Ok(());
        }
    }
    let hello = shepr_protocol::read_handshake_message::<_, ClientMessage>(&mut reader);
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
            debug!(
                ?client_id,
                error = %err,
                "failed to read client hello"
            );
            return Ok(());
        }
    };

    let ClientMessage::EndpointHello(hello) = hello else {
        debug!(?client_id, "first message was not a handshake, closing");
        write_endpoint_rejection(
            &mut stream,
            client_id,
            shepr_protocol::HandshakeRefusal::ExpectedHello,
        );
        return Ok(());
    };
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
        write_endpoint_rejection(&mut stream, client_id, reason);
        return Ok(());
    }

    if should_quit.is_requested() {
        return Ok(());
    }

    let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::accepted());
    // Keep framing separate from transport so the welcome can also be supplied
    // as pre-encoded bytes without changing the bounded socket write path.
    let welcome = shepr_protocol::encode_message(&welcome)
        .map_err(|error| io::Error::other(error.to_string()))?;
    write_framed_bytes(&mut stream, &welcome, CLIENT_WRITE_STALL_TIMEOUT)?;

    stream.set_recv_timeout(None)?;

    // Create separate channels for reliable control messages and droppable renders.
    let write_stream = stream.try_clone()?;
    let shutdown_stream = stream.try_clone()?;
    let writer_queue = ClientWriterQueue::new_for_connection(shutdown_stream);
    let writer = ClientWriter {
        control: ClientControlWriter::queue(Arc::clone(&writer_queue)),
        render: ClientRenderWriter::queue(Arc::clone(&writer_queue)),
    };

    // Spawn a writer thread that forwards messages from the channels to the stream.
    let writer_event_tx = server_event_tx.clone();
    std::thread::spawn(move || {
        client_writer_loop(write_stream, client_id, &writer_queue, &writer_event_tx);
    });

    if should_quit.is_requested() {
        send_shutdown_to_unregistered_client(&writer);
        return Ok(());
    }

    // Notify the main loop about the new client.
    let endpoint_control_writer = writer.control.clone();
    // The exact-build preamble guarantees support for semantic surfaces.
    let connected = ServerEvent::ClientShellConnected {
        client_id,
        surface_cols: hello.geometry.cols(),
        surface_rows: hello.geometry.rows(),
        cell_width_px: cell.width(),
        cell_height_px: cell.height(),
        pixel_mouse: cell.exact,
        mouse_capture: hello.mouse_capture,
        surface_active: hello.surface_active,
        writer,
    };
    if let Err(err) = server_event_tx.blocking_send(connected)
        && let ServerEvent::ClientShellConnected { writer, .. } = err.0
    {
        send_shutdown_to_unregistered_client(&writer);
    }

    // Enter read loop - read client messages and forward to main loop.
    client_read_loop_with_endpoint_controls(
        stream,
        client_id,
        server_event_tx,
        should_quit,
        Some(&endpoint_control_writer),
    )
}

fn send_shutdown_to_unregistered_client(writer: &ClientWriter) {
    if let Ok(framed) = shepr_protocol::encode_message(&ServerMessage::ServerShutdown {
        reason: Some(shepr_protocol::ShutdownReason::Message(
            "server is shutting down".to_owned(),
        )),
    }) && writer.control.send(framed).is_ok()
    {
        // Handshake handling runs on a transport thread, so waiting here
        // does not park the Tokio server loop. The wait is bounded: a writer
        // stuck on a client that stopped reading must not pin this thread
        // forever.
        let mut flushed = writer.flush();
        // clock-io-ok: the bound covers the writer thread's real socket write.
        let deadline = std::time::Instant::now() + UNREGISTERED_SHUTDOWN_FLUSH_TIMEOUT;
        while matches!(
            flushed.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ) {
            // clock-io-ok: the writer thread flushes concurrently in real time.
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(crate::limits::UNREGISTERED_SHUTDOWN_FLUSH_POLL_INTERVAL);
        }
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
        let (result, completed_control_bytes) = match item {
            ClientWriteItem::Control(data) => {
                let bytes = data.len();
                (
                    write_framed_bytes(&mut stream, &data, CLIENT_WRITE_STALL_TIMEOUT),
                    Some(bytes),
                )
            }
            ClientWriteItem::Render(data) => {
                // Report after this frame reaches the socket. Event-channel
                // pressure must not hold back the frame this connection has
                // already accepted from the server.
                let result = write_framed_bytes(&mut stream, &data, CLIENT_WRITE_STALL_TIMEOUT);
                if result.is_ok() {
                    // The event is reliable: dropping it could leave the
                    // server's deferred render unclaimed until another event.
                    server_event_tx
                        .blocking_send(ServerEvent::ClientWriterDrained { client_id })
                        .ok();
                }
                (result, None)
            }
            ClientWriteItem::Flush(ack) => {
                let result = match stream.flush() {
                    Ok(()) => {
                        // The waiter drops its receiver when it stops waiting
                        // (shutdown flush deadline passed); the flush happened
                        // either way and nobody is left to tell.
                        ack.send(()).ok();
                        Ok(())
                    }
                    Err(err) => Err(err),
                };
                (result, Some(0))
            }
        };
        if let Some(bytes) = completed_control_bytes {
            writer_queue.finish_control_item(bytes);
        }
        if let Err(err) = result {
            debug!(error = %err, "client write failed, closing writer");
            send_client_disconnected(server_event_tx, client_id);
            break;
        }
    }
    writer_queue.close_writer();
    debug!("client writer thread exiting");
}

fn shutdown_client_connection(stream: &LocalStream) -> io::Result<()> {
    let LocalStream::UdSocket(stream) = stream;
    stream.inner().shutdown(Shutdown::Both)
}

fn write_framed_bytes(
    stream: &mut LocalStream,
    data: &[u8],
    stall_timeout: Duration,
) -> io::Result<()> {
    shepr_platform::write_client_stream(stream, data, stall_timeout)?;
    stream.flush()
}

fn client_read_loop_with_endpoint_controls(
    mut stream: LocalStream,
    client_id: ClientId,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<shepr_api::ServerStopSignal>,
    endpoint_control_writer: Option<&ClientControlWriter>,
) -> io::Result<()> {
    while !should_quit.is_requested() {
        let message = shepr_protocol::read_message_single_frame_limited(
            &mut shepr_platform::ClientStreamReader(&mut stream),
            shepr_protocol::MAX_CLIENT_MESSAGE_SIZE,
        );
        let msg: ClientMessage = match message {
            Ok(msg) => msg,
            Err(shepr_protocol::FramingError::UnexpectedEof) => {
                // Client disconnected.
                send_client_disconnected(server_event_tx, client_id);
                break;
            }
            Err(shepr_protocol::FramingError::Oversized { claimed, max }) => {
                warn!(
                    ?client_id,
                    claimed, max, "oversized message from client, closing"
                );
                send_client_disconnected(server_event_tx, client_id);
                break;
            }
            Err(err) => {
                debug!(
                    ?client_id,
                    error = %err,
                    "client read error, closing"
                );
                send_client_disconnected(server_event_tx, client_id);
                break;
            }
        };

        let event = match msg {
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
                    send_client_disconnected(server_event_tx, client_id);
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
                    send_client_disconnected(server_event_tx, client_id);
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
                        send_client_disconnected(server_event_tx, client_id);
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
                        send_client_disconnected(server_event_tx, client_id);
                        break;
                    }
                }
            }
            ClientMessage::ClientShellEndpointRequest {
                boot_id,
                request_id,
                command,
            } => {
                // Both ids are echoed back and held with the reply until the
                // render after the command, so they are bounded here; the
                // command itself is bounded by `MAX_CLIENT_MESSAGE_SIZE`.
                if boot_id.len() > crate::server::client_commands::MAX_ENDPOINT_BOOT_ID_BYTES
                    || request_id.len()
                        > crate::server::client_commands::MAX_ENDPOINT_REQUEST_ID_BYTES
                {
                    warn!(
                        ?client_id,
                        boot_id_size = boot_id.len(),
                        request_id_size = request_id.len(),
                        "oversized client shell endpoint command ids, closing"
                    );
                    send_client_disconnected(server_event_tx, client_id);
                    break;
                }
                ServerEvent::ClientShellEndpointRequest {
                    client_id,
                    boot_id,
                    request_id,
                    command: Box::new(command),
                }
            }
            ClientMessage::PresentationSync(data) => ServerEvent::ClientShellPresentationSync {
                client_id,
                token: data,
            },
            ClientMessage::HealthPing => {
                // This acknowledges transport liveness for this reader and
                // writer, not responsiveness of the headless event loop. A
                // client removed from the registry has its socket shut down,
                // so its reader cannot keep that client healthy with pongs.
                let response = ServerMessage::HealthPong;
                let Some(writer) = endpoint_control_writer else {
                    continue;
                };
                let Ok(framed) = shepr_protocol::encode_message(&response) else {
                    break;
                };
                if writer.send(framed).is_err() {
                    break;
                }
                continue;
            }
            ClientMessage::Detach => {
                send_final_client_event(
                    server_event_tx,
                    client_id,
                    ServerEvent::ClientDetach { client_id },
                );
                break;
            }
            // A duplicate handshake, or a message only a direct terminal
            // client sent, means nothing to a client shell.
            _ => {
                debug!(?client_id, "ignoring a message no client shell sends");
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
pub(crate) use tests::RenderLaneReceiver;

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::path::PathBuf;

    /// How often a test reader re-checks the queue. The queue's condvar wakes one
    /// waiter, and a test can have two (the control drain and a render read), so
    /// test readers poll rather than rely on being the one woken.
    const TEST_LANE_POLL: Duration = Duration::from_millis(2);

    /// The test side of a writer's render slot, read the way the socket writer
    /// thread takes it. Mirrors the `std::sync::mpsc::Receiver` methods tests use.
    #[derive(Debug)]
    pub(crate) struct RenderLaneReceiver {
        queue: Arc<ClientWriterQueue>,
    }

    impl RenderLaneReceiver {
        pub(crate) fn try_recv(&self) -> Result<Vec<u8>, std::sync::mpsc::TryRecvError> {
            self.queue.take_render_for_test()
        }

        pub(crate) fn recv(&self) -> Result<Vec<u8>, std::sync::mpsc::RecvError> {
            loop {
                match self.queue.take_render_for_test() {
                    Ok(data) => return Ok(data),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Err(std::sync::mpsc::RecvError);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        self.queue.wait_for_test(TEST_LANE_POLL);
                    }
                }
            }
        }

        pub(crate) fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Result<Vec<u8>, std::sync::mpsc::RecvTimeoutError> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match self.queue.take_render_for_test() {
                    Ok(data) => return Ok(data),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(std::sync::mpsc::RecvTimeoutError::Timeout);
                }
                self.queue
                    .wait_for_test(TEST_LANE_POLL.min(deadline.saturating_duration_since(now)));
            }
        }
    }

    impl ClientWriter {
        pub(crate) fn test_close(&self) {
            self.render.queue.close_writer();
        }

        /// A writer over the production queue whose far side a test reads in
        /// place of the socket writer thread: control items arrive on the
        /// returned channel (flush barriers are acknowledged as they are
        /// reached), and the render slot is read through [`RenderLaneReceiver`].
        /// The server side sends through exactly the code production uses, so the
        /// one-slot render backpressure a test sees is the real one: a render the
        /// test has not read keeps the slot full.
        pub(crate) fn test_pair() -> (Self, std::sync::mpsc::Receiver<Vec<u8>>, RenderLaneReceiver)
        {
            let queue = ClientWriterQueue::with_limits(
                None,
                CLIENT_CONTROL_QUEUE_MAX_ITEMS,
                CLIENT_CONTROL_QUEUE_MAX_BYTES,
            );
            let writer = Self {
                control: ClientControlWriter::queue(Arc::clone(&queue)),
                render: ClientRenderWriter::queue(Arc::clone(&queue)),
            };
            let (control_tx, control_rx) = std::sync::mpsc::channel();
            let drain = Arc::clone(&queue);
            std::thread::spawn(move || {
                while let Some(item) = drain.recv_control_for_test() {
                    match item {
                        ClientControlItem::Data(data) => {
                            let bytes = data.len();
                            if control_tx.send(data).is_err() {
                                // The test dropped its control receiver: the
                                // client is gone, as a failed socket write says.
                                drain.close_writer();
                                return;
                            }
                            drain.finish_control_item(bytes);
                        }
                        ClientControlItem::Flush(ack) => {
                            // Same contract as the socket writer: a waiter that
                            // stopped waiting dropped its receiver, and the
                            // barrier was reached either way.
                            ack.send(()).ok();
                            drain.finish_control_item(0);
                        }
                    }
                }
            });
            (writer, control_rx, RenderLaneReceiver { queue })
        }
    }

    /// The far side of the queue as a test reads it, standing in for
    /// `client_writer_loop`. Only the consumer side is replaced; every send goes
    /// through the production methods above.
    impl ClientWriterQueue {
        /// The next control item, or `None` once the lane can produce no more.
        fn recv_control_for_test(&self) -> Option<ClientControlItem> {
            let mut state = self.lock_state();
            loop {
                if let Some(item) = state.control.pop_front() {
                    return Some(item);
                }
                if state.senders == 0 || !state.writer_alive {
                    return None;
                }
                state = self
                    .ready
                    .wait_timeout(state, TEST_LANE_POLL)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
        }

        /// Takes the render slot, as the writer thread does before writing it.
        fn take_render_for_test(&self) -> Result<Vec<u8>, std::sync::mpsc::TryRecvError> {
            let mut state = self.lock_state();
            if let Some(data) = state.render.take() {
                return Ok(data);
            }
            if state.senders == 0 || !state.writer_alive {
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            } else {
                Err(std::sync::mpsc::TryRecvError::Empty)
            }
        }

        fn wait_for_test(&self, timeout: Duration) {
            let state = self.lock_state();
            drop(
                self.ready
                    .wait_timeout(state, timeout)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
    }

    /// The client read loop - reads messages from the client and forwards to the server event channel.
    fn client_read_loop(
        stream: LocalStream,
        client_id: ClientId,
        server_event_tx: &mpsc::Sender<ServerEvent>,
        should_quit: &Arc<shepr_api::ServerStopSignal>,
    ) -> io::Result<()> {
        client_read_loop_with_endpoint_controls(
            stream,
            client_id,
            server_event_tx,
            should_quit,
            None,
        )
    }

    struct TestSocketPath(PathBuf);

    impl Drop for TestSocketPath {
        fn drop(&mut self) {
            // Tidiness only: the socket lives in a per-test ScratchDir that
            // is cleared when next handed out, and a panic here could run
            // during an unwind and abort the test binary.
            std::fs::remove_file(&self.0).ok();
        }
    }

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, TestSocketPath) {
        let path = crate::test_support::ScratchDir::new(name).join("s.sock");
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        runtime
            // The timer must be created inside the runtime, so build it in the
            // async block rather than as the argument.
            .block_on(async { tokio::time::timeout(Duration::from_secs(1), receiver.recv()).await })
            .unwrap_or_else(|_| panic!("{context}: timed out"))
            .unwrap_or_else(|| panic!("{context}: channel closed"))
    }

    fn test_queue_writer() -> (ClientWriter, Arc<ClientWriterQueue>) {
        let queue = ClientWriterQueue::with_limits(
            None,
            CLIENT_CONTROL_QUEUE_MAX_ITEMS,
            CLIENT_CONTROL_QUEUE_MAX_BYTES,
        );
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
    fn client_control_queue_bounds_outstanding_bytes_and_items() {
        let queue = ClientWriterQueue::with_limits(None, 2, 5);
        let writer = ClientWriter {
            control: ClientControlWriter::queue(Arc::clone(&queue)),
            render: ClientRenderWriter::queue(Arc::clone(&queue)),
        };
        writer
            .control
            .send(vec![b'x'; 5])
            .expect("message within the byte bound fits");
        let Some(ClientWriteItem::Control(data)) = queue.recv() else {
            panic!("expected the queued control message");
        };
        assert_eq!(data.len(), 5);
        assert!(matches!(writer.control.send(vec![b'y']), Err(SendError(_))));
        assert!(matches!(
            writer.render.try_send(vec![b'z']),
            Err(TrySendError::Disconnected(_))
        ));

        let queue = ClientWriterQueue::with_limits(None, 2, 10);
        let writer = ClientWriter {
            control: ClientControlWriter::queue(Arc::clone(&queue)),
            render: ClientRenderWriter::queue(Arc::clone(&queue)),
        };
        writer.control.send(vec![b'a']).expect("first item fits");
        writer.control.send(vec![b'b']).expect("second item fits");
        assert!(matches!(queue.recv(), Some(ClientWriteItem::Control(_))));
        assert!(matches!(writer.control.send(vec![b'c']), Err(SendError(_))));
    }

    #[test]
    fn client_control_queue_closes_when_endpoint_reply_exceeds_remaining_byte_budget() {
        let reply =
            shepr_protocol::encode_message(&crate::server::client_commands::response_message(
                shepr_test_fixtures::fixed_boot_id(1),
                "request-a".into(),
                Ok(shepr_protocol::command::EndpointReply::Done),
            ))
            .expect("endpoint response frames");
        let byte_cap = reply.len();
        let empty_queue = ClientWriterQueue::with_limits(None, 4, byte_cap);
        let empty_writer = ClientWriter {
            control: ClientControlWriter::queue(Arc::clone(&empty_queue)),
            render: ClientRenderWriter::queue(Arc::clone(&empty_queue)),
        };
        empty_writer
            .control
            .send(reply.clone())
            .expect("the endpoint reply fits an empty queue");

        let queue = ClientWriterQueue::with_limits(None, 4, byte_cap);
        let writer = ClientWriter {
            control: ClientControlWriter::queue(Arc::clone(&queue)),
            render: ClientRenderWriter::queue(Arc::clone(&queue)),
        };

        writer
            .control
            .send(vec![b'x'])
            .expect("first control item fits");
        assert!(matches!(writer.control.send(reply), Err(SendError(_))));
        assert!(matches!(
            writer.render.try_send(vec![b'z']),
            Err(TrySendError::Disconnected(_))
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

        match shepr_protocol::read_message(&mut client_stream).expect("read control") {
            ServerMessage::WindowTitle { title } => assert_eq!(title.as_deref(), Some("control")),
            other => panic!("expected control message first, got {other:?}"),
        }
        match shepr_protocol::read_message(&mut client_stream).expect("read render") {
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
    fn client_writer_delivers_render_before_waiting_for_drain_event_capacity() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-writer-event-backpressure");
        client_stream
            .set_recv_timeout(Some(Duration::from_secs(1)))
            .expect("test precondition");
        let (writer, queue) = test_queue_writer();
        writer
            .render
            .try_send(frame_server_message(&ServerMessage::WindowTitle {
                title: Some("render".into()),
            }))
            .expect("queue render");

        let (server_event_tx, mut server_event_rx) = mpsc::channel(1);
        server_event_tx
            .try_send(ServerEvent::HostShutdownWake)
            .expect("fill the server event channel");
        let handle = std::thread::spawn(move || {
            client_writer_loop(
                server_stream,
                ClientId::test_new(10),
                &queue,
                &server_event_tx,
            );
        });

        assert!(matches!(
            shepr_protocol::read_message(&mut client_stream).expect("render is written"),
            ServerMessage::WindowTitle { title: Some(title) } if title == "render"
        ));
        assert!(matches!(
            server_event_rx.blocking_recv(),
            Some(ServerEvent::HostShutdownWake)
        ));
        assert!(matches!(
            server_event_rx.blocking_recv(),
            Some(ServerEvent::ClientWriterDrained { client_id }) if client_id == 10
        ));

        drop(writer);
        handle.join().expect("writer exits after handles drop");
    }

    #[test]
    fn closing_client_writer_shuts_down_both_socket_directions() {
        use std::io::Read as _;

        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-writer-close-connection");
        client_stream
            .set_recv_timeout(Some(Duration::from_secs(1)))
            .expect("test precondition");
        let queue = ClientWriterQueue::new_for_connection(
            server_stream.try_clone().expect("clone shutdown handle"),
        );
        let writer = ClientWriter {
            control: ClientControlWriter::queue(Arc::clone(&queue)),
            render: ClientRenderWriter::queue(Arc::clone(&queue)),
        };

        writer.close();
        let mut bytes = Vec::new();
        client_stream
            .read_to_end(&mut bytes)
            .expect("peer observes shutdown");
        assert!(bytes.is_empty());
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
            done_tx
                .send(())
                .expect("test still waiting for the writer to exit");
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
            done_tx
                .send(())
                .expect("test still waiting for the writer to exit");
        });

        drop(writer);
        cloned_writer
            .control
            .send(frame_server_message(&ServerMessage::WindowTitle {
                title: Some("cloned".into()),
            }))
            .expect("cloned writer still sends after original drops");
        match shepr_protocol::read_message(&mut client_stream)
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
            done_tx
                .send(())
                .expect("test still waiting for the writer to exit");
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
        let worker = std::thread::spawn(move || {
            assert!(
                write_framed_bytes(
                    &mut server,
                    &vec![b'x'; 1024 * 1024],
                    Duration::from_millis(100),
                )
                .is_ok()
            );
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
            // Pace reads so the sender must make progress across timeout windows.
            std::thread::sleep(Duration::from_millis(5));
        }
        worker.join().expect("test precondition");
    }

    #[test]
    fn foreign_build_preamble_gets_the_server_identity_and_no_session() {
        use std::io::Read as _;

        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-handshake-foreign-build");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(shepr_api::ServerStopSignal::default());
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
        // The server hangs up without reading the hello, and Linux reports
        // closing a unix socket with unread data as a reset to the peer; a
        // timeout would mean it never hung up.
        if let Err(err) = client_stream.read_to_end(&mut rest) {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::ConnectionReset,
                "server hangs up after a foreign preamble: {err}"
            );
        }
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
                    cols: shepr_protocol::MAX_SURFACE_DIMENSION,
                    rows: shepr_protocol::MAX_SURFACE_DIMENSION,
                },
                8,
                16,
            )
            .is_some()
        );
        assert!(
            client_shell_geometry_error(
                shepr_protocol::ClientSurfaceSize { cols: 80, rows: 24 },
                shepr_protocol::MAX_CELL_SIZE_PX + 1,
                16,
            )
            .is_some()
        );
    }

    #[test]
    fn dedicated_client_shell_handshake_uses_surface_viewport() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-shell-handshake");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(shepr_api::ServerStopSignal::default());
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
            shepr_protocol::read_message(&mut client_stream).expect("read welcome");
        assert_eq!(endpoint_welcome(welcome), EndpointServerWelcome::Accepted);

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
        should_quit.request();
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
        let should_quit = Arc::new(shepr_api::ServerStopSignal::default());
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
        let should_quit = Arc::new(shepr_api::ServerStopSignal::default());
        let read_quit = Arc::clone(&should_quit);
        let handle = std::thread::spawn(move || {
            client_read_loop(
                server_stream,
                ClientId::test_new(7),
                &server_event_tx,
                &read_quit,
            )
        });

        shepr_protocol::write_message(&mut client_stream, &ClientMessage::HealthPing)
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
        let should_quit = Arc::new(shepr_api::ServerStopSignal::default());
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
                    shepr_protocol::MAX_SURFACE_DIMENSION,
                    shepr_protocol::MAX_SURFACE_DIMENSION,
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
    fn client_read_loop_uses_authoritative_shell_resize_surface() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-resize");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(shepr_api::ServerStopSignal::default());
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
        let should_quit = Arc::new(shepr_api::ServerStopSignal::default());
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
        let mut payload =
            shepr_test_fixtures::encode_to_vec(&ClientMessage::ClientShellHostTheme {
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
        should_quit.request();
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
