use std::io;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use tracing::info;

use crate::ipc::LocalStream;
use crate::protocol::endpoint::{
    ENDPOINT_HELLO_KIND, ENDPOINT_WELCOME_KIND, EndpointClientHello, EndpointServerWelcome,
};
use crate::protocol::{
    self, ClientMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION, RenderEncoding, ServerMessage,
};

use super::{ClientError, shell};

/// Time to wait for the server's complete Welcome reply during the handshake.
/// This is an overall deadline for the frame, not a per-read idle timeout.
///
/// A local client talks to an already-connected server, so 5s is plenty. The
/// remote bridge client (`shepr --remote`) sits behind a fresh per-attach ssh
/// connection whose cold-connect (TCP + key exchange + auth) happens inside this
/// window; on a high-latency link that easily exceeds 5s, so it gets a far
/// larger budget.
pub(super) const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn is_remote_client_process() -> bool {
    std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR).is_ok()
}

pub(super) fn client_shell_keybinding_source() -> shell::ClientShellKeybindingSource {
    match std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR)
        .ok()
        .as_deref()
    {
        Some("server") => shell::ClientShellKeybindingSource::Endpoint,
        Some(_) => shell::ClientShellKeybindingSource::RemoteLocal,
        None => shell::ClientShellKeybindingSource::Local,
    }
}

pub(super) fn handshake_read_timeout() -> Duration {
    if is_remote_client_process() {
        return REMOTE_HANDSHAKE_READ_TIMEOUT;
    }
    LOCAL_HANDSHAKE_READ_TIMEOUT
}

fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    _context: &'static str,
) -> Result<(), ClientError> {
    stream
        .set_recv_timeout(timeout)
        .map_err(ClientError::ConnectionFailed)
}

#[derive(Debug)]
pub(super) struct HandshakeResult {
    pub(super) encoding: RenderEncoding,
    pub(super) endpoint_methods: Option<Vec<String>>,
    pub(super) endpoint_capabilities: Option<Vec<String>>,
}

/// Performs the client→server handshake.
///
/// Direct terminal clients send `TerminalHello`; client-owned shells send a JSON
/// endpoint hello. Both carry `PROTOCOL_VERSION` and are rejected on mismatch.
pub(super) fn do_handshake(
    stream: &mut LocalStream,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    exact_cell_size: bool,
    shell_surface_size: Option<crate::protocol::ClientSurfaceSize>,
    endpoint_keybindings: bool,
    mouse_capture: bool,
    surface_active: bool,
) -> Result<HandshakeResult, ClientError> {
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    let endpoint_shell = shell_surface_size.is_some();
    let hello = if let Some(surface_size) = shell_surface_size {
        let hello = EndpointClientHello {
            version: PROTOCOL_VERSION,
            cell_width_px,
            cell_height_px,
            surface_size,
            pixel_mouse: exact_cell_size,
            endpoint_keybindings,
            mouse_capture,
            surface_active,
            surface_reuse: true,
            surface_delta: true,
        };
        ClientMessage::EndpointControl {
            kind: ENDPOINT_HELLO_KIND.into(),
            data: serde_json::to_string(&hello).map_err(|error| {
                ClientError::ConnectionFailed(io::Error::new(io::ErrorKind::InvalidData, error))
            })?,
        }
    } else {
        ClientMessage::TerminalHello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse: exact_cell_size,
        }
    };
    protocol::write_message(stream, &hello).map_err(hello_write_error)?;

    let read_timeout = if endpoint_shell && !surface_active {
        REMOTE_HANDSHAKE_READ_TIMEOUT
    } else {
        handshake_read_timeout()
    };
    // One deadline for the whole Welcome frame, not a per-read idle timeout.
    let welcome = protocol::read_message::<_, ServerMessage>(
        &mut crate::ipc::DeadlineReader::new(stream, std::time::Instant::now() + read_timeout),
        MAX_FRAME_SIZE,
    )?;
    set_handshake_recv_timeout(
        stream,
        None,
        "failed to clear client handshake read timeout",
    )?;

    // A server that is going down answers the hello with its shutdown notice. That is
    // a transient condition to report as such, not a malformed welcome.
    let welcome = match welcome {
        ServerMessage::ServerShutdown { reason } => {
            return Err(ClientError::ServerShutdown { reason });
        }
        welcome => welcome,
    };

    if endpoint_shell {
        let ServerMessage::EndpointControl { kind, data } = welcome else {
            return Err(ClientError::Protocol(protocol::FramingError::Io(
                io::Error::new(io::ErrorKind::InvalidData, "expected endpoint welcome"),
            )));
        };
        if kind != ENDPOINT_WELCOME_KIND {
            return Err(ClientError::Protocol(protocol::FramingError::Io(
                io::Error::new(io::ErrorKind::InvalidData, "expected endpoint welcome"),
            )));
        }
        let welcome: EndpointServerWelcome = serde_json::from_str(&data).map_err(|error| {
            ClientError::Protocol(protocol::FramingError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid endpoint welcome: {error}"),
            )))
        })?;
        if let Some(error) = welcome.error {
            return Err(ClientError::HandshakeRejected {
                version: welcome.version,
                error: error.message,
            });
        }
        if let Err(error) = protocol::check_client_version(welcome.version) {
            return Err(ClientError::HandshakeRejected {
                version: welcome.version,
                error,
            });
        }
        info!(version = welcome.version, "endpoint handshake succeeded");
        return Ok(HandshakeResult {
            encoding: RenderEncoding::SemanticFrame,
            endpoint_methods: Some(welcome.methods),
            endpoint_capabilities: Some(welcome.capabilities),
        });
    }

    match welcome {
        ServerMessage::Welcome {
            version,
            encoding,
            error,
        } => {
            if let Some(error) = error {
                return Err(ClientError::HandshakeRejected { version, error });
            }
            info!(version, ?encoding, "handshake succeeded");
            Ok(HandshakeResult {
                encoding,
                endpoint_methods: None,
                endpoint_capabilities: None,
            })
        }
        _ => Err(ClientError::Protocol(protocol::FramingError::Io(
            io::Error::new(io::ErrorKind::InvalidData, "expected Welcome message"),
        ))),
    }
}

/// Keeps the socket error itself, kind included: the endpoint supervisor decides
/// between retrying and asking for attention by that kind, and a broken pipe or a
/// reset must stay a transient failure.
fn hello_write_error(error: protocol::FramingError) -> ClientError {
    match error {
        protocol::FramingError::Io(error) => ClientError::ConnectionFailed(error),
        // Encoding the hello failed: a local defect, not a connection problem.
        error => ClientError::Protocol(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;

    fn socket_pair(name: &str) -> (LocalStream, LocalStream, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "shepr-handshake-{name}-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = crate::ipc::bind_private_local_listener(&path).expect("test precondition");
        let client = crate::ipc::connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        (client, server, path)
    }

    fn handshake_against_shutdown(endpoint_shell: bool) -> ClientError {
        let (mut client, mut server, path) = socket_pair(if endpoint_shell {
            "shutdown-endpoint"
        } else {
            "shutdown-terminal"
        });
        let peer = std::thread::spawn(move || {
            let _hello: ClientMessage =
                protocol::read_message(&mut server, MAX_FRAME_SIZE).expect("test precondition");
            protocol::write_message(
                &mut server,
                &ServerMessage::ServerShutdown {
                    reason: Some("restarting".into()),
                },
            )
            .expect("test precondition");
        });
        let surface =
            endpoint_shell.then_some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 });
        let error = do_handshake(
            &mut client,
            80,
            24,
            8,
            16,
            false,
            surface,
            false,
            false,
            true,
        )
        .expect_err("a shutdown notice is not a welcome");
        peer.join().expect("test precondition");
        let _ = std::fs::remove_file(path);
        error
    }

    #[test]
    fn shutdown_in_place_of_welcome_is_reported_as_a_shutdown() {
        for endpoint_shell in [true, false] {
            match handshake_against_shutdown(endpoint_shell) {
                ClientError::ServerShutdown { reason } => {
                    assert_eq!(reason.as_deref(), Some("restarting"));
                }
                other => panic!("endpoint_shell={endpoint_shell}: {other}"),
            }
        }
    }

    #[test]
    fn hello_write_failures_keep_their_error_kind() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut,
        ] {
            match hello_write_error(protocol::FramingError::Io(io::Error::new(kind, "write"))) {
                ClientError::ConnectionFailed(error) => assert_eq!(error.kind(), kind),
                other => panic!("{other}"),
            }
        }
        assert!(matches!(
            hello_write_error(protocol::FramingError::Oversized { claimed: 2, max: 1 }),
            ClientError::Protocol(_)
        ));
    }
}
