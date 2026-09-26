use std::io;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use tracing::info;

use crate::ipc::LocalStream;
use crate::protocol::endpoint::{
    ENDPOINT_HELLO_KIND, ENDPOINT_WELCOME_KIND, EndpointClientHello, EndpointServerWelcome,
};
use crate::protocol::{self, ClientMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION, ServerMessage};

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

/// Where this client's keybindings come from. "server" imports the endpoint's; "local" or
/// no value keeps this client's own. Any other value refuses startup rather than guessing.
pub(super) fn client_shell_keybinding_source() -> Result<shell::ClientShellKeybindingSource, String>
{
    let var = crate::remote::REMOTE_KEYBINDINGS_ENV_VAR;
    match std::env::var(var) {
        Ok(value) if value == "server" => Ok(shell::ClientShellKeybindingSource::Endpoint),
        Ok(value) if value == "local" => Ok(shell::ClientShellKeybindingSource::RemoteLocal),
        Err(std::env::VarError::NotPresent) => Ok(shell::ClientShellKeybindingSource::RemoteLocal),
        Ok(value) => Err(format!("{var} must be 'local' or 'server', got {value:?}")),
        Err(std::env::VarError::NotUnicode(value)) => {
            Err(format!("{var} must be 'local' or 'server', got {value:?}"))
        }
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

/// Outcome of a successful handshake.
///
#[derive(Debug)]
pub(super) struct HandshakeResult;

/// Maps a failed preamble exchange onto the client's error kinds: an early
/// close or read failure stays a transient connection problem, while a peer
/// that is not this build is a rejection the user has to act on.
fn preamble_error(error: protocol::preamble::PreambleError) -> ClientError {
    use protocol::preamble::PreambleError;
    match error {
        PreambleError::UnexpectedEof => ClientError::from(protocol::FramingError::UnexpectedEof),
        PreambleError::Io(error) => ClientError::from(protocol::FramingError::Io(error)),
        error @ PreambleError::NotShepr => ClientError::Protocol(protocol::FramingError::Io(
            io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        )),
        PreambleError::DifferentBuild(peer) => {
            let version = peer.protocol_version;
            ClientError::HandshakeRejected {
                version,
                error: PreambleError::DifferentBuild(peer).to_string(),
            }
        }
    }
}

/// Performs the client→server handshake.
///
/// The connection opens with the raw build-identity preamble in both
/// directions (`protocol::preamble`), so a server of any other build is
/// reported as a mismatch before either side decodes a codec frame. Direct
/// terminal clients then send `TerminalHello`; client-owned shells send a JSON
/// endpoint hello. Both still carry `PROTOCOL_VERSION`.
///
/// `deadline`, when given, caps the wait for the reply below the usual read timeout: the
/// saved-machine endpoint supervisor bounds each whole connection attempt by its
/// attempt budget.
/// The usual 60 s remote read budget applies to `shepr --remote`.
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
    deadline: Option<std::time::Instant>,
) -> Result<HandshakeResult, ClientError> {
    let (cell_width_px, cell_height_px, exact_cell_size) =
        super::terminal_geometry::bounded_cell_geometry(
            cell_width_px,
            cell_height_px,
            exact_cell_size,
        );
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
    // Preamble and hello go out together; the server's preamble is read back
    // before its welcome, so a different build is named even if its welcome
    // would not decode.
    let mut opening = protocol::preamble::local_preamble().to_vec();
    opening.extend_from_slice(&protocol::encode_frame(&hello).map_err(hello_write_error)?);
    {
        use std::io::Write as _;
        stream
            .write_all(&opening)
            .and_then(|()| stream.flush())
            .map_err(|error| hello_write_error(protocol::FramingError::Io(error)))?;
    }

    let read_timeout = if endpoint_shell && !surface_active {
        REMOTE_HANDSHAKE_READ_TIMEOUT
    } else {
        handshake_read_timeout()
    };
    // One deadline for the preamble and the whole Welcome frame together, not a
    // per-read idle timeout.
    let read_deadline = std::time::Instant::now() + read_timeout;
    let read_deadline = deadline.map_or(read_deadline, |deadline| deadline.min(read_deadline));
    let mut reader = crate::ipc::DeadlineReader::new(stream, read_deadline);
    protocol::preamble::read_preamble(&mut reader).map_err(preamble_error)?;
    let welcome = protocol::read_message::<_, ServerMessage>(&mut reader, MAX_FRAME_SIZE)?;
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
        return Ok(HandshakeResult);
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
            Ok(HandshakeResult)
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
        // Kept until the test process exits; callers remove the socket.
        let path = crate::test_support::ScratchDir::new(name)
            .keep_until_exit()
            .join("s.sock");
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
            protocol::preamble::write_preamble(&mut server).expect("test precondition");
            protocol::preamble::read_preamble(&mut server).expect("client preamble");
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
            None,
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

    /// Runs a handshake against a peer that answers with `server_opening` raw
    /// bytes and then hangs up.
    fn handshake_against_opening(name: &str, server_opening: Vec<u8>) -> ClientError {
        use std::io::Write as _;
        let (mut client, mut server, path) = socket_pair(name);
        let peer = std::thread::spawn(move || {
            let _ = server.write_all(&server_opening);
            // Hold the connection until the client has read the opening.
            let mut client_preamble = [0u8; protocol::preamble::PREAMBLE_LEN];
            let _ = std::io::Read::read_exact(&mut server, &mut client_preamble);
            std::thread::sleep(Duration::from_millis(50));
        });
        let error = do_handshake(
            &mut client,
            80,
            24,
            8,
            16,
            false,
            Some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
            false,
            false,
            true,
            None,
        )
        .expect_err("the opening is not this build");
        peer.join().expect("test precondition");
        let _ = std::fs::remove_file(path);
        error
    }

    #[test]
    fn an_attempt_deadline_caps_a_silent_peer_below_the_read_timeout() {
        let (mut client, server, path) = socket_pair("deadline-silent-peer");
        // A saved-machine handshake (endpoint shell, surface off) would otherwise wait the
        // full remote read timeout for a peer that never answers.
        let started = std::time::Instant::now();
        let error = do_handshake(
            &mut client,
            80,
            24,
            8,
            16,
            false,
            Some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
            false,
            false,
            false,
            Some(started + Duration::from_millis(200)),
        )
        .expect_err("a silent peer never welcomes");
        let elapsed = started.elapsed();
        drop(server);
        let _ = std::fs::remove_file(path);
        assert!(
            elapsed < REMOTE_HANDSHAKE_READ_TIMEOUT / 4,
            "deadline ignored: {elapsed:?}"
        );
        match error {
            ClientError::ConnectionLost(error) => {
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            }
            other => panic!("expected a timeout, got {other}"),
        }
    }

    #[test]
    fn different_build_is_reported_from_the_preamble() {
        // A server of another build: its preamble names it, and nothing after
        // it (here: garbage) needs to decode for the mismatch to be reported.
        let mut opening = protocol::preamble::local_preamble().to_vec();
        let version_start = protocol::preamble::PREAMBLE_MAGIC.len();
        opening[version_start..version_start + 4]
            .copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
        let id_start = version_start + 4;
        let other_id = if opening[id_start] == b'0' {
            b'1'
        } else {
            b'0'
        };
        opening[id_start] = other_id;
        opening.extend_from_slice(&[0xff; 16]);
        match handshake_against_opening("preamble-other-build", opening) {
            ClientError::HandshakeRejected { version, error } => {
                assert_eq!(version, PROTOCOL_VERSION + 1);
                assert!(error.contains("different shepr build"), "{error}");
            }
            other => panic!("expected a build mismatch, got {other}"),
        }
    }

    #[test]
    fn peer_without_a_preamble_is_not_mistaken_for_a_closed_connection() {
        // A peer that answers straight with a codec frame.
        let mut opening = protocol::encode_frame(&ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            encoding: crate::protocol::RenderEncoding::SemanticFrame,
            error: None,
        })
        .expect("test precondition");
        opening.resize(opening.len().max(protocol::preamble::PREAMBLE_LEN), 0);
        match handshake_against_opening("preamble-missing", opening) {
            ClientError::Protocol(protocol::FramingError::Io(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(error.to_string().contains("preamble"), "{error}");
            }
            other => panic!("expected a missing-preamble error, got {other}"),
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
