use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::client::{ApiClient, ApiClientDeadlineError, ApiClientError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub version: Option<String>,
    pub build_id: String,
    /// The server process's boot identity: what a conditional stop names to
    /// stop this instance and no other.
    pub boot_id: String,
    /// The server has begun stopping and will not accept a new client.
    pub stopping: bool,
    /// The server has bound its socket but is still restoring panes, and does
    /// not accept a TUI connection yet.
    pub starting: bool,
}

pub(crate) fn read_runtime_status_at(
    socket_path: &Path,
    timeout: Duration,
) -> io::Result<Option<RuntimeStatus>> {
    // Absence is "no status"; a stat that fails otherwise (EACCES, ELOOP) is
    // an error, not a missing server.
    let present = socket_path.try_exists().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not check server socket {}: {error}",
                socket_path.display()
            ),
        )
    })?;
    if !present {
        return Ok(None);
    }

    // clock-io-ok: one deadline bounds the real connect, write and status read.
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "status timeout is too large")
    })?;
    match read_runtime_status_until(socket_path, deadline) {
        Ok(status) => Ok(Some(status)),
        Err(error) if status_probe_has_no_answer(&error) => Ok(None),
        Err(
            ApiClientDeadlineError::Connect(error)
            | ApiClientDeadlineError::Request(ApiClientError::Io(error)),
        ) => Err(error),
        Err(ApiClientDeadlineError::Request(error)) => Err(io::Error::other(error)),
    }
}

/// Launch and conditional stop read the same identity and classify a lost
/// status answer identically. A missing status answer does not prove absence;
/// callers must still check whether the socket is live before launching.
pub(crate) fn read_runtime_status_until(
    socket_path: &Path,
    deadline: Instant,
) -> Result<RuntimeStatus, ApiClientDeadlineError> {
    ApiClient::for_socket(socket_path).status_until(deadline)
}

/// A transport close, refusal or timeout means the socket gave no status
/// answer. Decoded API failures are not that: they are errors, never evidence
/// that a server went away.
pub(crate) fn status_probe_has_no_answer(error: &ApiClientDeadlineError) -> bool {
    let no_answer_kind = |kind| {
        matches!(
            kind,
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::NotFound
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::NotConnected
                | io::ErrorKind::TimedOut
                | io::ErrorKind::WouldBlock
        )
    };
    match error {
        ApiClientDeadlineError::Connect(error)
        | ApiClientDeadlineError::Request(ApiClientError::Io(error)) => {
            no_answer_kind(error.kind())
        }
        ApiClientDeadlineError::Request(ApiClientError::EmptyResponse) => true,
        ApiClientDeadlineError::Request(
            ApiClientError::Json(_)
            | ApiClientError::ErrorResponse(_)
            | ApiClientError::UnexpectedResult(_),
        ) => false,
    }
}

/// Presence of the server socket and its readiness for TUI connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerPresence {
    /// No live listener at the socket, including one that went away while its
    /// status answer was awaited.
    Gone,
    /// Live, answering `starting`: still restoring panes.
    Starting,
    /// Live and accepting TUI connections.
    Running(RuntimeStatus),
    /// Live, answering `stopping` (which wins over `starting`).
    Stopping,
    /// Live before and after a status request that got no answer.
    Unresponsive,
}

/// Probes liveness, then asks for status, then probes liveness again when the
/// status request got no answer, so a server that finished shutting down
/// between the two reads as gone rather than unresponsive. A liveness probe
/// that cannot decide is the error.
pub fn read_server_presence_at(socket: &Path, timeout: Duration) -> io::Result<ServerPresence> {
    let live = || {
        shepr_platform::ipc::socket_is_live(socket).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot tell whether a shepr server listens at {}: {error}",
                    socket.display()
                ),
            )
        })
    };
    if !live()? {
        return Ok(ServerPresence::Gone);
    }
    let status = read_runtime_status_at(socket, timeout).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "the shepr server at {} did not give a usable status answer: {error}",
                socket.display()
            ),
        )
    })?;
    match status {
        Some(status) if status.stopping => Ok(ServerPresence::Stopping),
        Some(status) if status.starting => Ok(ServerPresence::Starting),
        Some(status) => Ok(ServerPresence::Running(status)),
        None if !live()? => Ok(ServerPresence::Gone),
        None => Ok(ServerPresence::Unresponsive),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader};

    fn pong_presence(stopping: bool, starting: bool) -> ServerPresence {
        use std::io::Write;
        let scratch = shepr_test_support::ScratchDir::new("presence-pong");
        let socket = scratch.join("server.sock");
        let listener = shepr_platform::ipc::bind_private_local_listener(&socket).expect("bind");
        let server = std::thread::spawn(move || {
            loop {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut line = String::new();
                BufReader::new(stream.try_clone().expect("clone"))
                    .read_line(&mut line)
                    .expect("request");
                if line.is_empty() {
                    continue;
                }
                let response = serde_json::json!({"id":"api-client:status","result":{"type":"pong","version":"0.1.0","build_id":"0123456789abcdef","boot_id":"test","stopping":stopping,"starting":starting}});
                writeln!(stream, "{response}").expect("pong");
                break;
            }
        });
        let presence = read_server_presence_at(&socket, Duration::from_secs(1)).expect("presence");
        server.join().expect("server");
        presence
    }

    #[test]
    fn a_dead_socket_is_gone() {
        let scratch = shepr_test_support::ScratchDir::new("presence-dead");
        let socket = scratch.join("server.sock");
        assert_eq!(
            read_server_presence_at(&socket, Duration::from_millis(30)).expect("absent"),
            ServerPresence::Gone
        );
        drop(shepr_platform::ipc::bind_private_local_listener(&socket).expect("bind stale"));
        assert_eq!(
            read_server_presence_at(&socket, Duration::from_millis(30)).expect("stale"),
            ServerPresence::Gone
        );
    }

    #[test]
    fn a_starting_pong_is_starting() {
        assert_eq!(pong_presence(false, true), ServerPresence::Starting);
    }

    #[test]
    fn stopping_wins_over_starting() {
        assert_eq!(pong_presence(true, true), ServerPresence::Stopping);
    }

    #[test]
    fn a_silent_listener_is_unresponsive() {
        let scratch = shepr_test_support::ScratchDir::new("presence-silent");
        let socket = scratch.join("server.sock");
        let _listener = shepr_platform::ipc::bind_private_local_listener(&socket).expect("bind");
        assert_eq!(
            read_server_presence_at(&socket, Duration::from_millis(30)).expect("presence"),
            ServerPresence::Unresponsive
        );
    }

    #[test]
    fn a_listener_that_vanishes_before_answering_is_gone() {
        let scratch = shepr_test_support::ScratchDir::new("presence-vanished");
        let socket = scratch.join("server.sock");
        let listener = shepr_platform::ipc::bind_private_local_listener(&socket).expect("bind");
        let path = socket.clone();
        let server = std::thread::spawn(move || {
            loop {
                let (stream, _) = listener.accept().expect("accept");
                let mut line = String::new();
                let mut reader = BufReader::new(stream);
                reader.read_line(&mut line).expect("request");
                if line.is_empty() {
                    continue;
                }
                drop(listener);
                std::fs::remove_file(path).expect("remove socket before closing request");
                break;
            }
        });
        assert_eq!(
            read_server_presence_at(&socket, Duration::from_secs(1)).expect("presence"),
            ServerPresence::Gone
        );
        server.join().expect("server");
    }

    #[test]
    fn launch_and_stop_share_status_transport_failure_classification() {
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::NotFound,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::NotConnected,
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
        ] {
            assert!(status_probe_has_no_answer(
                &ApiClientDeadlineError::Connect(io::Error::from(kind),)
            ));
            assert!(status_probe_has_no_answer(
                &ApiClientDeadlineError::Request(ApiClientError::Io(io::Error::from(kind)),)
            ));
        }
        assert!(!status_probe_has_no_answer(
            &ApiClientDeadlineError::Connect(io::Error::from(io::ErrorKind::PermissionDenied),)
        ));
        assert!(!status_probe_has_no_answer(
            &ApiClientDeadlineError::Request(ApiClientError::UnexpectedResult(
                "invalid status result".into()
            ),)
        ));
    }

    #[test]
    fn stalled_server_reports_no_status_instead_of_an_error() {
        let scratch = shepr_test_support::ScratchDir::new("status");
        let path = scratch.join("stalled.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("test precondition").0;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("test precondition");
            // Hold the connection open without answering.
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the test releases the stalled connection");
        });

        let status = read_runtime_status_at(&path, Duration::from_millis(100));
        release_tx
            .send(())
            .expect("the stalled server is still holding the connection");
        server.join().expect("test precondition");
        assert!(
            matches!(status, Ok(None)),
            "a stalled server must read as no status: {status:?}"
        );
    }

    #[test]
    fn server_that_closes_without_a_status_line_reports_no_status() {
        let scratch = shepr_test_support::ScratchDir::new("status-empty-response");
        let path = scratch.join("empty.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("test precondition").0;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("read the status request");
            assert!(!line.is_empty(), "the client sent a status request");
        });

        let status = read_runtime_status_at(&path, Duration::from_millis(100));
        server.join().expect("test precondition");
        assert!(
            matches!(status, Ok(None)),
            "a close without a response must read as no status: {status:?}"
        );
    }
}
