use super::*;
use tracing::debug;

pub(super) fn start_endpoint_transport(
    stream: LocalStream,
    lifetime: impl Send + 'static,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    surface_decoder: shepr_protocol::surface_reuse::Decoder,
) -> Result<endpoint::NativeEndpointTransport, ClientError> {
    let reader = stream.try_clone().map_err(ClientError::ConnectionFailed)?;
    let transport = endpoint::NativeEndpointTransport::with_lifetime(stream, lifetime)
        .map_err(ClientError::ConnectionFailed)?;
    let stopped = transport.stop_handle();
    spawn_endpoint_reader(
        reader,
        event_tx,
        &stopped,
        endpoint_id,
        generation,
        surface_decoder,
    )?;
    Ok(transport)
}

pub(super) fn spawn_endpoint_reader(
    reader: LocalStream,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    stopped: &Arc<AtomicBool>,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    surface_decoder: shepr_protocol::surface_reuse::Decoder,
) -> Result<(), ClientError> {
    let event_tx = event_tx.clone();
    let stopped = Arc::clone(stopped);
    std::thread::Builder::new()
        .name("endpoint-reader".into())
        .spawn(move || {
            server_reader_thread(
                reader,
                &event_tx,
                &stopped,
                endpoint_id,
                generation,
                surface_decoder,
            );
        })
        .map_err(ClientError::ConnectionFailed)?;
    Ok(())
}

/// Reads complete frames while retaining partial-read progress across nonblocking polls.
pub(super) fn server_reader_thread(
    mut stream: LocalStream,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    mut surface_decoder: shepr_protocol::surface_reuse::Decoder,
) {
    if let Err(error) = stream.set_nonblocking(true) {
        report_disconnect(
            event_tx,
            ClientLoopEvent::ServerDisconnected {
                endpoint_id,
                generation,
                error,
            },
        );
        return;
    }

    let mut stream = EndpointReader {
        stream: &mut stream,
        stopped: should_quit,
    };
    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }

        let message = shepr_protocol::read_message(&mut stream).and_then(|message| {
            surface_decoder
                .decode(message)
                .map_err(shepr_protocol::FramingError::SurfaceDecode)
        });
        match message {
            Ok(msg) => {
                if event_tx
                    .blocking_send(ClientLoopEvent::ServerMessage {
                        endpoint_id: endpoint_id.clone(),
                        generation,
                        message: Box::new(msg),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(shepr_protocol::FramingError::UnexpectedEof) => {
                debug!(
                    endpoint = %endpoint_id.storage_key(),
                    generation,
                    "server closed connection"
                );
                report_disconnect(
                    event_tx,
                    ClientLoopEvent::ServerDisconnected {
                        endpoint_id: endpoint_id.clone(),
                        generation,
                        error: framing_error_to_io(
                            shepr_protocol::FramingError::UnexpectedEof,
                            endpoint_id.clone(),
                        ),
                    },
                );
                break;
            }
            // `EndpointReader` waits out WouldBlock itself, so any error here is final.
            Err(err) => {
                warn!(
                    endpoint = %endpoint_id.storage_key(),
                    generation,
                    error = %err,
                    "server read error"
                );
                report_disconnect(
                    event_tx,
                    ClientLoopEvent::ServerDisconnected {
                        endpoint_id: endpoint_id.clone(),
                        generation,
                        error: framing_error_to_io(err, endpoint_id.clone()),
                    },
                );
                break;
            }
        }
    }
}

/// Hands a reader's final disconnect to the client loop. The send fails only once the loop
/// has exited and dropped its receiver, and then no one is left to act on the disconnect:
/// the loop is tearing every endpoint down anyway.
fn report_disconnect(
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    disconnect: ClientLoopEvent,
) {
    event_tx.blocking_send(disconnect).ok();
}

fn framing_error_to_io(
    error: shepr_protocol::FramingError,
    endpoint_id: endpoint::ClientEndpointId,
) -> io::Error {
    let kind = match &error {
        shepr_protocol::FramingError::UnexpectedEof => io::ErrorKind::UnexpectedEof,
        shepr_protocol::FramingError::Io(error) => error.kind(),
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(
        kind,
        EndpointFramingError {
            endpoint_id,
            source: error,
        },
    )
}

#[derive(Debug)]
struct EndpointFramingError {
    endpoint_id: endpoint::ClientEndpointId,
    source: shepr_protocol::FramingError,
}

impl std::fmt::Display for EndpointFramingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "endpoint {}: ", self.endpoint_id.storage_key())?;
        if matches!(&self.source, shepr_protocol::FramingError::UnexpectedEof) {
            formatter.write_str("server closed connection")
        } else {
            write!(formatter, "{}", self.source)
        }
    }
}

// Display includes the framing cause, so leave the source chain empty to avoid repeating it.
impl std::error::Error for EndpointFramingError {}

struct EndpointReader<'a> {
    stream: &'a mut LocalStream,
    stopped: &'a AtomicBool,
}

impl io::Read for EndpointReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return Ok(0);
            }
            match shepr_platform::ipc::poll_local_stream_read_count(self.stream, buffer)? {
                shepr_platform::ipc::LocalStreamReadCount::Data(count) => return Ok(count),
                shepr_platform::ipc::LocalStreamReadCount::Closed => return Ok(0),
                shepr_platform::ipc::LocalStreamReadCount::Pending => {
                    shepr_platform::wait_client_stream_readable(self.stream)?;
                }
            }
        }
    }
}

/// Writes one client message, which crosses in one frame: the server reads
/// client messages with a one-frame cap, so a larger one fails here.
pub(crate) fn write_to_local_server(
    stream: &mut LocalStream,
    msg: &ClientMessage,
) -> io::Result<()> {
    let frame = shepr_protocol::encode_frame(msg).map_err(io::Error::other)?;
    io::Write::write_all(stream, &frame)?;
    io::Write::flush(stream)
}

pub(super) trait ClientMessageSink {
    fn send_client_message(&mut self, message: &ClientMessage) -> io::Result<()>;
}

impl ClientMessageSink for LocalStream {
    fn send_client_message(&mut self, message: &ClientMessage) -> io::Result<()> {
        write_to_local_server(self, message)
    }
}

impl ClientMessageSink for endpoint::EndpointRegistry {
    fn send_client_message(&mut self, message: &ClientMessage) -> io::Result<()> {
        // The lifecycle loop consumes failures for every endpoint, including Local. A send
        // failure must not bypass that transition or tear down unrelated connections.
        self.send(message);
        Ok(())
    }
}

pub(super) fn write_to_server(
    stream: &mut impl ClientMessageSink,
    msg: &ClientMessage,
) -> io::Result<()> {
    stream.send_client_message(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointTransport as _;
    use interprocess::local_socket::traits::{Listener as _, Stream as _};
    use std::io::{Read as _, Write as _};
    use std::time::{Duration, Instant};

    #[test]
    fn server_reader_errors_keep_eof_io_and_decode_causes() {
        let endpoint_id = endpoint::ClientEndpointId::Local;
        let eof = framing_error_to_io(
            shepr_protocol::FramingError::UnexpectedEof,
            endpoint_id.clone(),
        );
        assert_eq!(eof.kind(), io::ErrorKind::UnexpectedEof);
        assert!(eof.to_string().contains("server closed connection"));
        assert!(eof.to_string().contains("endpoint local"));
        assert!(!eof.to_string().contains("generation"));

        let io_error = framing_error_to_io(
            shepr_protocol::FramingError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "peer reset",
            )),
            endpoint_id.clone(),
        );
        assert_eq!(io_error.kind(), io::ErrorKind::BrokenPipe);
        assert!(io_error.to_string().contains("peer reset"));

        let decode_error = framing_error_to_io(
            shepr_protocol::FramingError::Oversized {
                claimed: 32,
                max: 16,
            },
            endpoint_id.clone(),
        );
        assert_eq!(decode_error.kind(), io::ErrorKind::InvalidData);
        assert!(
            decode_error
                .to_string()
                .contains("frame size 32 exceeds maximum 16")
        );

        let surface_error = framing_error_to_io(
            shepr_protocol::FramingError::SurfaceDecode(
                shepr_protocol::surface_reuse::SurfaceDecodeError::WithSubject {
                    source: Box::new(
                        shepr_protocol::surface_reuse::SurfaceDecodeError::BaselineMismatch,
                    ),
                    subject: shepr_protocol::surface_reuse::SurfaceDecodeSubject {
                        boot_id: crate::tests::test_boot_id("boot"),
                        projection_revision: shepr_protocol::ProjectionRevision::new(2),
                        surface_revision: shepr_protocol::SurfaceRevision::new(3),
                        pane_ids: Vec::new(),
                    },
                },
            ),
            endpoint_id,
        );
        assert_eq!(surface_error.kind(), io::ErrorKind::InvalidData);
        assert!(matches!(
            surface_error.get_ref().and_then(|error| error.downcast_ref::<EndpointFramingError>()),
            Some(error) if matches!(
                &error.source,
                shepr_protocol::FramingError::SurfaceDecode(
                    shepr_protocol::surface_reuse::SurfaceDecodeError::WithSubject {
                        subject,
                        source,
                    }
                ) if subject.boot_id == crate::tests::test_boot_id("boot")
                    && subject.projection_revision == 2
                    && subject.surface_revision == 3
                    && matches!(
                        source.as_ref(),
                        shepr_protocol::surface_reuse::SurfaceDecodeError::BaselineMismatch
                    )
            ) && error.endpoint_id == endpoint::ClientEndpointId::Local
        ));
        assert!(
            surface_error
                .to_string()
                .contains(&format!("boot {}", crate::tests::test_boot_id("boot")))
        );
        assert!(surface_error.to_string().contains("projection revision 2"));
    }

    #[test]
    fn upload_cancellation_preserves_pending_endpoint_download() {
        let scratch = shepr_test_support::ScratchDir::new("cancel");
        let path = scratch.join("s.sock");
        let listener = shepr_platform::ipc::bind_local_listener(&path).expect("test precondition");
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        let mut bridge = listener.accept().expect("test precondition");
        std::fs::remove_file(path).expect("test precondition");
        drop(listener);
        let mut reader_stream = client.try_clone().expect("test precondition");
        let mut writer = endpoint::NativeEndpointTransport::with_lifetime(client, ())
            .expect("test precondition");
        let stopped = writer.stop_handle();
        struct ForwardedInput(std::sync::mpsc::Sender<Vec<u8>>);
        impl io::Write for ForwardedInput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0
                    .send(bytes.to_vec())
                    .map_err(|error| io::Error::other(error.to_string()))?;
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (forwarded_tx, forwarded_rx) = std::sync::mpsc::channel();
        let upload_stream = bridge.try_clone().expect("test precondition");
        upload_stream
            .set_nonblocking(true)
            .expect("test stream supports nonblocking mode");
        let upload = shepr_remote::BridgeUpload::spawn(
            upload_stream,
            ForwardedInput(forwarded_tx),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .expect("test bridge upload starts");
        let cancel = move || {
            upload.cancel();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !upload.is_finished() {
                assert!(
                    Instant::now() < deadline,
                    "upload worker completes within timeout"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let end = upload.join().expect("upload worker does not panic");
            end.result.expect("upload copy completes without error");
            assert!(
                !end.client_closed,
                "upload cancellation must not report peer EOF"
            );
        };
        let message = ClientMessage::ClientShellFocus { focused: false };
        let mut expected = Vec::new();
        shepr_protocol::write_message(&mut expected, &message).expect("test precondition");
        writer.send(&message).expect("test precondition");
        let mut forwarded = Vec::new();
        while forwarded.len() < expected.len() {
            forwarded.extend(
                forwarded_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("test precondition"),
            );
        }
        assert_eq!(forwarded, expected);
        cancel();

        // A client write after upload cancellation must not stop the download reader.
        writer
            .send(&ClientMessage::ClientShellFocus { focused: true })
            .expect("test precondition");
        let flushed = writer.flush(Instant::now() + Duration::from_secs(3));
        if flushed.is_ok() {
            let received: ClientMessage =
                shepr_protocol::read_message(&mut bridge).expect("test precondition");
            assert_eq!(received, ClientMessage::ClientShellFocus { focused: true });
        }
        const FINAL: &[u8] = b"pending-download: FINAL OUTPUT\n";
        bridge.write_all(FINAL).expect("test precondition");
        drop(bridge);
        let mut output = Vec::new();
        EndpointReader {
            stream: &mut reader_stream,
            stopped: &stopped,
        }
        .read_to_end(&mut output)
        .expect("test precondition");
        assert_eq!(output, FINAL);
        assert!(flushed.is_ok(), "client write failed: {flushed:?}");
        assert!(!stopped.load(Ordering::Acquire));
        assert!(writer.take_error().is_none());
    }
}
