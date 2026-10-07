mod writer;
pub(crate) use writer::{EndpointReadActivity, NativeEndpointTransport};

use crate::errors::{ClientRunError, endpoint_connection_launch_error};
use crate::events::ClientLoopEvent;
use crate::{endpoint, handshake};
use shepr_platform::ipc::LocalStream;
use shepr_protocol::ServerMessage;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::debug;

pub(crate) struct AcceptedEndpoint {
    stream: LocalStream,
    remote_connection: Option<Arc<shepr_remote::MachineSshConnection>>,
}

/// How a connection ended: a machine's SSH connection classifies its own end
/// (its SSH diagnostic wins over `error`), and a local stream's error stands.
/// Blocks for the SSH worker's bounded join, so only reader and attempt threads
/// call it.
fn connection_end(
    connection: Option<&shepr_remote::MachineSshConnection>,
    error: io::Error,
) -> io::Error {
    match connection {
        Some(connection) => connection.ended(error),
        None => error,
    }
}

pub(crate) enum LocalAttachFailure {
    Connection(io::Error),
    Handshake(io::Error),
    Setup(io::Error),
}

impl LocalAttachFailure {
    pub(crate) fn initial_failure(&self) -> Option<shepr_launch::EndpointFailure> {
        match self {
            Self::Connection(_) => None,
            Self::Handshake(error) | Self::Setup(error) => {
                Some(shepr_launch::EndpointFailure::from_error(error))
            }
        }
    }

    pub(crate) fn into_launch_error(self) -> ClientRunError {
        match self {
            Self::Connection(error) => endpoint_connection_launch_error(error),
            Self::Handshake(error) | Self::Setup(error) => ClientRunError::Launch(error),
        }
    }
}

pub(crate) fn attach_local_endpoint(
    path: &std::path::Path,
    hello: shepr_protocol::endpoint::EndpointClientHello,
    mismatch_guidance: &str,
) -> Result<AcceptedEndpoint, LocalAttachFailure> {
    let stream = shepr_platform::ipc::connect_trusted_local_stream(path)
        .map_err(LocalAttachFailure::Connection)?
        .into_local_stream();
    attach_endpoint_stream(
        stream,
        hello,
        endpoint::EndpointPolicy::Local,
        None,
        Some(mismatch_guidance),
        None,
    )
    .map_err(LocalAttachFailure::Handshake)
}

pub(crate) fn attach_endpoint_stream(
    mut stream: LocalStream,
    hello: shepr_protocol::endpoint::EndpointClientHello,
    endpoint_policy: endpoint::EndpointPolicy,
    deadline: Option<std::time::Instant>,
    mismatch_guidance: Option<&str>,
    remote_connection: Option<shepr_remote::MachineSshConnection>,
) -> io::Result<AcceptedEndpoint> {
    if let Err(error) =
        handshake::do_handshake_for_endpoint(&mut stream, hello, endpoint_policy, deadline)
    {
        return Err(connection_end(
            remote_connection.as_ref(),
            error.class(mismatch_guidance),
        ));
    }
    let remote_connection = remote_connection.map(Arc::new);
    Ok(AcceptedEndpoint {
        stream,
        remote_connection,
    })
}

/// Owns both halves of one accepted connection, for launch and supervised attaches alike.
/// The reader thread is spawned at assembly but publishes nothing until `activate`, which
/// runs once the generation is accepted and right before the writer is registered, so no
/// message reaches the loop ahead of its connection. Dropping it unactivated ends the reader.
pub(crate) struct EndpointConnectionIo {
    writer: NativeEndpointTransport,
    start_reader: std::sync::mpsc::Sender<()>,
}

impl EndpointConnectionIo {
    pub(crate) fn start(
        accepted: AcceptedEndpoint,
        event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
        endpoint_id: endpoint::ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
    ) -> io::Result<Self> {
        shepr_platform::structured_log!(INFO, event = endpoint.handshake, outcome = Accepted, endpoint = %endpoint_id, %generation, "endpoint handshake accepted");
        let assemble = || -> io::Result<Self> {
            let reader = accepted.stream.try_clone()?;
            let writer = match &accepted.remote_connection {
                Some(connection) => NativeEndpointTransport::with_remote_connection(
                    accepted.stream,
                    Arc::clone(connection),
                )?,
                None => NativeEndpointTransport::with_lifetime(accepted.stream, ())?,
            };
            let (start_reader, wait) = std::sync::mpsc::channel();
            spawn_endpoint_reader(
                reader,
                event_tx,
                &writer,
                endpoint_id,
                generation,
                wait,
                accepted.remote_connection,
            )?;
            Ok(Self {
                writer,
                start_reader,
            })
        };
        // Acceptance already succeeded. Every failure from here is local setup, regardless
        // of its IO kind; launch and retry callers use this same classified result.
        assemble().map_err(|error| {
            io::Error::new(
                error.kind(),
                shepr_launch::EndpointFailure::local_setup(format!(
                    "failed to set up configured endpoint transport: {error}"
                )),
            )
        })
    }

    pub(crate) fn activate(self) -> NativeEndpointTransport {
        self.start_reader.send(()).ok();
        self.writer
    }
}

fn spawn_endpoint_reader(
    reader: LocalStream,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    transport: &NativeEndpointTransport,
    endpoint_id: endpoint::ClientEndpointId,
    generation: shepr_protocol::ConnectionGeneration,
    wait: std::sync::mpsc::Receiver<()>,
    remote_connection: Option<Arc<shepr_remote::MachineSshConnection>>,
) -> io::Result<()> {
    let event_tx = event_tx.clone();
    let stopped = transport.stop_handle();
    let read_activity = transport.read_activity();
    std::thread::Builder::new()
        .name("endpoint-reader".into())
        .spawn(move || {
            if wait.recv().is_err() {
                return;
            }
            server_reader_thread(
                reader,
                &event_tx,
                &stopped,
                &read_activity,
                &endpoint_id,
                generation,
                remote_connection.as_deref(),
            );
        })?;
    Ok(())
}

/// Why the endpoint reader stopped: the framing layer failed to deliver a
/// message, or a delivered message did not decode against the connection's
/// surface baseline.
#[derive(Debug)]
enum EndpointReadError {
    Framing(shepr_protocol::FramingError),
    SurfaceDecode(shepr_surface::decode::SurfaceDecodeError),
}

impl std::fmt::Display for EndpointReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Framing(error) => write!(f, "{error}"),
            Self::SurfaceDecode(error) => write!(f, "surface decode error: {error}"),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for EndpointReadError {}

/// Reads complete frames while retaining partial-read progress across nonblocking polls.
fn server_reader_thread(
    mut stream: LocalStream,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    transport_stopped: &Arc<AtomicBool>,
    read_activity: &EndpointReadActivity,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: shepr_protocol::ConnectionGeneration,
    remote_connection: Option<&shepr_remote::MachineSshConnection>,
) {
    let mut surface_decoder = shepr_surface::decode::Decoder::default();
    // The reader is a clone of the writer's stream, sharing one file description, which
    // the writer set nonblocking during assembly; no setup remains here.
    let mut stream = EndpointReader {
        stream: &mut stream,
        stopped: transport_stopped,
    };
    loop {
        if transport_stopped.load(Ordering::Acquire) {
            if let Some(connection) = remote_connection {
                connection.close();
            }
            break;
        }

        let message = shepr_protocol::read_message::<_, ServerMessage>(&mut stream)
            .map_err(EndpointReadError::Framing)
            .and_then(|message| {
                // clock-io-ok: stamps when this frame came off the transport, for health.
                read_activity.record(
                    std::time::Instant::now(),
                    matches!(message, ServerMessage::EndpointSnapshot(_)),
                );
                surface_decoder
                    .decode_client(message)
                    .map_err(EndpointReadError::SurfaceDecode)
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
            Err(EndpointReadError::Framing(shepr_protocol::FramingError::UnexpectedEof)) => {
                if transport_stopped.load(Ordering::Acquire) {
                    if let Some(connection) = remote_connection {
                        connection.close();
                    }
                    break;
                }
                debug!(
                    endpoint = %endpoint_id,
                    %generation,
                    "server closed connection"
                );
                report_disconnect(
                    event_tx,
                    ClientLoopEvent::ServerDisconnected {
                        endpoint_id: endpoint_id.clone(),
                        generation,
                        error: connection_end(
                            remote_connection,
                            read_error_to_io(
                                EndpointReadError::Framing(
                                    shepr_protocol::FramingError::UnexpectedEof,
                                ),
                                endpoint_id.clone(),
                            ),
                        ),
                    },
                );
                break;
            }
            // `EndpointReader` waits out WouldBlock itself, so any error here is final.
            Err(err) => {
                shepr_platform::structured_log!(
                    WARN, event = endpoint.read, outcome = Error,
                    endpoint = %endpoint_id,
                    %generation,
                    error = %err,
                    "server read error"
                );
                report_disconnect(
                    event_tx,
                    ClientLoopEvent::ServerDisconnected {
                        endpoint_id: endpoint_id.clone(),
                        generation,
                        error: connection_end(
                            remote_connection,
                            read_error_to_io(err, endpoint_id.clone()),
                        ),
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

fn read_error_to_io(
    error: EndpointReadError,
    endpoint_id: endpoint::ClientEndpointId,
) -> io::Error {
    let kind = match &error {
        EndpointReadError::Framing(shepr_protocol::FramingError::UnexpectedEof) => {
            io::ErrorKind::UnexpectedEof
        }
        EndpointReadError::Framing(shepr_protocol::FramingError::Io(error)) => error.kind(),
        EndpointReadError::Framing(_) | EndpointReadError::SurfaceDecode(_) => {
            io::ErrorKind::InvalidData
        }
    };
    let framed = EndpointFramingError {
        endpoint_id,
        source: error,
    };
    let failure = match &framed.source {
        EndpointReadError::Framing(shepr_protocol::FramingError::Io(error)) => {
            shepr_launch::EndpointFailure::from_error(error)
                .with_context(&format!("endpoint {}", framed.endpoint_id))
        }
        EndpointReadError::Framing(shepr_protocol::FramingError::UnexpectedEof) => {
            shepr_launch::EndpointFailure::from_error(&io::Error::new(kind, framed))
        }
        EndpointReadError::Framing(_) | EndpointReadError::SurfaceDecode(_) => {
            shepr_launch::EndpointFailure::incompatible(framed.to_string())
        }
    };
    io::Error::new(kind, failure)
}

#[derive(Debug)]
struct EndpointFramingError {
    endpoint_id: endpoint::ClientEndpointId,
    source: EndpointReadError,
}

impl std::fmt::Display for EndpointFramingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "endpoint {}: ", self.endpoint_id)?;
        if matches!(
            &self.source,
            EndpointReadError::Framing(shepr_protocol::FramingError::UnexpectedEof)
        ) {
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
    use std::time::Duration;

    #[test]
    fn server_reader_errors_keep_eof_io_and_decode_causes() {
        let endpoint_id = endpoint::ClientEndpointId::Local;
        let eof = read_error_to_io(
            EndpointReadError::Framing(shepr_protocol::FramingError::UnexpectedEof),
            endpoint_id.clone(),
        );
        assert_eq!(eof.kind(), io::ErrorKind::UnexpectedEof);
        assert!(eof.to_string().contains("server closed connection"));
        assert!(eof.to_string().contains("endpoint local"));
        assert!(!eof.to_string().contains("generation"));

        let io_error = read_error_to_io(
            EndpointReadError::Framing(shepr_protocol::FramingError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "peer reset",
            ))),
            endpoint_id.clone(),
        );
        assert_eq!(io_error.kind(), io::ErrorKind::BrokenPipe);
        assert!(io_error.to_string().contains("peer reset"));

        let decode_error = read_error_to_io(
            EndpointReadError::Framing(shepr_protocol::FramingError::LimitExceeded(
                shepr_protocol::LimitExceeded::new(
                    shepr_protocol::Limit::new(shepr_protocol::LimitKind::MessageBytes, 16),
                    32,
                ),
            )),
            endpoint_id.clone(),
        );
        assert_eq!(decode_error.kind(), io::ErrorKind::InvalidData);
        assert!(
            decode_error
                .to_string()
                .contains("message of 32 bytes exceeds its limit of 16 bytes")
        );

        let surface_error = read_error_to_io(
            EndpointReadError::SurfaceDecode(
                shepr_surface::decode::SurfaceDecodeError::WithSubject {
                    source: Box::new(shepr_surface::decode::SurfaceDecodeError::BaselineMismatch),
                    subject: Box::new(shepr_surface::decode::SurfaceDecodeSubject {
                        boot_id: crate::tests::test_boot_id("boot"),
                        projection_revision: shepr_test_fixtures::counter_at(2),
                        surface_revision: shepr_test_fixtures::counter_at(3),
                        pane_ids: Vec::new(),
                    }),
                },
            ),
            endpoint_id,
        );
        assert_eq!(surface_error.kind(), io::ErrorKind::InvalidData);
        let failure = surface_error
            .get_ref()
            .and_then(|error| error.downcast_ref::<shepr_launch::EndpointFailure>())
            .expect("the io error carries the typed endpoint failure");
        assert_eq!(
            failure.disposition(),
            shepr_launch::FailureDisposition::Incompatible
        );
        assert!(failure.to_string().contains("endpoint local"));
        assert!(
            surface_error
                .to_string()
                .contains(&format!("boot {}", crate::tests::test_boot_id("boot")))
        );
        assert!(surface_error.to_string().contains("projection revision 2"));
        assert!(surface_error.to_string().contains("surface decode error: "));
    }

    #[tokio::test]
    async fn an_assembled_reader_waits_for_connection_acceptance() {
        let scratch = shepr_test_support::ScratchDir::new("reader-activation");
        let path = scratch.join("s.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test listener");
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test client");
        let mut peer = listener.accept().expect("test peer").0;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
        let connection = EndpointConnectionIo::start(
            AcceptedEndpoint {
                stream: client,
                remote_connection: None,
            },
            &event_tx,
            endpoint::ClientEndpointId::Local,
            crate::tests::test_generation(7),
        )
        .expect("test connection assembly");
        shepr_protocol::write_message(&mut peer, &ServerMessage::HealthPong).expect("test frame");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), event_rx.recv())
                .await
                .is_err(),
            "an unaccepted reader cannot publish",
        );
        let mut writer = connection.activate();
        let event = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .expect("reader activation is bounded")
            .expect("activated reader publishes");
        assert!(matches!(
            event,
            ClientLoopEvent::ServerMessage { generation, .. }
                if generation == crate::tests::test_generation(7)
        ));
        writer.disconnect();
    }

    #[tokio::test]
    async fn dropping_an_unactivated_connection_ends_its_reader() {
        let scratch = shepr_test_support::ScratchDir::new("reader-abandoned");
        let path = scratch.join("s.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test listener");
        let stream = shepr_platform::ipc::connect_local_stream(&path).expect("test client");
        let _peer = listener.accept().expect("test peer").0;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
        let connection = EndpointConnectionIo::start(
            AcceptedEndpoint {
                stream,
                remote_connection: None,
            },
            &event_tx,
            endpoint::ClientEndpointId::Local,
            crate::tests::test_generation(7),
        )
        .expect("test connection assembly");
        drop(event_tx);
        drop(connection);
        // The reader owns the last sender. Closure proves that it exited, even
        // while the peer stays connected and sends no bytes to wake a read.
        assert!(
            tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
                .await
                .expect("abandoned reader exits within the deadline")
                .is_none()
        );
    }
}
