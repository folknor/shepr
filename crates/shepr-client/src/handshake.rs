use tracing::info;

use shepr_platform::ipc::{LocalStream, LocalStreamDeadlineReader};
use shepr_protocol::endpoint::{EndpointClientHello, EndpointServerWelcome};
use shepr_protocol::{ClientMessage, ServerMessage, TerminalGeometry};

use crate::errors::HandshakeError;
use crate::limits::Deadline;

/// Retains the preamble cause; the handshake failure classifier decides the
/// endpoint disposition.
fn preamble_error(error: shepr_protocol::preamble::PreambleError) -> HandshakeError {
    HandshakeError::Preamble(error)
}

/// Performs the client-to-server handshake for launch and supervised attaches,
/// the endpoint policy selecting its read timeout.
///
/// The connection opens with the raw build-identity preamble in both
/// directions (`shepr_protocol::preamble`), so a server of any other build is
/// reported as a mismatch before either side decodes a codec frame. The client
/// then sends its endpoint hello, and the welcome does not negotiate an encoding.
///
/// The welcome accepts or refuses the connection and carries no config.
/// A malformed welcome is a protocol failure of the handshake.
///
/// `deadline`, when given, caps the wait for the reply below the policy's read timeout.
pub(crate) fn do_handshake_for_endpoint(
    stream: &mut LocalStream,
    geometry: TerminalGeometry,
    mouse_capture: bool,
    surface_active: bool,
    endpoint_policy: crate::endpoint::EndpointPolicy,
    deadline: Option<std::time::Instant>,
) -> Result<(), HandshakeError> {
    stream
        .set_nonblocking(false)
        .map_err(HandshakeError::EndpointSetup)?;

    let hello = ClientMessage::EndpointHello(EndpointClientHello {
        geometry,
        mouse_capture,
        surface_active,
    });
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

    let read_timeout = endpoint_policy.handshake_read_timeout();
    // One deadline for the preamble and the whole Welcome frame together, not a
    // per-read idle timeout.
    // clock-io-ok: bound the real handshake reads after writing the hello.
    let read_deadline = Deadline::after(std::time::Instant::now(), read_timeout);
    let read_deadline = deadline.map_or(read_deadline, |deadline| {
        read_deadline.min(Deadline::at(deadline))
    });
    let mut reader = LocalStreamDeadlineReader::new(stream, read_deadline.instant());
    shepr_protocol::preamble::read_preamble(&mut reader).map_err(preamble_error)?;
    let welcome = shepr_protocol::read_message::<_, ServerMessage>(&mut reader)?;

    // A pre-welcome shutdown notice is transient if a peer sends one. The local server closes
    // without a welcome when stopping is observed during the handshake; if stopping races
    // after acceptance, it sends its notice after the welcome.
    let welcome = match welcome {
        ServerMessage::ServerShutdown { reason } => {
            return Err(HandshakeError::ServerShutdown { reason });
        }
        welcome => welcome,
    };

    let ServerMessage::EndpointWelcome(welcome) = welcome else {
        return Err(HandshakeError::UnexpectedWelcome);
    };
    match welcome {
        EndpointServerWelcome::Accepted => {
            info!("endpoint handshake succeeded");
            Ok(())
        }
        EndpointServerWelcome::Refused(error) => Err(HandshakeError::HandshakeRejected { error }),
    }
}

/// Keeps the IO cause at the write boundary and identifies hello encoding
/// failures as local setup.
fn hello_write_error(error: shepr_protocol::FramingError) -> HandshakeError {
    match error {
        shepr_protocol::FramingError::Io(error) => HandshakeError::ConnectionFailed(error),
        // Encoding the hello failed: a local defect, not a connection problem.
        error => HandshakeError::EndpointSetup(std::io::Error::other(error)),
    }
}

impl HandshakeError {
    /// Classifies a failed handshake for the endpoint status. The launch's first Local
    /// handshake runs before any supervisor attempt and goes through here too, so both report
    /// alike. `mismatch_guidance` is the Local endpoint's way out of a build mismatch;
    /// a configured machine has none here, its bridge reports its own.
    pub(crate) fn class(self, mismatch_guidance: Option<&str>) -> std::io::Error {
        use shepr_protocol::FramingError;
        use shepr_remote::EndpointFailure;
        let (kind, failure) = match self {
            HandshakeError::EndpointSetup(error) => (
                error.kind(),
                EndpointFailure::local_setup(error.to_string()),
            ),
            HandshakeError::ConnectionFailed(error)
            | HandshakeError::ConnectionLost(error)
            | HandshakeError::Protocol(FramingError::Io(error))
            | HandshakeError::Preamble(shepr_protocol::preamble::PreambleError::Io(error)) => {
                (error.kind(), EndpointFailure::from_error(&error))
            }
            HandshakeError::HandshakeRejected {
                error:
                    error @ (shepr_protocol::HandshakeRefusal::ConnectionLimit(_)
                    | shepr_protocol::HandshakeRefusal::ServerStarting),
            } => (
                std::io::ErrorKind::ConnectionAborted,
                EndpointFailure::retry(error.to_string()),
            ),
            HandshakeError::HandshakeRejected { error } => (
                std::io::ErrorKind::Unsupported,
                EndpointFailure::incompatible(error.to_string()),
            ),
            HandshakeError::Preamble(shepr_protocol::preamble::PreambleError::DifferentBuild(
                peer,
            )) if mismatch_guidance.is_some() => (
                std::io::ErrorKind::Unsupported,
                EndpointFailure::incompatible(local_build_mismatch(
                    &peer.build_id.to_string(),
                    mismatch_guidance.unwrap_or_default(),
                )),
            ),
            HandshakeError::Preamble(
                error @ shepr_protocol::preamble::PreambleError::DifferentBuild(_),
            ) => (
                std::io::ErrorKind::Unsupported,
                EndpointFailure::incompatible(error.to_string()),
            ),
            HandshakeError::Preamble(shepr_protocol::preamble::PreambleError::UnexpectedEof)
            | HandshakeError::Protocol(FramingError::UnexpectedEof) => (
                std::io::ErrorKind::UnexpectedEof,
                EndpointFailure::retry("connection closed before the endpoint finished connecting"),
            ),
            HandshakeError::Preamble(error) => (
                std::io::ErrorKind::InvalidData,
                EndpointFailure::incompatible(error.to_string()),
            ),
            HandshakeError::UnexpectedWelcome => (
                std::io::ErrorKind::InvalidData,
                EndpointFailure::incompatible(HandshakeError::UnexpectedWelcome.to_string()),
            ),
            HandshakeError::Protocol(error) => (
                std::io::ErrorKind::InvalidData,
                EndpointFailure::incompatible(error.to_string()),
            ),
            HandshakeError::ServerShutdown { reason } => (
                std::io::ErrorKind::ConnectionAborted,
                EndpointFailure::server_shutdown(reason),
            ),
        };
        std::io::Error::new(kind, failure.with_context(HANDSHAKE_CONTEXT))
    }
}

/// Applies Local's launch guidance and the SSH bridge's stderr to the same classified handshake
/// result for both initial and supervised attachment.
pub(crate) fn classify_handshake_error(
    error: crate::errors::HandshakeError,
    mismatch_guidance: Option<&str>,
    ssh_bridge: Option<&shepr_remote::MachineSshBridge>,
) -> std::io::Error {
    let error = error.class(mismatch_guidance);
    // An SSH endpoint that closes before Welcome usually means ssh itself failed. The bridge
    // holds the real stderr; prefer it so diagnosis and attention classification use its cause.
    if error.kind() == std::io::ErrorKind::UnexpectedEof
        && let Some(failure) = ssh_bridge
            .and_then(shepr_remote::MachineSshBridge::reported_failure)
            .map(|failure| {
                let kind = failure.kind();
                let diagnostic = shepr_remote::EndpointFailure::from_error(&failure)
                    .with_context(HANDSHAKE_CONTEXT);
                std::io::Error::new(kind, diagnostic)
            })
    {
        failure
    } else {
        error
    }
}

/// The shell status line and machine notice title supply the endpoint label;
/// keep only the failing phase here so it is not repeated in the displayed error.
const HANDSHAKE_CONTEXT: &str = "handshake failed";

/// The Local endpoint's build-mismatch diagnostic, on one line for the
/// endpoint status: both builds, then the guidance the launch
/// check prints when no configured machines keep the client running.
fn local_build_mismatch(running: &str, guidance: &str) -> String {
    format!(
        "build mismatch: the Local server runs shepr build {running}; this client is build {}. {}",
        shepr_protocol::BUILD_ID,
        guidance.replace('\n', " ")
    )
}

/// The handshake under Local's endpoint policy, for tests.
#[cfg(test)]
pub(super) fn do_handshake(
    stream: &mut LocalStream,
    geometry: TerminalGeometry,
    mouse_capture: bool,
    surface_active: bool,
    deadline: Option<std::time::Instant>,
) -> Result<(), HandshakeError> {
    do_handshake_for_endpoint(
        stream,
        geometry,
        mouse_capture,
        surface_active,
        crate::endpoint::EndpointPolicy::Local,
        deadline,
    )
}

#[cfg(test)]
pub(crate) fn handshake_error(
    error: HandshakeError,
    mismatch_guidance: Option<&str>,
) -> std::io::Error {
    error.class(mismatch_guidance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::time::Duration;

    fn test_geometry() -> TerminalGeometry {
        TerminalGeometry::new(80, 24, 8, 16, false)
    }

    fn socket_pair(name: &str) -> (LocalStream, LocalStream) {
        let path = shepr_test_support::ScratchDir::new(name).join("s.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition").0;
        (client, server)
    }

    fn handshake_against_shutdown() -> HandshakeError {
        let (mut client, mut server) = socket_pair("shutdown-endpoint");
        let peer = std::thread::spawn(move || {
            use std::io::Write as _;
            // The server reads the client's opening before it writes anything.
            shepr_protocol::preamble::read_preamble(&mut server).expect("client preamble");
            let _hello: ClientMessage =
                shepr_protocol::read_message(&mut server).expect("test precondition");
            server
                .write_all(&shepr_protocol::preamble::local_preamble())
                .expect("test precondition");
            shepr_protocol::write_message(
                &mut server,
                &ServerMessage::ServerShutdown {
                    reason: shepr_protocol::ShutdownReason::Stopping,
                },
            )
            .expect("test precondition");
        });
        let error = do_handshake(&mut client, test_geometry(), false, true, None)
            .expect_err("a shutdown notice is not a welcome");
        peer.join().expect("test precondition");
        error
    }

    /// Runs a handshake against a peer that answers with the build preamble and
    /// then `welcome_frames`, raw.
    fn handshake_against_welcome(
        name: &str,
        welcome_frames: Vec<u8>,
    ) -> Result<(), HandshakeError> {
        use std::io::Write as _;
        let (mut client, mut server) = socket_pair(name);
        let peer = std::thread::spawn(move || {
            // The server reads the client's opening before it writes anything.
            shepr_protocol::preamble::read_preamble(&mut server).expect("client preamble");
            let _hello: ClientMessage =
                shepr_protocol::read_message(&mut server).expect("test precondition");
            server
                .write_all(&shepr_protocol::preamble::local_preamble())
                .expect("test precondition");
            server
                .write_all(&welcome_frames)
                .expect("test precondition: the client reads the welcome");
        });
        let result = do_handshake(&mut client, test_geometry(), false, true, None);
        assert!(
            peer.join().is_ok(),
            "test precondition: the peer thread finishes"
        );
        result
    }

    #[test]
    fn accepted_welcome_succeeds() {
        let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::accepted());
        let frames = shepr_protocol::encode_message(&welcome).expect("test precondition");
        handshake_against_welcome("accepted-welcome", frames).expect("accepted welcome");
    }

    #[test]
    fn handshake_timeout_follows_endpoint_policy_not_surface_activity() {
        assert_eq!(
            crate::endpoint::EndpointPolicy::Local.handshake_read_timeout(),
            crate::limits::LOCAL_HANDSHAKE_READ_TIMEOUT
        );
        assert_eq!(
            crate::endpoint::EndpointPolicy::Machine.handshake_read_timeout(),
            crate::limits::REMOTE_HANDSHAKE_READ_TIMEOUT
        );
    }

    #[test]
    fn truncated_and_garbage_welcomes_fail_the_handshake() {
        let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::accepted());
        let encoded = shepr_protocol::encode_message(&welcome).expect("test precondition");
        for (name, payload) in [
            ("truncated-welcome", vec![encoded[4]]),
            ("garbage-welcome", vec![encoded[4], 0xff, 0x7f]),
        ] {
            let mut frames = u32::try_from(payload.len())
                .expect("small payload")
                .to_le_bytes()
                .to_vec();
            frames.extend_from_slice(&payload);
            assert!(matches!(
                handshake_against_welcome(name, frames),
                Err(HandshakeError::Protocol(
                    shepr_protocol::FramingError::Codec(_)
                ))
            ));
        }
    }

    #[test]
    fn refused_welcome_is_a_handshake_rejection() {
        let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::refused(
            shepr_protocol::HandshakeRefusal::ExpectedHello,
        ));
        let frames = shepr_protocol::encode_message(&welcome).expect("test precondition");
        match handshake_against_welcome("welcome-refused", frames) {
            Err(HandshakeError::HandshakeRejected { error }) => {
                assert_eq!(error, shepr_protocol::HandshakeRefusal::ExpectedHello);
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_in_place_of_welcome_is_reported_as_a_shutdown() {
        match handshake_against_shutdown() {
            HandshakeError::ServerShutdown { reason } => {
                assert_eq!(reason, shepr_protocol::ShutdownReason::Stopping);
            }
            other => panic!("{other}"),
        }
    }

    /// Runs a handshake against a peer that answers with `server_opening` raw
    /// bytes and then hangs up.
    fn handshake_against_opening(name: &str, server_opening: Vec<u8>) -> HandshakeError {
        use std::io::Write as _;
        let (mut client, mut server) = socket_pair(name);
        let peer = std::thread::spawn(move || {
            server
                .write_all(&server_opening)
                .expect("test precondition: the client reads the opening");
            // The client writes its preamble before reading anything, so it is
            // always there.
            let mut client_preamble = [0u8; shepr_protocol::preamble::PREAMBLE_LEN];
            std::io::Read::read_exact(&mut server, &mut client_preamble)
                .expect("test precondition: the client sends its preamble first");
            // Hold the connection, draining whatever else the client sends,
            // until the client hangs up after reading the opening. Every
            // opening here is at least a preamble long, so the client fails
            // on its bytes without waiting for this end to close.
            let mut rest = Vec::new();
            // The client may reset rather than close cleanly; either ends the hold.
            drop(std::io::Read::read_to_end(&mut server, &mut rest));
        });
        let error = do_handshake(&mut client, test_geometry(), false, true, None)
            .expect_err("the opening is not this build");
        // Hanging up is what releases the peer's hold.
        drop(client);
        peer.join().expect("test precondition");
        error
    }

    #[test]
    fn an_attempt_deadline_caps_a_silent_peer_below_the_read_timeout() {
        let (mut client, server) = socket_pair("deadline-silent-peer");
        // A machine's handshake (endpoint shell, surface off) would otherwise wait the
        // full remote read timeout for a peer that never answers.
        let started = std::time::Instant::now();
        let deadline = started + Duration::from_millis(200);
        let error = do_handshake(&mut client, test_geometry(), false, false, Some(deadline))
            .expect_err("a silent peer never welcomes");
        let elapsed = started.elapsed();
        drop(server);
        let maximum_elapsed = deadline.saturating_duration_since(started) + Duration::from_secs(1);
        assert!(elapsed < maximum_elapsed, "deadline ignored: {elapsed:?}");
        match error {
            HandshakeError::Preamble(shepr_protocol::preamble::PreambleError::Io(error)) => {
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
            HandshakeError::Preamble(error) => {
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
        let mut opening =
            shepr_protocol::encode_frame(&ServerMessage::HealthPong).expect("test precondition");
        opening.resize(opening.len().max(shepr_protocol::preamble::PREAMBLE_LEN), 0);
        match handshake_against_opening("preamble-missing", opening) {
            HandshakeError::Preamble(error) => {
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
                HandshakeError::ConnectionFailed(error) => assert_eq!(error.kind(), kind),
                other => panic!("{other}"),
            }
        }
        assert!(matches!(
            hello_write_error(shepr_protocol::FramingError::LimitExceeded(
                shepr_protocol::LimitExceeded::new(
                    shepr_protocol::Limit::new(shepr_protocol::LimitKind::MessageBytes, 1),
                    2,
                ),
            )),
            HandshakeError::EndpointSetup(_)
        ));
    }
}
