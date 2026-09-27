use super::*;

pub(super) fn start_endpoint_transport(
    stream: LocalStream,
    lifetime: impl Send + 'static,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    max_frame_size: usize,
    surface_decoder: protocol::surface_reuse::Decoder,
) -> Result<endpoint::NativeEndpointTransport, ClientError> {
    let reader = stream.try_clone().map_err(ClientError::ConnectionFailed)?;
    let transport = endpoint::NativeEndpointTransport::with_lifetime(stream, lifetime)
        .map_err(ClientError::ConnectionFailed)?;
    let stopped = transport.stop_handle();
    spawn_endpoint_reader(
        reader,
        event_tx,
        &stopped,
        max_frame_size,
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
    max_frame_size: usize,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    surface_decoder: protocol::surface_reuse::Decoder,
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
                max_frame_size,
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
    max_frame_size: usize,
    endpoint_id: endpoint::ClientEndpointId,
    generation: u64,
    mut surface_decoder: protocol::surface_reuse::Decoder,
) {
    if let Err(error) = stream.set_nonblocking(true) {
        let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected {
            endpoint_id,
            generation,
            error,
        });
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

        let message = protocol::read_message(&mut stream, max_frame_size).and_then(|message| {
            surface_decoder
                .decode(message)
                .map_err(|error| protocol::FramingError::SurfaceDecode(error.to_string()))
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
            Err(protocol::FramingError::UnexpectedEof) => {
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected {
                    endpoint_id: endpoint_id.clone(),
                    generation,
                    error: framing_error_to_io(protocol::FramingError::UnexpectedEof),
                });
                break;
            }
            // `EndpointReader` waits out WouldBlock itself, so any error here is final.
            Err(err) => {
                warn!(err = %err, "server read error");
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected {
                    endpoint_id: endpoint_id.clone(),
                    generation,
                    error: framing_error_to_io(err),
                });
                break;
            }
        }
    }
}

fn framing_error_to_io(error: protocol::FramingError) -> io::Error {
    match error {
        protocol::FramingError::UnexpectedEof => {
            io::Error::new(io::ErrorKind::UnexpectedEof, "server closed connection")
        }
        protocol::FramingError::Io(error) => error,
        error => io::Error::new(io::ErrorKind::InvalidData, error),
    }
}

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
            match crate::ipc::poll_local_stream_read_count(self.stream, buffer)? {
                crate::ipc::LocalStreamReadCount::Data(count) => return Ok(count),
                crate::ipc::LocalStreamReadCount::Closed => return Ok(0),
                crate::ipc::LocalStreamReadCount::Pending => {
                    shepr_platform::wait_client_stream_readable(self.stream)?;
                }
            }
        }
    }
}

pub(in crate::client) fn write_to_local_server(
    stream: &mut LocalStream,
    msg: &ClientMessage,
) -> io::Result<()> {
    protocol::write_message(stream, msg).map_err(|error| io::Error::other(error.to_string()))
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
    use crate::client::endpoint::EndpointTransport as _;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::{Read as _, Write as _};
    use std::time::Instant;

    #[test]
    fn server_reader_errors_keep_eof_io_and_decode_causes() {
        let eof = framing_error_to_io(protocol::FramingError::UnexpectedEof);
        assert_eq!(eof.kind(), io::ErrorKind::UnexpectedEof);
        assert!(eof.to_string().contains("server closed connection"));

        let io_error = framing_error_to_io(protocol::FramingError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "peer reset",
        )));
        assert_eq!(io_error.kind(), io::ErrorKind::BrokenPipe);
        assert!(io_error.to_string().contains("peer reset"));

        let decode_error = framing_error_to_io(protocol::FramingError::Oversized {
            claimed: 32,
            max: 16,
        });
        assert_eq!(decode_error.kind(), io::ErrorKind::InvalidData);
        assert!(
            decode_error
                .to_string()
                .contains("frame size 32 exceeds maximum 16")
        );

        let surface_error = framing_error_to_io(protocol::FramingError::SurfaceDecode(
            "baseline mismatch".into(),
        ));
        assert_eq!(surface_error.kind(), io::ErrorKind::InvalidData);
        assert!(matches!(
            surface_error.get_ref().and_then(|error| error.downcast_ref::<protocol::FramingError>()),
            Some(protocol::FramingError::SurfaceDecode(message)) if message == "baseline mismatch"
        ));
    }

    #[test]
    fn upload_cancellation_preserves_pending_endpoint_download() {
        let scratch = crate::test_support::ScratchDir::new("cancel");
        let path = scratch.join("s.sock");
        let listener = crate::ipc::bind_local_listener(&path).expect("test precondition");
        let client = crate::ipc::connect_local_stream(&path).expect("test precondition");
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
        let cancel = crate::remote::bridge_upload_cancellation_for_test(
            bridge.try_clone().expect("test precondition"),
            ForwardedInput(forwarded_tx),
        );
        let message = ClientMessage::ClientShellFocus { focused: false };
        let mut expected = Vec::new();
        protocol::write_message(&mut expected, &message).expect("test precondition");
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
                protocol::read_message(&mut bridge, protocol::MAX_FRAME_SIZE)
                    .expect("test precondition");
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
