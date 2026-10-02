use super::*;
use shepr_platform::ipc::LocalStream;
use tracing::debug;

pub(super) struct AttachedEndpoint {
    pub(super) reader: LocalStream,
    pub(super) writer: endpoint::NativeEndpointTransport,
}

pub(super) enum EndpointAttachFailure {
    Handshake(io::Error),
    Setup(io::Error),
}

pub(super) enum LocalAttachFailure {
    Connection(io::Error),
    Handshake(io::Error),
    Setup(io::Error),
}

impl LocalAttachFailure {
    pub(super) fn initial_failure(&self) -> Option<shepr_remote::EndpointFailure> {
        match self {
            Self::Connection(_) => None,
            Self::Handshake(error) => Some(shepr_remote::EndpointFailure::from_error(error)),
            Self::Setup(error) => Some(shepr_remote::EndpointFailure::local_setup(
                error.to_string(),
            )),
        }
    }

    pub(super) fn into_launch_error(self) -> ClientRunError {
        match self {
            Self::Connection(error) => ClientRunError::Launch(io::Error::new(
                error.kind(),
                ClientError::ConnectionFailed(error),
            )),
            Self::Handshake(error) => ClientRunError::Launch(error),
            Self::Setup(error) => {
                ClientRunError::Launch(io::Error::other(ClientError::EndpointSetup(error)))
            }
        }
    }
}

pub(super) fn attach_local_endpoint(
    path: &std::path::Path,
    geometry: shepr_protocol::TerminalGeometry,
    mouse_capture: bool,
    surface_active: bool,
    mismatch_guidance: &str,
) -> Result<AttachedEndpoint, LocalAttachFailure> {
    let stream = shepr_platform::ipc::connect_trusted_local_stream(path)
        .map_err(LocalAttachFailure::Connection)?;
    attach_endpoint_stream(
        stream,
        geometry,
        mouse_capture,
        surface_active,
        endpoint::EndpointPolicy::Local,
        None,
        Some(mismatch_guidance),
        None,
    )
    .map_err(|failure| match failure {
        EndpointAttachFailure::Handshake(error) => LocalAttachFailure::Handshake(error),
        EndpointAttachFailure::Setup(error) => LocalAttachFailure::Setup(error),
    })
}

pub(super) fn attach_endpoint_stream(
    mut stream: LocalStream,
    geometry: shepr_protocol::TerminalGeometry,
    mouse_capture: bool,
    surface_active: bool,
    endpoint_policy: endpoint::EndpointPolicy,
    deadline: Option<std::time::Instant>,
    mismatch_guidance: Option<&str>,
    ssh_bridge: Option<shepr_remote::MachineSshBridge>,
) -> Result<AttachedEndpoint, EndpointAttachFailure> {
    if let Err(error) = handshake::do_handshake_for_endpoint(
        &mut stream,
        geometry,
        mouse_capture,
        surface_active,
        endpoint_policy,
        deadline,
    ) {
        return Err(EndpointAttachFailure::Handshake(
            endpoint::classify_handshake_error(error, mismatch_guidance, ssh_bridge.as_ref()),
        ));
    }
    let reader = stream.try_clone().map_err(EndpointAttachFailure::Setup)?;
    let lifetime: Box<dyn Send> = match ssh_bridge {
        Some(bridge) => Box::new(bridge),
        None => Box::new(()),
    };
    let writer = endpoint::NativeEndpointTransport::with_lifetime(stream, lifetime)
        .map_err(EndpointAttachFailure::Setup)?;
    Ok(AttachedEndpoint { reader, writer })
}

pub(super) fn spawn_endpoint_reader(
    reader: LocalStream,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    transport: &endpoint::NativeEndpointTransport,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    surface_decoder: shepr_protocol::surface_reuse::Decoder,
) -> Result<(), ClientError> {
    let event_tx = event_tx.clone();
    let stopped = transport.stop_handle();
    let read_activity = transport.read_activity();
    std::thread::Builder::new()
        .name("endpoint-reader".into())
        .spawn(move || {
            server_reader_thread(
                reader,
                &event_tx,
                &stopped,
                &read_activity,
                endpoint_id,
                generation,
                surface_decoder,
            );
        })
        .map_err(ClientError::EndpointSetup)?;
    Ok(())
}

/// Reads complete frames while retaining partial-read progress across nonblocking polls.
pub(super) fn server_reader_thread(
    mut stream: LocalStream,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    transport_stopped: &Arc<AtomicBool>,
    read_activity: &endpoint::EndpointReadActivity,
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
                error: io::Error::new(
                    error.kind(),
                    shepr_remote::EndpointFailure::local_setup(error.to_string()),
                ),
            },
        );
        return;
    }

    let mut stream = EndpointReader {
        stream: &mut stream,
        stopped: transport_stopped,
    };
    loop {
        if transport_stopped.load(Ordering::Acquire) {
            break;
        }

        let message =
            shepr_protocol::read_message::<_, ServerMessage>(&mut stream).and_then(|message| {
                // clock-io-ok: stamps when this frame came off the transport, for health.
                read_activity.record(
                    std::time::Instant::now(),
                    matches!(message, ServerMessage::EndpointSnapshot(_)),
                );
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
    let framed = EndpointFramingError {
        endpoint_id,
        source: error,
    };
    let failure = match &framed.source {
        shepr_protocol::FramingError::Io(error) => shepr_remote::EndpointFailure::from_error(error)
            .with_context(&format!("endpoint {}", framed.endpoint_id.storage_key())),
        shepr_protocol::FramingError::UnexpectedEof => {
            shepr_remote::EndpointFailure::from_error(&io::Error::new(kind, framed))
        }
        _ => shepr_remote::EndpointFailure::incompatible(framed.to_string()),
    };
    io::Error::new(kind, failure)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::EndpointTransport as _;
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
        let failure = surface_error
            .get_ref()
            .and_then(|error| error.downcast_ref::<shepr_remote::EndpointFailure>())
            .expect("the io error carries the typed endpoint failure");
        assert_eq!(
            failure.disposition(),
            shepr_remote::FailureDisposition::Incompatible
        );
        assert!(failure.to_string().contains("endpoint local"));
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
        let mut bridge = listener.accept().expect("test precondition").0;
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
