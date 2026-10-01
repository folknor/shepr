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
}

pub fn read_runtime_status_at(
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
/// callers must still observe the endpoint lifetime before launching.
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

/// Presence of one two-socket server, including its status identity when it
/// answers. The lifetime contract lives in the platform layer; status supplies
/// the boot identity and stopping latch, never a second absence predicate.
pub type ServerPresence = shepr_platform::ipc::ServerPresence<RuntimeStatus>;

pub fn read_server_presence_at(
    client_socket: &Path,
    api_socket: &Path,
    timeout: Duration,
) -> io::Result<ServerPresence> {
    use shepr_platform::ipc::ServerLifetime;
    let endpoint_is_live = |path: &Path| {
        crate::server_stop::server_socket_is_live(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot tell whether a shepr server listens at {}: {error}",
                    path.display()
                ),
            )
        })
    };
    let client_live = endpoint_is_live(client_socket)?;
    if !client_live {
        return Ok(ServerLifetime::observe(
            endpoint_is_live(api_socket)?,
            false,
            None,
        ));
    }
    let status = read_runtime_status_at(api_socket, timeout).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "the shepr server at {} did not give a usable status answer: {error}",
                client_socket.display()
            ),
        )
    })?;
    match status {
        Some(status) => {
            let stopping = status.stopping;
            Ok(ServerLifetime::observe(
                true,
                true,
                Some((status, stopping)),
            ))
        }
        None => Ok(ServerLifetime::observe(
            endpoint_is_live(api_socket)?,
            true,
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader};

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
