use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use tracing::info;

use shepr_platform::ipc::LocalStream;
use shepr_protocol::endpoint::EndpointClientHello;
use shepr_protocol::{ClientMessage, ServerMessage};

use super::{ClientError, shell};
use crate::limits::Deadline;
pub(super) use crate::limits::{LOCAL_HANDSHAKE_READ_TIMEOUT, REMOTE_HANDSHAKE_READ_TIMEOUT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientProcessRole {
    Local,
    Remote {
        keybindings: shell::ClientShellKeybindingSource,
    },
}

/// Where this client's keybindings come from. "server" imports the endpoint's; "local"
/// keeps this client's own; unset (or empty) is a local client. Any other value refuses
/// startup rather than guessing.
impl ClientProcessRole {
    pub(super) fn from_env() -> Result<Self, String> {
        match shepr_remote::RemoteKeybindings::from_env()? {
            Some(shepr_remote::RemoteKeybindings::Server) => Ok(Self::Remote {
                keybindings: shell::ClientShellKeybindingSource::Endpoint,
            }),
            Some(shepr_remote::RemoteKeybindings::Local) => Ok(Self::Remote {
                keybindings: shell::ClientShellKeybindingSource::RemoteLocal,
            }),
            None => Ok(Self::Local),
        }
    }

    pub(super) fn keybinding_source(self) -> shell::ClientShellKeybindingSource {
        match self {
            Self::Local => shell::ClientShellKeybindingSource::RemoteLocal,
            Self::Remote { keybindings } => keybindings,
        }
    }

    pub(super) fn handshake_read_timeout(self) -> Duration {
        match self {
            Self::Local => LOCAL_HANDSHAKE_READ_TIMEOUT,
            Self::Remote { .. } => REMOTE_HANDSHAKE_READ_TIMEOUT,
        }
    }
}

fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    context: &'static str,
) -> Result<(), ClientError> {
    stream.set_recv_timeout(timeout).map_err(|error| {
        ClientError::ConnectionFailed(std::io::Error::new(
            error.kind(),
            format!("{context}: {error}"),
        ))
    })
}

/// Maps a failed preamble exchange onto the client's error kinds: an early
/// close or read failure stays a transient connection problem, while a peer
/// that is not this build is a rejection the user has to act on.
fn preamble_error(error: shepr_protocol::preamble::PreambleError) -> ClientError {
    use shepr_protocol::preamble::PreambleError;
    match error {
        PreambleError::UnexpectedEof => {
            ClientError::from(shepr_protocol::FramingError::UnexpectedEof)
        }
        PreambleError::Io(error) => ClientError::from(shepr_protocol::FramingError::Io(error)),
        error @ (PreambleError::NotShepr | PreambleError::DifferentBuild(_)) => {
            ClientError::Preamble(error)
        }
    }
}

/// Performs the client→server handshake.
///
/// The connection opens with the raw build-identity preamble in both
/// directions (`shepr_protocol::preamble`), so a server of any other build is
/// reported as a mismatch before either side decodes a codec frame. Direct
/// terminal clients then send `TerminalHello`; client-owned shells send a JSON
/// endpoint hello. The hello variant selects terminal ANSI or semantic surface
/// delivery, so the welcome does not negotiate an encoding.
///
/// `deadline`, when given, caps the wait for the reply below the usual read timeout: the
/// saved-machine endpoint supervisor bounds each whole connection attempt by its
/// attempt budget.
/// The usual 60 s remote read budget applies to `shepr --remote`.
pub(super) fn do_handshake(
    stream: &mut LocalStream,
    role: ClientProcessRole,
    geometry: shepr_core::geometry::HostGeometry,
    shell_surface_size: Option<shepr_protocol::ClientSurfaceSize>,
    mouse_capture: bool,
    surface_active: bool,
    deadline: Option<std::time::Instant>,
) -> Result<(), ClientError> {
    let (cols, rows) = (geometry.cols(), geometry.rows());
    let (cell_width_px, cell_height_px, exact_cell_size) =
        super::terminal_geometry::bounded_cell_geometry(
            geometry.cell_width(),
            geometry.cell_height(),
            geometry.exact,
        );
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    let endpoint_shell = shell_surface_size.is_some();
    let hello = if let Some(surface_size) = shell_surface_size {
        let hello = EndpointClientHello {
            geometry: shepr_protocol::TerminalGeometry::new(
                surface_size.cols,
                surface_size.rows,
                cell_width_px,
                cell_height_px,
                exact_cell_size,
            ),
            mouse_capture,
            surface_active,
        };
        ClientMessage::EndpointHello(hello)
    } else {
        ClientMessage::TerminalHello {
            geometry: shepr_protocol::TerminalGeometry::new(
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                exact_cell_size,
            ),
        }
    };
    // Preamble and hello go out together; the server's preamble is read back
    // before its welcome, so a different build is named even if its welcome
    // would not decode.
    let mut opening = shepr_protocol::preamble::local_preamble().to_vec();
    opening.extend_from_slice(&shepr_protocol::encode_frame(&hello).map_err(hello_write_error)?);
    {
        use std::io::Write as _;
        stream
            .write_all(&opening)
            .and_then(|()| stream.flush())
            .map_err(|error| hello_write_error(shepr_protocol::FramingError::Io(error)))?;
    }

    let read_timeout = if endpoint_shell && !surface_active {
        REMOTE_HANDSHAKE_READ_TIMEOUT
    } else {
        role.handshake_read_timeout()
    };
    // One deadline for the preamble and the whole Welcome frame together, not a
    // per-read idle timeout.
    // clock-io-ok: bound the real handshake reads after writing the hello.
    let read_deadline = Deadline::after(std::time::Instant::now(), read_timeout);
    let read_deadline = deadline.map_or(read_deadline, |deadline| {
        read_deadline.min(Deadline::at(deadline))
    });
    let mut reader = shepr_platform::ipc::DeadlineReader::new(stream, read_deadline.instant());
    shepr_protocol::preamble::read_preamble(&mut reader).map_err(preamble_error)?;
    let welcome = shepr_protocol::read_message::<_, ServerMessage>(&mut reader)?;
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
        let ServerMessage::EndpointWelcome(welcome) = welcome else {
            return Err(ClientError::UnexpectedWelcome { endpoint: true });
        };
        if let Some(error) = welcome.error {
            return Err(ClientError::HandshakeRejected { error });
        }
        info!("endpoint handshake succeeded");
        return Ok(());
    }

    match welcome {
        ServerMessage::Welcome { error } => {
            if let Some(error) = error {
                return Err(ClientError::HandshakeRejected { error });
            }
            info!("terminal handshake succeeded");
            Ok(())
        }
        _ => Err(ClientError::UnexpectedWelcome { endpoint: false }),
    }
}

/// Keeps the socket error itself, kind included: the endpoint supervisor decides
/// between retrying and asking for attention by that kind, and a broken pipe or a
/// reset must stay a transient failure.
fn hello_write_error(error: shepr_protocol::FramingError) -> ClientError {
    match error {
        shepr_protocol::FramingError::Io(error) => ClientError::ConnectionFailed(error),
        // Encoding the hello failed: a local defect, not a connection problem.
        error => ClientError::Protocol(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::io;

    fn socket_pair(name: &str) -> (LocalStream, LocalStream) {
        let path = shepr_test_support::ScratchDir::new(name).join("s.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        (client, server)
    }

    fn handshake_against_shutdown(endpoint_shell: bool) -> ClientError {
        let (mut client, mut server) = socket_pair(if endpoint_shell {
            "shutdown-endpoint"
        } else {
            "shutdown-terminal"
        });
        let peer = std::thread::spawn(move || {
            shepr_protocol::preamble::write_preamble(&mut server).expect("test precondition");
            shepr_protocol::preamble::read_preamble(&mut server).expect("client preamble");
            let _hello: ClientMessage =
                shepr_protocol::read_message(&mut server).expect("test precondition");
            shepr_protocol::write_message(
                &mut server,
                &ServerMessage::ServerShutdown {
                    reason: Some(shepr_protocol::ShutdownReason::Message("restarting".into())),
                },
            )
            .expect("test precondition");
        });
        let surface =
            endpoint_shell.then_some(shepr_protocol::ClientSurfaceSize { cols: 80, rows: 24 });
        let error = do_handshake(
            &mut client,
            ClientProcessRole::Local,
            shepr_core::geometry::HostGeometry::new(80, 24, 8, 16, false),
            surface,
            false,
            true,
            None,
        )
        .expect_err("a shutdown notice is not a welcome");
        peer.join().expect("test precondition");
        error
    }

    #[test]
    fn shutdown_in_place_of_welcome_is_reported_as_a_shutdown() {
        for endpoint_shell in [true, false] {
            match handshake_against_shutdown(endpoint_shell) {
                ClientError::ServerShutdown { reason } => {
                    assert_eq!(
                        reason,
                        Some(shepr_protocol::ShutdownReason::Message("restarting".into()))
                    );
                }
                other => panic!("endpoint_shell={endpoint_shell}: {other}"),
            }
        }
    }

    /// Runs a handshake against a peer that answers with `server_opening` raw
    /// bytes and then hangs up.
    fn handshake_against_opening(name: &str, server_opening: Vec<u8>) -> ClientError {
        use std::io::Write as _;
        let (mut client, mut server) = socket_pair(name);
        let peer = std::thread::spawn(move || {
            server
                .write_all(&server_opening)
                .expect("test precondition: the client reads the opening");
            // Hold the connection until the client has read the opening. The client
            // writes its preamble before reading anything, so it is always there.
            let mut client_preamble = [0u8; shepr_protocol::preamble::PREAMBLE_LEN];
            std::io::Read::read_exact(&mut server, &mut client_preamble)
                .expect("test precondition: the client sends its preamble first");
            std::thread::sleep(Duration::from_millis(50));
        });
        let error = do_handshake(
            &mut client,
            ClientProcessRole::Local,
            shepr_core::geometry::HostGeometry::new(80, 24, 8, 16, false),
            Some(shepr_protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
            false,
            true,
            None,
        )
        .expect_err("the opening is not this build");
        peer.join().expect("test precondition");
        error
    }

    #[test]
    fn an_attempt_deadline_caps_a_silent_peer_below_the_read_timeout() {
        let (mut client, server) = socket_pair("deadline-silent-peer");
        // A saved-machine handshake (endpoint shell, surface off) would otherwise wait the
        // full remote read timeout for a peer that never answers.
        let started = std::time::Instant::now();
        let deadline = started + Duration::from_millis(200);
        let error = do_handshake(
            &mut client,
            ClientProcessRole::Local,
            shepr_core::geometry::HostGeometry::new(80, 24, 8, 16, false),
            Some(shepr_protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
            false,
            false,
            Some(deadline),
        )
        .expect_err("a silent peer never welcomes");
        let elapsed = started.elapsed();
        drop(server);
        let maximum_elapsed = deadline.saturating_duration_since(started) + Duration::from_secs(1);
        assert!(elapsed < maximum_elapsed, "deadline ignored: {elapsed:?}");
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
        let mut opening = shepr_protocol::preamble::local_preamble().to_vec();
        let id_start = shepr_protocol::preamble::PREAMBLE_MAGIC.len();
        let other_id = if opening[id_start] == b'0' {
            b'1'
        } else {
            b'0'
        };
        opening[id_start] = other_id;
        let peer_id = String::from_utf8_lossy(&opening[id_start..]).into_owned();
        opening.extend_from_slice(&[0xff; 16]);
        match handshake_against_opening("preamble-other-build", opening) {
            ClientError::Preamble(error) => {
                let error = error.to_string();
                assert!(error.contains(&format!("build {peer_id}")), "{error}");
                assert!(error.contains("different shepr build"), "{error}");
            }
            other => panic!("expected a build mismatch, got {other}"),
        }
    }

    #[test]
    fn peer_without_a_preamble_is_not_mistaken_for_a_closed_connection() {
        // A peer that answers straight with a codec frame.
        let mut opening = shepr_protocol::encode_frame(&ServerMessage::Welcome { error: None })
            .expect("test precondition");
        opening.resize(opening.len().max(shepr_protocol::preamble::PREAMBLE_LEN), 0);
        match handshake_against_opening("preamble-missing", opening) {
            ClientError::Preamble(error) => {
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
            match hello_write_error(shepr_protocol::FramingError::Io(io::Error::new(
                kind, "write",
            ))) {
                ClientError::ConnectionFailed(error) => assert_eq!(error.kind(), kind),
                other => panic!("{other}"),
            }
        }
        assert!(matches!(
            hello_write_error(shepr_protocol::FramingError::Oversized { claimed: 2, max: 1 }),
            ClientError::Protocol(_)
        ));
    }
}
