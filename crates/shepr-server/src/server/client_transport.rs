//! Blocking transport for TUI connections to the headless server.
//!
//! The server socket's listener (`shepr_api`) hands each TUI connection to
//! [`ClientTransportHandler`] from byte zero. This module owns the thin-client
//! handshake, read loop, and writer loop.
//! It converts socket I/O into [`ServerEvent`] values consumed by
//! `HeadlessServer`.

use crate::server::ClientId;
use crate::server::outbox::{ClientOutbox, ClientWriteItem, ControlSender, Delivery, OutboxQueue};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, warn};

use shepr_platform::ipc::LocalStream;
use shepr_protocol::endpoint::EndpointServerWelcome;
use shepr_protocol::{
    self, ClientMessage, ClientPaneInputEvent, InputBatchCharge, MAX_INPUT_PAYLOAD, ServerMessage,
};

use crate::limits::{
    CLIENT_WRITE_STALL_TIMEOUT, HANDSHAKE_TIMEOUT, UNREGISTERED_SHUTDOWN_FLUSH_TIMEOUT,
};

/// The server's client protocol, installed into the listener's gate once
/// panes are restored. Each TUI connection gets a fresh client id and runs
/// its handshake and read loop on the listener's connection thread, holding
/// its admission slot until it ends.
pub(crate) struct ClientTransportHandler {
    pub(crate) server_event_tx: mpsc::Sender<ServerEvent>,
    pub(crate) should_quit: Arc<shepr_api::ServerStopSignal>,
    pub(crate) wake: Arc<tokio::sync::Notify>,
    pub(crate) ids: crate::server::clients::ClientIdAllocator,
}

impl shepr_api::ClientProtocolHandler for ClientTransportHandler {
    fn serve(&self, stream: LocalStream, slot: shepr_api::ConnectionSlot, accepted: Instant) {
        let _slot = slot;
        let client_id = self.ids.allocate();
        if let Err(error) = handle_client_handshake(
            stream,
            client_id,
            accepted + HANDSHAKE_TIMEOUT,
            &self.server_event_tx,
            &self.should_quit,
            Arc::clone(&self.wake),
        ) {
            debug!(?client_id, %error, "client transport failed");
        }
    }
}

/// Why a client shell's geometry is refused, if it is. The grid is already
/// nonzero through `GridSize`; this checks the protocol's upper bounds. A
/// same-build client clamps to them before asking.
fn client_shell_geometry_error(
    surface_size: shepr_protocol::ClientSurfaceSize,
    cell_width_px: u32,
    cell_height_px: u32,
) -> Option<&'static str> {
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

/// Internal event sent from client transport threads to the main event loop.
#[derive(Debug)]
#[expect(
    clippy::enum_variant_names,
    reason = "every event comes from a client connection, so the prefix names the source"
)]
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
        outbox: ClientOutbox,
    },
    /// A fully decoded interactive paste exceeded the text-input limit.
    ClientPasteRejected { client_id: ClientId, size: usize },
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
    /// A shell that just committed to showing this connection asks for its current mouse
    /// capture, keyboard mode and title, which it dropped while preparing the connection.
    ClientShellReplayHostEffects { client_id: ClientId },
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEventLimit {
    WithinLimits,
    TooManyEvents,
    PasteTooLarge { size: usize },
    InputPayloadTooLarge { size: usize },
}

/// Charges a received batch with the same `InputBatchCharge` the client
/// batcher builds messages under. An oversized payload reads as a paste
/// overflow when pastes carry all of its text, so the client can show its
/// paste notice.
fn pane_input_event_limit(events: &[ClientPaneInputEvent]) -> InputEventLimit {
    let charge = InputBatchCharge::of_events(events);
    if !charge.events_fit() {
        return InputEventLimit::TooManyEvents;
    }
    if charge.bytes_fit() {
        return InputEventLimit::WithinLimits;
    }
    let size = charge.text_bytes();
    let paste_only = events
        .iter()
        .all(|event| matches!(event, ClientPaneInputEvent::Paste(_)) || event.text_bytes() == 0);
    if paste_only {
        InputEventLimit::PasteTooLarge { size }
    } else {
        InputEventLimit::InputPayloadTooLarge { size }
    }
}

/// Handles the client handshake on a blocking thread.
///
/// Reads the client's preamble first, since the client speaks first, and
/// answers a recognisable preamble of any build with this build's. A client of
/// another build is closed there, without decoding its hello. A client of this
/// build then has its endpoint hello read and its surface geometry validated,
/// and is sent the welcome accepting the connection; its messages are then
/// forwarded to the server event channel. Any other first message is refused.
/// `deadline` bounds reading the preamble and hello together and is counted
/// from accept, so classification time is part of the handshake budget.
/// `wake` is the server loop's outbox wake, raised when this connection's
/// outbox closes, its control lane makes room for held replies, or its writer
/// finishes a render frame.
fn handle_client_handshake(
    mut stream: LocalStream,
    client_id: ClientId,
    deadline: Instant,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<shepr_api::ServerStopSignal>,
    wake: Arc<tokio::sync::Notify>,
) -> io::Result<()> {
    if should_quit.is_requested() {
        return Ok(());
    }

    // Accepted streams start blocking (classification peeks without changing
    // the mode). The bounded preamble write below switches the stream to
    // nonblocking; the handshake reads still work because the deadline reader
    // polls for readiness before every read.

    // The client's preamble and hello are read against one overall deadline,
    // counted from accept. The client speaks first, so nothing is written
    // until its preamble has been read.
    let mut reader = shepr_platform::ipc::LocalStreamDeadlineReader::new(&mut stream, deadline);
    let foreign = match shepr_protocol::preamble::read_preamble(&mut reader) {
        Ok(()) => None,
        Err(shepr_protocol::preamble::PreambleError::UnexpectedEof) => {
            debug!(?client_id, "client disconnected before handshake");
            return Ok(());
        }
        Err(shepr_protocol::preamble::PreambleError::Io(error)) => {
            debug!(?client_id, %error, "failed to read client preamble");
            return Ok(());
        }
        Err(error @ shepr_protocol::preamble::PreambleError::NotShepr) => {
            warn!(?client_id, %error, "rejecting client connection");
            return Ok(());
        }
        Err(error @ shepr_protocol::preamble::PreambleError::DifferentBuild(_)) => Some(error),
    };
    // A recognisable preamble of any build is answered with this build's, so
    // a client of another build learns which build it reached.
    if let Err(error) = shepr_platform::write_client_stream(
        &stream,
        &shepr_protocol::preamble::local_preamble(),
        CLIENT_WRITE_STALL_TIMEOUT,
    ) {
        debug!(?client_id, %error, "client left before the build-identity preamble");
        return Ok(());
    }
    if let Some(error) = foreign {
        // The client reports the mismatch from this server's preamble;
        // nothing it sends after a foreign preamble can be decoded.
        warn!(?client_id, %error, "rejecting client connection");
        return Ok(());
    }
    let mut reader = shepr_platform::ipc::LocalStreamDeadlineReader::new(&mut stream, deadline);
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

    // Oversized raw dimensions were rejected above, so `from_wire`'s
    // oversize fallback cannot be reached on this transport path.
    let cell = shepr_protocol::ProtocolCellSize::from_wire(
        hello.geometry.width(),
        hello.geometry.height(),
        hello.geometry.pixel_mouse,
    );

    if should_quit.is_requested() {
        return Ok(());
    }

    let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::accepted());
    // Keep framing separate from transport so the welcome can also be supplied
    // as pre-encoded bytes without changing the bounded socket write path.
    let welcome = shepr_protocol::encode_message(&welcome)
        .map_err(|error| io::Error::other(error.to_string()))?;
    write_framed_bytes(&mut stream, &welcome, CLIENT_WRITE_STALL_TIMEOUT)?;

    stream.set_read_timeout(None)?;

    // One outbox carries the reliable control lane and the droppable surface slot.
    let write_stream = stream.try_clone()?;
    let shutdown_stream = stream.try_clone()?;
    let outbox = ClientOutbox::for_connection(shutdown_stream, Arc::clone(&wake));
    let writer_queue = outbox.queue_handle();

    // Spawn a writer thread that drains the outbox queue to the stream.
    std::thread::spawn(move || {
        client_writer_loop(write_stream, client_id, &writer_queue, &wake);
    });

    if should_quit.is_requested() {
        send_shutdown_to_unregistered_client(&outbox);
        return Ok(());
    }

    // Notify the main loop about the new client.
    let endpoint_control_writer = outbox.control_sender();
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
        outbox,
    };
    if let Err(err) = server_event_tx.blocking_send(connected)
        && let ServerEvent::ClientShellConnected { outbox, .. } = err.0
    {
        send_shutdown_to_unregistered_client(&outbox);
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

fn send_shutdown_to_unregistered_client(outbox: &ClientOutbox) {
    if outbox.send(&ServerMessage::ServerShutdown {
        reason: Some(shepr_protocol::ShutdownReason::Message(
            "server is shutting down".to_owned(),
        )),
    }) == Delivery::Queued
    {
        // Handshake handling runs on a transport thread, so waiting here
        // does not park the Tokio server loop. The wait is bounded: a writer
        // stuck on a client that stopped reading must not pin this thread
        // forever.
        let mut flushed = outbox.flush_barrier();
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
    writer_queue: &Arc<OutboxQueue>,
    wake: &tokio::sync::Notify,
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
                let result = write_framed_bytes(&mut stream, &data, CLIENT_WRITE_STALL_TIMEOUT);
                if result.is_ok() {
                    // The loop derives surface debt from client state, so all
                    // completed render slots can share a coalescing wake.
                    wake.notify_one();
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
            debug!(?client_id, error = %err, "client write failed, closing writer");
            writer_queue.close_connection();
            break;
        }
    }
    writer_queue.close_connection();
    debug!(?client_id, "client writer thread exiting");
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
    endpoint_control_writer: Option<&ControlSender>,
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
                if let Some(reason) =
                    client_shell_geometry_error(surface_size, geometry.width(), geometry.height())
                {
                    warn!(?client_id, %reason, "invalid client shell resize, closing");
                    send_client_disconnected(server_event_tx, client_id);
                    break;
                }
                // Oversized raw dimensions were rejected above, so
                // `from_wire`'s oversize fallback cannot be reached here.
                let cell = shepr_protocol::ProtocolCellSize::from_wire(
                    geometry.width(),
                    geometry.height(),
                    geometry.pixel_mouse,
                );
                let (cell_width_px, cell_height_px, pixel_mouse) =
                    (cell.width(), cell.height(), cell.exact);
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
                        ServerEvent::ClientPasteRejected { client_id, size }
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
                // Request ids are echoed and held with replies. Boot ids are
                // already canonical bounded values after protocol decoding.
                if request_id.len() > crate::server::client_commands::MAX_ENDPOINT_REQUEST_ID_BYTES
                {
                    warn!(
                        ?client_id,
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
            ClientMessage::ReplayHostEffects => {
                ServerEvent::ClientShellReplayHostEffects { client_id }
            }
            ClientMessage::HealthPing => {
                // This acknowledges transport liveness for this reader and
                // writer, not responsiveness of the headless event loop. A
                // client removed from the registry has its socket shut down,
                // so its reader cannot keep that client healthy with pongs.
                // A pong that cannot be queued (lane overflow or an encode
                // failure) has closed the outbox, which wakes the loop to
                // reap the client; the reader has nothing left to serve.
                let Some(writer) = endpoint_control_writer else {
                    continue;
                };
                if writer.send(&ServerMessage::HealthPong) == Delivery::Closed {
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
mod tests {
    use super::*;
    use crate::limits::{CLIENT_CONTROL_QUEUE_MAX_BYTES, CLIENT_CONTROL_QUEUE_MAX_ITEMS};
    use shepr_protocol::MAX_INPUT_EVENT_BATCH;
    use std::path::PathBuf;
    use std::sync::mpsc::{SendError, TrySendError};
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
        let server = listener.accept().expect("test precondition").0;
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
        client_stream
            .write_all(&shepr_protocol::preamble::local_preamble())
            .expect("write client preamble");
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

    fn test_queue_writer() -> (ClientOutbox, Arc<OutboxQueue>) {
        let queue = OutboxQueue::with_limits(
            None,
            Arc::new(tokio::sync::Notify::new()),
            CLIENT_CONTROL_QUEUE_MAX_ITEMS,
            CLIENT_CONTROL_QUEUE_MAX_BYTES,
        );
        (ClientOutbox::from_queue(Arc::clone(&queue)), queue)
    }

    fn encode_test_frame(message: &ServerMessage) -> Vec<u8> {
        shepr_protocol::encode_frame(message).expect("frame server message")
    }

    #[test]
    fn client_writer_prioritizes_control_before_render() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-writer-priority");
        let (writer, queue) = test_queue_writer();
        writer
            .queue_handle()
            .try_send_render(encode_test_frame(&ServerMessage::WindowTitle {
                title: Some("render".into()),
            }))
            .expect("queue render");
        writer
            .queue_handle()
            .send_control(encode_test_frame(&ServerMessage::WindowTitle {
                title: Some("control".into()),
            }))
            .expect("queue control");

        let wake = Arc::new(tokio::sync::Notify::new());
        let writer_wake = Arc::clone(&wake);
        let handle = std::thread::spawn(move || {
            client_writer_loop(server_stream, ClientId::test_new(9), &queue, &writer_wake);
        });

        match shepr_protocol::read_message(&mut client_stream).expect("read control") {
            ServerMessage::WindowTitle { title } => assert_eq!(title.as_deref(), Some("control")),
            other => panic!("expected control message first, got {other:?}"),
        }
        match shepr_protocol::read_message(&mut client_stream).expect("read render") {
            ServerMessage::WindowTitle { title } => assert_eq!(title.as_deref(), Some("render")),
            other => panic!("expected render message second, got {other:?}"),
        }
        drop(writer);
        handle.join().expect("writer exits after senders drop");
    }

    #[test]
    fn client_writer_wakes_without_using_the_server_event_channel() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-writer-event-backpressure");
        client_stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("test precondition");
        let (writer, queue) = test_queue_writer();
        writer
            .queue_handle()
            .try_send_render(encode_test_frame(&ServerMessage::WindowTitle {
                title: Some("render".into()),
            }))
            .expect("queue render");

        let wake = Arc::new(tokio::sync::Notify::new());
        let writer_wake = Arc::clone(&wake);
        let handle = std::thread::spawn(move || {
            client_writer_loop(server_stream, ClientId::test_new(10), &queue, &writer_wake);
        });

        assert!(matches!(
            shepr_protocol::read_message(&mut client_stream).expect("render is written"),
            ServerMessage::WindowTitle { title: Some(title) } if title == "render"
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), wake.notified())
                .await
                .expect("render drain wakes the server loop");
        });

        drop(writer);
        handle.join().expect("writer exits after handles drop");
    }

    #[test]
    fn closing_client_writer_shuts_down_both_socket_directions() {
        use std::io::Read as _;

        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-writer-close-connection");
        client_stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("test precondition");
        let queue = OutboxQueue::new_for_connection(
            server_stream.try_clone().expect("clone shutdown handle"),
            Arc::new(tokio::sync::Notify::new()),
        );
        let writer = ClientOutbox::from_queue(Arc::clone(&queue));

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
        let wake = Arc::new(tokio::sync::Notify::new());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, ClientId::test_new(11), &queue, &wake);
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
        let cloned_writer = writer.control_sender();
        let wake = Arc::new(tokio::sync::Notify::new());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, ClientId::test_new(12), &queue, &wake);
            done_tx
                .send(())
                .expect("test still waiting for the writer to exit");
        });

        drop(writer);
        assert_eq!(
            cloned_writer.send(&ServerMessage::WindowTitle {
                title: Some("cloned".into()),
            }),
            Delivery::Queued,
            "cloned writer still sends after original drops"
        );
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
        let wake = Arc::new(tokio::sync::Notify::new());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, ClientId::test_new(13), &queue, &wake);
            done_tx
                .send(())
                .expect("test still waiting for the writer to exit");
        });

        drop(client_stream);
        writer
            .queue_handle()
            .send_control(vec![b'x'; 1024 * 1024])
            .expect("message is accepted before the writer observes socket failure");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer exits after socket write failure");

        assert!(matches!(
            writer.queue_handle().send_control(vec![b'y']),
            Err(SendError(_))
        ));
        assert!(matches!(
            writer.queue_handle().try_send_render(vec![b'z']),
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
            .set_read_timeout(Some(Duration::from_secs(3)))
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
    fn a_foreign_client_is_answered_with_the_server_identity_through_the_gate() {
        use crate::test_support::AppPathsFixture as _;
        use std::io::Read;
        let scratch = shepr_test_support::ScratchDir::new("foreign-gate");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let (tx, _rx) = mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
        let stop = Arc::new(shepr_api::ServerStopSignal::default());
        let api = shepr_api::start_server(tx, Arc::clone(&stop), &paths).expect("shared socket");
        let (server_event_tx, mut events) = mpsc::channel(4);
        api.client_gate().open(Arc::new(ClientTransportHandler {
            server_event_tx,
            should_quit: stop,
            wake: Arc::new(tokio::sync::Notify::new()),
            ids: crate::server::clients::ClientIdAllocator::default(),
        }));
        let mut peer = shepr_platform::ipc::connect_local_stream(paths.server_address().socket())
            .expect("connect");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("bound");
        let mut foreign = shepr_protocol::preamble::local_preamble();
        let last = foreign.last_mut().expect("identity byte");
        *last = if *last == b'0' { b'1' } else { b'0' };
        peer.write_all(&foreign).expect("foreign identity");
        shepr_protocol::preamble::read_preamble(&mut peer).expect("server identity");
        let mut rest = Vec::new();
        peer.read_to_end(&mut rest).expect("closed without welcome");
        assert!(rest.is_empty());
        assert!(events.try_recv().is_err());
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
                Instant::now() + HANDSHAKE_TIMEOUT,
                &server_event_tx,
                &handshake_quit,
                Arc::new(tokio::sync::Notify::new()),
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
            .set_read_timeout(Some(Duration::from_secs(2)))
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
                Instant::now() + HANDSHAKE_TIMEOUT,
                &server_event_tx,
                &handshake_quit,
                Arc::new(tokio::sync::Notify::new()),
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
                outbox: writer,
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
    fn an_oversized_payload_is_a_paste_overflow_only_when_pastes_carry_all_text() {
        let paste = ClientPaneInputEvent::Paste("p".repeat(MAX_INPUT_PAYLOAD));
        let empty_commit = ClientPaneInputEvent::TextCommit(String::new());
        let size = MAX_INPUT_PAYLOAD + 1;
        assert_eq!(
            pane_input_event_limit(&[
                paste.clone(),
                empty_commit,
                ClientPaneInputEvent::Paste("q".into())
            ]),
            InputEventLimit::PasteTooLarge { size }
        );
        assert_eq!(
            pane_input_event_limit(&[paste.clone(), ClientPaneInputEvent::TextCommit("x".into())]),
            InputEventLimit::InputPayloadTooLarge { size }
        );
        assert_eq!(
            pane_input_event_limit(&[paste]),
            InputEventLimit::WithinLimits
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
