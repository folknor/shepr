use shepr_protocol::BootId;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::client::{ApiClient, ApiClientDeadlineError, ApiClientError};

// Stopping a server only connects to its socket (to stop or probe it); it
// never binds one. Binding goes through
// `ipc::bind_private_local_listener` in the server and API, and the peer check
// on accept is theirs, so nothing here needs the staged bind or `SO_PEERCRED`.

use crate::limits::{
    STOP_LEASE_WAIT_TIMEOUT, STOP_STATUS_PROBE_TIMEOUT, STOP_WAIT_POLL, STOP_WAIT_TIMEOUT,
};

/// The exit status `shepr server stop` ends with when no server is running at
/// the address, so a caller that ran it over SSH can tell "the server already
/// exited" from any other failure without parsing stderr.
// limits-exempt: process exit status shared by the server stop command and its SSH caller.
const NO_SERVER_EXIT_CODE: i32 = 4;

/// The exit status `shepr server stop --expect-boot` ends with when a different
/// boot is found, either at the stop request or while the named boot shuts
/// down, so an SSH caller can identify a changed occupant without parsing stderr.
// limits-exempt: process exit status shared by the server stop command and its SSH caller.
const BOOT_MISMATCH_EXIT_CODE: i32 = 3;

/// A server-stop outcome that a caller can distinguish by process exit code.
/// The CLI uses this to encode the outcome; SSH callers use it to decode it.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerStopExit {
    NoServer = NO_SERVER_EXIT_CODE,
    BootMismatch = BOOT_MISMATCH_EXIT_CODE,
}

impl ServerStopExit {
    /// The process exit status for this stop outcome.
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// Decodes a process exit status emitted by `shepr server stop`.
    pub const fn from_code(code: i32) -> Option<Self> {
        match code {
            NO_SERVER_EXIT_CODE => Some(Self::NoServer),
            BOOT_MISMATCH_EXIT_CODE => Some(Self::BootMismatch),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum ServerStopError {
    NotRunning {
        label: String,
        path: PathBuf,
        source: io::Error,
    },
    Unreachable {
        label: String,
        path: PathBuf,
        source: io::Error,
    },
    TimedOut {
        label: String,
        timeout: Duration,
        socket: PathBuf,
    },
    /// The server no longer answers, or its socket disappeared, but a process
    /// still holds the data directory lease.
    LeaseHeld {
        label: String,
        timeout: Duration,
        path: PathBuf,
    },
    Io {
        context: String,
        source: io::Error,
    },
    Protocol(String),
    /// A conditional stop reached a server that is not the boot it named, so
    /// it was refused and the server keeps running. The occupant of the socket
    /// changed after it was observed (it stopped and another server started).
    BootMismatch {
        label: String,
        expected_boot_id: BootId,
        /// The server's own words, naming the boot it is.
        detail: String,
    },
    /// A conditional stop was accepted for one boot, but a different boot
    /// answered while the requested boot was shutting down.
    OccupantChanged {
        label: String,
        expected_boot_id: BootId,
        actual_boot_id: BootId,
    },
}

impl ServerStopError {
    /// The API error code the CLI reports this failure under.
    pub fn error_code(&self) -> crate::error::ApiErrorCode {
        match self {
            Self::BootMismatch { .. } | Self::OccupantChanged { .. } => {
                crate::error::ApiErrorCode::ServerBootMismatch
            }
            _ => crate::error::ApiErrorCode::ServerStopFailed,
        }
    }

    /// Whether a conditional stop found a different boot, either when the
    /// request arrived or while the requested boot was shutting down.
    pub fn is_boot_mismatch(&self) -> bool {
        matches!(
            self,
            Self::BootMismatch { .. } | Self::OccupantChanged { .. }
        )
    }

    /// Whether the stop found no server at the address; nothing was stopped.
    pub fn is_not_running(&self) -> bool {
        matches!(self, Self::NotRunning { .. })
    }
}

impl std::fmt::Display for ServerStopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(message) => f.write_str(message),
            Self::NotRunning {
                label,
                path,
                source,
            } => {
                write!(f, "{label} is not running at {}: {source}", path.display())
            }
            Self::Unreachable {
                label,
                path,
                source,
            } => {
                write!(
                    f,
                    "{label} cannot be reached at {}: {source}",
                    path.display()
                )
            }
            Self::TimedOut {
                label,
                timeout,
                socket,
            } => write!(
                f,
                "{label} did not stop within {}ms; the socket at {} is still reachable",
                timeout.as_millis(),
                socket.display()
            ),
            Self::LeaseHeld {
                label,
                timeout,
                path,
            } => write!(
                f,
                "the data directory lease at {} was still held {}ms after {label} stopped answering or its socket disappeared; another process may still be using it",
                path.display(),
                timeout.as_millis()
            ),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::BootMismatch {
                label,
                expected_boot_id,
                detail,
            } => write!(
                f,
                "{label} was not stopped: it is not the server instance that was expected (boot {expected_boot_id}); it may have been restarted since. {detail}"
            ),
            Self::OccupantChanged {
                label,
                expected_boot_id,
                actual_boot_id,
            } => write!(
                f,
                "{label} stopped answering as boot {expected_boot_id}, but boot {actual_boot_id} now answers; no stop was sent to the new occupant"
            ),
        }
    }
}

impl std::error::Error for ServerStopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotRunning { source, .. }
            | Self::Unreachable { source, .. }
            | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for ServerStopError {
    fn from(source: io::Error) -> Self {
        Self::Io {
            context: "server operation failed".into(),
            source,
        }
    }
}

/// Stops the server the resolved address names, whatever its build. This never
/// launches a server: with none listening it fails with
/// [`ServerStopError::NotRunning`].
///
/// With `expected_boot_id` (a boot identity read from that server's status) the
/// stop is conditional: the server checks the identity itself, in the same
/// request that stops it, and refuses with [`ServerStopError::BootMismatch`]
/// when it is a different boot. After accepting the request, this function
/// waits for that boot to stop answering, for the server socket to stop
/// accepting connections, and for the data-directory lease to be released. It
/// reports another boot that answers during shutdown as
/// [`ServerStopError::OccupantChanged`]. Nothing here reads the status before
/// sending the stop, because a separate read could not close that race. The
/// stop is the same for a remote server: the remote `shepr server stop
/// --expect-boot <id>` runs this function on its own host.
///
/// # Errors
///
/// When there is no server to stop, the stop request fails, the named server
/// does not stop answering in time, or a different boot answers.
pub fn stop_active_server(
    paths: &shepr_config::AppPaths,
    expected_boot_id: Option<&BootId>,
) -> Result<(), ServerStopError> {
    stop_active_server_with_timeout(paths, expected_boot_id, STOP_WAIT_TIMEOUT)
}

fn stop_active_server_with_timeout(
    paths: &shepr_config::AppPaths,
    expected_boot_id: Option<&BootId>,
    timeout: Duration,
) -> Result<(), ServerStopError> {
    let address = paths.server_address();
    let socket_path = address.socket().to_path_buf();
    stop_socket_with_timeout(
        &socket_path,
        Some((&paths.data_dir_lease_path(), STOP_LEASE_WAIT_TIMEOUT)),
        timeout,
        "server",
        expected_boot_id,
    )
}

/// Stops the server at `socket_path` and waits for the named boot to stop
/// answering and its socket to stop accepting connections when the request is
/// conditional, or for the socket to disappear otherwise. With
/// `lease` (the lease file and how long to wait for it), it also waits for the
/// data-directory lease to become available.
fn stop_socket_with_timeout(
    socket_path: &Path,
    lease: Option<(&Path, Duration)>,
    timeout: Duration,
    label: &str,
    expected_boot_id: Option<&BootId>,
) -> Result<(), ServerStopError> {
    // clock-io-ok: one deadline bounds the real stop request's socket reads
    // and the server process's exit, so it must share their real clock.
    let deadline = Instant::now() + timeout;
    let request = server_stop_request("cli:server:stop", expected_boot_id);
    send_stop_request(socket_path, &request, deadline, label, expected_boot_id)?;
    let stopped = if let Some(expected_boot_id) = expected_boot_id {
        match wait_until_boot_stops(socket_path, expected_boot_id, deadline, label)? {
            BootStopWait::Gone => true,
            BootStopWait::Changed(actual_boot_id) => {
                return Err(ServerStopError::OccupantChanged {
                    label: label.into(),
                    expected_boot_id: expected_boot_id.clone(),
                    actual_boot_id,
                });
            }
            BootStopWait::TimedOut => false,
        }
    } else {
        wait_until_stopped_until(socket_path, deadline).map_err(|source| ServerStopError::Io {
            context: format!("could not check whether {label} stopped"),
            source,
        })?
    };
    if !stopped {
        return Err(ServerStopError::TimedOut {
            label: label.into(),
            timeout,
            socket: socket_path.to_path_buf(),
        });
    }
    let mut socket_deadline = deadline;
    if let Some((lease_path, lease_timeout)) = lease {
        // clock-io-ok: the lease wait polls another process's lock.
        let lease_deadline = Instant::now() + lease_timeout;
        let released = if let Some(expected_boot_id) = expected_boot_id {
            match wait_for_lease_release_or_new_boot(
                lease_path,
                lease_deadline,
                socket_path,
                expected_boot_id,
                label,
            )? {
                LeaseWait::Released => true,
                LeaseWait::Held => false,
                LeaseWait::NewBoot(actual_boot_id) => {
                    return Err(ServerStopError::OccupantChanged {
                        label: label.into(),
                        expected_boot_id: expected_boot_id.clone(),
                        actual_boot_id,
                    });
                }
            }
        } else {
            wait_for_lease_release(lease_path, lease_deadline).map_err(|source| {
                ServerStopError::Io {
                    context: format!(
                        "could not check whether {label} released {}",
                        lease_path.display()
                    ),
                    source,
                }
            })?
        };
        if !released {
            return Err(ServerStopError::LeaseHeld {
                label: label.into(),
                timeout: lease_timeout,
                path: lease_path.into(),
            });
        }
        // Lease release may consume its separate budget before the socket
        // disappears, so retain the later deadline for that wait.
        socket_deadline = socket_deadline.max(lease_deadline);
    }
    if let Some(expected_boot_id) = expected_boot_id {
        match wait_until_socket_stopped_or_new_boot(
            socket_path,
            expected_boot_id,
            socket_deadline,
            label,
        )? {
            BootStopWait::Gone => {}
            BootStopWait::Changed(actual_boot_id) => {
                return Err(ServerStopError::OccupantChanged {
                    label: label.into(),
                    expected_boot_id: expected_boot_id.clone(),
                    actual_boot_id,
                });
            }
            BootStopWait::TimedOut => {
                return Err(ServerStopError::TimedOut {
                    label: label.into(),
                    timeout,
                    socket: socket_path.to_path_buf(),
                });
            }
        }
        // clock-io-ok: the final probe is one real socket request.
        let final_probe_deadline = Instant::now() + STOP_STATUS_PROBE_TIMEOUT;
        match probe_boot(socket_path, expected_boot_id, label, final_probe_deadline)? {
            BootProbe::Gone => {}
            BootProbe::Changed(actual_boot_id) => {
                return Err(ServerStopError::OccupantChanged {
                    label: label.into(),
                    expected_boot_id: expected_boot_id.clone(),
                    actual_boot_id,
                });
            }
            BootProbe::Expected => {
                match wait_until_boot_stops(socket_path, expected_boot_id, deadline, label)? {
                    BootStopWait::Gone => {}
                    BootStopWait::Changed(actual_boot_id) => {
                        return Err(ServerStopError::OccupantChanged {
                            label: label.into(),
                            expected_boot_id: expected_boot_id.clone(),
                            actual_boot_id,
                        });
                    }
                    BootStopWait::TimedOut => {
                        return Err(ServerStopError::TimedOut {
                            label: label.into(),
                            timeout,
                            socket: socket_path.to_path_buf(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Waits until `deadline` for the data directory lease at `lease_path` to be
/// free, by taking and dropping its lock. A directory with no lease file has
/// never been served, so there is nothing to wait for (and nothing is created).
fn wait_for_lease_release(lease_path: &Path, deadline: Instant) -> io::Result<bool> {
    if !lease_path.try_exists()? {
        return Ok(true);
    }
    loop {
        if data_dir_lease_is_free(lease_path)? {
            return Ok(true);
        }
        // clock-io-ok: polls another process's lock while it shuts down.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(STOP_WAIT_POLL.min(remaining));
    }
}

// Flock has no nonintrusive ownership query. This brief exclusive probe can
// race a server start before its socket bind; the client launcher retries a
// daemon that loses the probe. Keeping it lets a stop detect a successor that
// acquired the lease before the socket appeared.
fn data_dir_lease_is_free(lease_path: &Path) -> io::Result<bool> {
    shepr_platform::DataDirectoryLease::probe(lease_path)
}

// The boot probe says which process answers; the socket check says whether
// anything listens. Both must hold for a conditional stop to be complete.
enum BootProbe {
    Gone,
    Expected,
    Changed(BootId),
}

enum BootStopWait {
    Gone,
    Changed(BootId),
    TimedOut,
}

enum LeaseWait {
    Released,
    Held,
    NewBoot(BootId),
}

fn probe_boot(
    socket_path: &Path,
    expected_boot_id: &BootId,
    label: &str,
    deadline: Instant,
) -> Result<BootProbe, ServerStopError> {
    // clock-io-ok: bounds one real status request on the socket.
    let probe_deadline = (Instant::now() + STOP_STATUS_PROBE_TIMEOUT).min(deadline);
    match crate::status::read_runtime_status_until(socket_path, probe_deadline) {
        Ok(status) if &status.boot_id == expected_boot_id => Ok(BootProbe::Expected),
        Ok(status) => Ok(BootProbe::Changed(status.boot_id)),
        Err(error) if crate::status::status_probe_has_no_answer(&error) => Ok(BootProbe::Gone),
        Err(error) => Err(status_probe_error(error, label)),
    }
}

fn wait_until_boot_stops(
    socket_path: &Path,
    expected_boot_id: &BootId,
    deadline: Instant,
    label: &str,
) -> Result<BootStopWait, ServerStopError> {
    loop {
        // clock-io-ok: the wait bounds real status probes of a shutting-down server.
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Ok(BootStopWait::TimedOut);
        }
        match probe_boot(socket_path, expected_boot_id, label, deadline)? {
            BootProbe::Gone => return Ok(BootStopWait::Gone),
            BootProbe::Changed(actual_boot_id) => {
                return Ok(BootStopWait::Changed(actual_boot_id));
            }
            BootProbe::Expected => {}
        }
        // clock-io-ok: polls the server status while its real shutdown proceeds.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(BootStopWait::TimedOut);
        }
        std::thread::sleep(STOP_WAIT_POLL.min(remaining));
    }
}

/// Waits for the socket to go while checking that a successor has not taken
/// the address during lease retirement or socket removal.
fn wait_until_socket_stopped_or_new_boot(
    socket_path: &Path,
    expected_boot_id: &BootId,
    deadline: Instant,
    label: &str,
) -> Result<BootStopWait, ServerStopError> {
    loop {
        match probe_boot(socket_path, expected_boot_id, label, deadline)? {
            BootProbe::Gone | BootProbe::Expected => {}
            BootProbe::Changed(actual_boot_id) => {
                return Ok(BootStopWait::Changed(actual_boot_id));
            }
        }
        let socket_stopped =
            server_socket_is_stopped(socket_path).map_err(|source| ServerStopError::Io {
                context: format!("could not check whether {label} stopped"),
                source,
            })?;
        if socket_stopped {
            return Ok(BootStopWait::Gone);
        }
        // clock-io-ok: polls the server socket while the named process exits.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(BootStopWait::TimedOut);
        }
        std::thread::sleep(STOP_WAIT_POLL.min(remaining));
    }
}

fn wait_for_lease_release_or_new_boot(
    lease_path: &Path,
    deadline: Instant,
    socket_path: &Path,
    expected_boot_id: &BootId,
    label: &str,
) -> Result<LeaseWait, ServerStopError> {
    if !lease_path
        .try_exists()
        .map_err(|source| ServerStopError::Io {
            context: format!(
                "could not check whether {label} released {}",
                lease_path.display()
            ),
            source,
        })?
    {
        return Ok(LeaseWait::Released);
    }
    loop {
        let free = data_dir_lease_is_free(lease_path).map_err(|source| ServerStopError::Io {
            context: format!(
                "could not check whether {label} released {}",
                lease_path.display()
            ),
            source,
        })?;
        if free {
            return Ok(LeaseWait::Released);
        }
        // clock-io-ok: polls another process's lock while it shuts down.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(LeaseWait::Held);
        }
        match probe_boot(socket_path, expected_boot_id, label, deadline)? {
            BootProbe::Changed(actual_boot_id) => {
                return Ok(LeaseWait::NewBoot(actual_boot_id));
            }
            BootProbe::Gone | BootProbe::Expected => {}
        }
        std::thread::sleep(STOP_WAIT_POLL.min(remaining));
    }
}

fn status_probe_error(error: ApiClientDeadlineError, label: &str) -> ServerStopError {
    match error {
        ApiClientDeadlineError::Connect(source)
        | ApiClientDeadlineError::Request(ApiClientError::Io(source)) => ServerStopError::Io {
            context: format!("could not check whether {label} stopped"),
            source,
        },
        ApiClientDeadlineError::Request(error) => ServerStopError::Protocol(error.to_string()),
    }
}

fn send_stop_request(
    socket_path: &Path,
    request: &crate::schema::Request,
    deadline: Instant,
    label: &str,
    expected_boot_id: Option<&BootId>,
) -> Result<(), ServerStopError> {
    // clock-io-ok: the deadline is the one the real socket reader below keeps.
    if deadline.saturating_duration_since(Instant::now()).is_zero() {
        return Err(ServerStopError::Io {
            context: format!("could not send stop request to {label}"),
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                "stop deadline expired before the request was sent",
            ),
        });
    }
    let client = ApiClient::for_socket(socket_path);
    match client.request_until(request, deadline) {
        Ok(response) => match response.result {
            crate::schema::ResponseResult::Ok {} => Ok(()),
            _ => Err(ServerStopError::Protocol(
                "unexpected stop result from server".into(),
            )),
        },
        Err(ApiClientDeadlineError::Connect(error)) => {
            Err(stop_socket_io_error(socket_path, label, error))
        }
        // A connection closed without an answer is ambiguous. The server may
        // have begun stopping before it wrote one, or it may have dropped the
        // connection unanswered, which the listener and the API connection
        // handler do in several failure paths (a refused peer, a worker that
        // cannot spawn, a saturated overflow queue, a request line it cannot
        // read). The client cannot tell which, so this is a decision to count
        // the request as accepted and let the wait that follows decide. A stop
        // that never arrived then ends in the wait's `TimedOut`, whose
        // wording reads as though the stop was delivered; that ambiguity is
        // accepted rather than resolved.
        Err(ApiClientDeadlineError::Request(ApiClientError::EmptyResponse)) => Ok(()),
        Err(ApiClientDeadlineError::Request(ApiClientError::Io(error)))
            if stop_request_error_allows_wait(&error) =>
        {
            Ok(())
        }
        Err(ApiClientDeadlineError::Request(ApiClientError::Io(error))) => Err(error.into()),
        Err(ApiClientDeadlineError::Request(ApiClientError::Json(error))) => {
            Err(ServerStopError::Protocol(error.to_string()))
        }
        Err(ApiClientDeadlineError::Request(ApiClientError::ErrorResponse(response))) => {
            match expected_boot_id {
                Some(expected)
                    if response.error.code == crate::error::ApiErrorCode::ServerBootMismatch =>
                {
                    Err(ServerStopError::BootMismatch {
                        label: label.into(),
                        expected_boot_id: expected.clone(),
                        detail: response.error.message,
                    })
                }
                _ => Err(ServerStopError::Protocol(response.error.message)),
            }
        }
        Err(ApiClientDeadlineError::Request(ApiClientError::UnexpectedResult(result))) => {
            Err(ServerStopError::Protocol(result))
        }
    }
}

fn stop_socket_io_error(socket_path: &Path, label: &str, error: io::Error) -> ServerStopError {
    match shepr_platform::ipc::socket_is_live(socket_path) {
        Ok(false) => ServerStopError::NotRunning {
            label: label.into(),
            path: socket_path.into(),
            source: error,
        },
        Ok(true) | Err(_) => ServerStopError::Unreachable {
            label: label.into(),
            path: socket_path.into(),
            source: error,
        },
    }
}

fn server_stop_request(id: &str, expected_boot_id: Option<&BootId>) -> crate::schema::Request {
    let method = match expected_boot_id {
        Some(expected_boot_id) => {
            crate::schema::Method::ServerStopIfBoot(crate::schema::ServerStopIfBootParams {
                expected_boot_id: expected_boot_id.clone(),
            })
        }
        None => crate::schema::Method::ServerStop(crate::schema::ServerStopParams::default()),
    };
    crate::schema::Request {
        id: id.into(),
        method,
    }
}

fn stop_request_error_allows_wait(err: &std::io::Error) -> bool {
    // This error came from the request after connecting; a refused or missing
    // listener cannot mean that an already-sent stop may be in flight.
    matches!(
        shepr_platform::ipc::classify_stream_error(err.kind()),
        shepr_platform::ipc::StreamFailure::PeerGone | shepr_platform::ipc::StreamFailure::TimedOut
    )
}

/// Whether the server socket has no listener. Errors never prove absence.
fn server_socket_is_stopped(socket: &Path) -> io::Result<bool> {
    shepr_platform::ipc::socket_is_live(socket).map(|live| !live)
}

fn wait_until_stopped_until(socket_path: &Path, deadline: Instant) -> io::Result<bool> {
    loop {
        // clock-io-ok: polls another process's socket while it exits.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if server_socket_is_stopped(socket_path)? {
            return Ok(true);
        }
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(STOP_WAIT_POLL.min(remaining));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::{IsolatedEnv, ScratchDir};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::Ordering;

    /// An isolated environment with config and state directories under its
    /// scratch HOME.
    fn isolated_config_env() -> (IsolatedEnv, shepr_config::AppPaths) {
        let env = IsolatedEnv::new();
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
        (env, paths)
    }

    #[test]
    fn stop_error_display_names_the_one_reachable_socket() {
        let error = ServerStopError::TimedOut {
            label: "test server".into(),
            timeout: Duration::from_millis(75),
            socket: PathBuf::from("/run/test.sock"),
        };
        assert_eq!(
            error.to_string(),
            "test server did not stop within 75ms; the socket at /run/test.sock is still reachable"
        );
    }

    #[test]
    fn stop_wait_timeout_allows_slow_graceful_shutdown() {
        assert_eq!(STOP_WAIT_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn stop_request_errors_wait_for_socket_state() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::WouldBlock,
        ] {
            let err = std::io::Error::from(kind);
            assert!(stop_request_error_allows_wait(&err), "{kind:?}");
        }
    }

    #[test]
    fn stop_request_empty_response_is_accepted() {
        let scratch = ScratchDir::new("stop-empty");
        let socket_path = scratch.join("s.sock");
        let listener =
            shepr_platform::ipc::bind_local_listener(&socket_path).expect("bind test stop socket");
        let handle = std::thread::spawn(move || {
            let server = listener.accept().expect("accept stop request").0;
            let mut request = String::new();
            BufReader::new(server)
                .read_line(&mut request)
                .expect("stop request line");
            request
        });
        let request = server_stop_request(
            "cli:server:stop",
            Some(&"17-23".parse().expect("boot identity")),
        );

        send_stop_request(
            &socket_path,
            &request,
            Instant::now() + Duration::from_millis(100),
            "test server",
            Some(&"17-23".parse().expect("boot identity")),
        )
        .expect("test precondition");
        let received = handle.join().expect("test precondition");
        let received: crate::schema::Request =
            serde_json::from_str(&received).expect("stop request is valid API JSON");
        assert_eq!(
            received,
            server_stop_request(
                "cli:server:stop",
                Some(&"17-23".parse().expect("boot identity"))
            )
        );
        assert_eq!(received.method.traits().name, "server.stop_if_boot");
    }

    #[test]
    fn stop_times_out_when_socket_stays_open_without_response() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().socket().to_path_buf();
        std::fs::create_dir_all(socket_path.parent().expect("test precondition"))
            .expect("test precondition");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("test precondition");
        listener.set_nonblocking(true).expect("test precondition");
        let keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let keep_running_for_thread = std::sync::Arc::clone(&keep_running);
        let handle = std::thread::spawn(move || {
            let mut held_streams = Vec::new();
            while keep_running_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Ok(reader_stream) = stream.try_clone() {
                            let mut request = String::new();
                            match BufReader::new(reader_stream).read_line(&mut request) {
                                Ok(0) | Err(_) => continue,
                                Ok(_) if request.contains("server.stop") => {
                                    held_streams.push(stream);
                                }
                                Ok(_) => {}
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let err = stop_active_server_with_timeout(&paths, None, Duration::from_millis(75))
            .expect_err("silent server should fail after timeout");

        assert!(matches!(err, ServerStopError::TimedOut { .. }), "{err}");
        keep_running.store(false, Ordering::Relaxed);
        handle.join().expect("test precondition");
    }

    /// Answers every request at `socket_path` with `reply` and records each
    /// request line, so a test can see what a stop sent.
    fn serve_reply(
        socket_path: &Path,
        reply: &'static str,
    ) -> (
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<Vec<String>>,
    ) {
        std::fs::create_dir_all(socket_path.parent().expect("test precondition"))
            .expect("test precondition");
        let listener =
            std::os::unix::net::UnixListener::bind(socket_path).expect("test precondition");
        listener.set_nonblocking(true).expect("test precondition");
        let keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let keep_running_for_thread = std::sync::Arc::clone(&keep_running);
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            while keep_running_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).expect("test precondition");
                        let mut request = String::new();
                        let reader = stream.try_clone().expect("test precondition");
                        // A bare connect that closes without a request is the
                        // stop's reachability poll, not a request.
                        match BufReader::new(reader).read_line(&mut request) {
                            Ok(0) | Err(_) => continue,
                            Ok(_) => {}
                        }
                        // A client may close before reading this reply; the test
                        // asserts on the requests recorded, not on the reply
                        // reaching every one of them.
                        drop(stream.write_all(reply.as_bytes()));
                        requests.push(request);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            requests
        });
        (keep_running, handle)
    }

    #[test]
    fn a_conditional_stop_refused_by_another_boot_reports_the_changed_occupant() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().socket().to_path_buf();
        let (keep_running, handle) = serve_reply(
            &socket_path,
            "{\"id\":\"cli:server:stop\",\"error\":{\"code\":\"server_boot_mismatch\",\"message\":\"this server is boot 9-9\"}}\n",
        );

        let error = stop_active_server(&paths, Some(&"1-1".parse().expect("boot identity")))
            .expect_err("a stop aimed at another boot is refused");

        keep_running.store(false, Ordering::Relaxed);
        let requests = handle.join().expect("test precondition");
        assert!(
            matches!(&error, ServerStopError::BootMismatch { expected_boot_id, .. }
                if expected_boot_id == "1-1"),
            "{error}"
        );
        assert!(error.is_boot_mismatch());
        assert_eq!(
            error.error_code(),
            crate::error::ApiErrorCode::ServerBootMismatch
        );
        // The expected boot travelled in the one stop request; nothing else was sent.
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].contains("server.stop"), "{requests:?}");
        assert!(
            requests[0].contains("\"expected_boot_id\":\"1-1\""),
            "{requests:?}"
        );
    }

    #[test]
    fn a_conditional_stop_reports_a_new_boot_after_acceptance() {
        let scratch = ScratchDir::new("stop-replaced");
        let socket_path = scratch.join("api.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("test precondition");
        listener.set_nonblocking(true).expect("test precondition");
        let keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let keep_running_for_thread = std::sync::Arc::clone(&keep_running);
        let handle = std::thread::spawn(move || {
            let mut status_requests = 0;
            while keep_running_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = String::new();
                        if BufReader::new(stream.try_clone().expect("test precondition"))
                            .read_line(&mut request)
                            .is_err()
                        {
                            continue;
                        }
                        let response = if request.contains("server.stop") {
                            "{\"id\":\"cli:server:stop\",\"result\":{\"type\":\"ok\"}}\n".to_owned()
                        } else {
                            status_requests += 1;
                            let boot_id = if status_requests == 1 {
                                "17-23"
                            } else {
                                "17-24"
                            };
                            format!(
                                "{{\"id\":\"api-client:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.1.0\",\"build_id\":\"0123456789abcdef\",\"boot_id\":\"{boot_id}\"}}}}\n"
                            )
                        };
                        drop(stream.write_all(response.as_bytes()));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let error = stop_socket_with_timeout(
            &socket_path,
            None,
            Duration::from_secs(2),
            "test server",
            Some(&"17-23".parse().expect("boot identity")),
        )
        .expect_err("a new boot must be reported instead of waiting for its socket");

        keep_running.store(false, Ordering::Relaxed);
        handle.join().expect("test precondition");
        assert!(
            matches!(
                &error,
                ServerStopError::OccupantChanged {
                    expected_boot_id,
                    actual_boot_id,
                    ..
                } if expected_boot_id == "17-23" && actual_boot_id == "17-24"
            ),
            "{error}"
        );
        assert!(error.is_boot_mismatch());
    }

    #[test]
    fn an_unconditional_stop_sends_no_expected_boot_and_never_reads_the_build() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().socket().to_path_buf();
        let (keep_running, handle) = serve_reply(
            &socket_path,
            "{\"id\":\"cli:server:stop\",\"result\":{\"type\":\"ok\"}}\n",
        );

        // The fake keeps its socket up, so the stop times out: what is under
        // test is the request it sent.
        let error = stop_active_server_with_timeout(&paths, None, Duration::from_millis(75))
            .expect_err("the fake never exits");

        keep_running.store(false, Ordering::Relaxed);
        let requests = handle.join().expect("test precondition");
        assert!(matches!(error, ServerStopError::TimedOut { .. }), "{error}");
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].contains("server.stop"), "{requests:?}");
        assert!(!requests[0].contains("ping"), "{requests:?}");
    }

    #[test]
    fn a_stop_rejects_a_success_response_with_the_wrong_result() {
        let scratch = ScratchDir::new("stop-wrong-result");
        let socket_path = scratch.join("api.sock");
        let reply = concat!(
            r#"{"id":"cli:server:stop","result":{"type":"pong","version":"0.1.0","#,
            r#""build_id":"0123456789abcdef","boot_id":"17-23"}}"#,
            "\n"
        );
        let (keep_running, handle) = serve_reply(&socket_path, reply);
        let request = server_stop_request("cli:server:stop", None);

        let error = send_stop_request(
            &socket_path,
            &request,
            Instant::now() + Duration::from_secs(1),
            "test server",
            None,
        )
        .expect_err("a pong is not a stop acceptance");

        keep_running.store(false, Ordering::Relaxed);
        let requests = handle.join().expect("test precondition");
        assert!(
            matches!(
                &error,
                ServerStopError::Protocol(message)
                    if message == "unexpected stop result from server"
            ),
            "{error}"
        );
        assert_eq!(requests.len(), 1, "{requests:?}");
    }

    #[test]
    fn a_stop_with_no_server_reports_not_running_without_starting_one() {
        let (_env, paths) = isolated_config_env();
        let error = stop_active_server(&paths, None).expect_err("nothing is listening");
        assert!(
            matches!(error, ServerStopError::NotRunning { .. }),
            "{error}"
        );
        assert!(
            !paths
                .server_address()
                .socket()
                .try_exists()
                .expect("test precondition"),
            "a stop must not create a server socket"
        );
    }

    #[test]
    fn stop_fails_when_socket_remains_reachable_after_timeout() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().socket().to_path_buf();
        std::fs::create_dir_all(socket_path.parent().expect("test precondition"))
            .expect("test precondition");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("test precondition");
        listener.set_nonblocking(true).expect("test precondition");
        let keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let keep_running_for_thread = std::sync::Arc::clone(&keep_running);
        let handle = std::thread::spawn(move || {
            while keep_running_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Ok(reader_stream) = stream.try_clone() {
                            let mut request = String::new();
                            match BufReader::new(reader_stream).read_line(&mut request) {
                                Ok(_) if !request.trim().is_empty() => {}
                                _ => continue,
                            }
                        }
                        // A client may close before reading this reply; the test
                        // asserts on the stop call's timeout, not on the reply. A
                        // Unix stream has nothing to flush.
                        drop(stream.write_all(
                            b"{\"id\":\"cli:server:stop\",\"result\":{\"type\":\"ok\"}}\n",
                        ));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let err = stop_active_server_with_timeout(&paths, None, Duration::from_millis(75))
            .expect_err("still-running server should fail");

        assert!(
            matches!(&err, ServerStopError::TimedOut { socket, .. } if socket == &socket_path),
            "{err}"
        );
        keep_running.store(false, Ordering::Relaxed);
        handle.join().expect("test precondition");
    }

    #[test]
    fn lease_wait_sees_a_held_lease_and_its_release() {
        let scratch = ScratchDir::new("stop-lease");
        let lease_path = scratch.join("session.lock");
        let soon = || Instant::now() + Duration::from_millis(60);

        // No lease file: never served, nothing to wait for, nothing created.
        assert!(wait_for_lease_release(&lease_path, soon()).expect("absent lease"));
        assert!(!lease_path.try_exists().expect("test precondition"));

        let held =
            shepr_platform::ipc::acquire_flock_lock(&lease_path, false).expect("hold the lease");
        assert!(!wait_for_lease_release(&lease_path, soon()).expect("held lease"));
        drop(held);
        assert!(wait_for_lease_release(&lease_path, soon()).expect("released lease"));
    }

    #[test]
    fn a_conditional_stop_waits_for_the_socket_to_disappear() {
        let scratch = ScratchDir::new("stop-socket");
        let path = scratch.join("server.sock");
        // Answers the stop, then keeps listening without answering a ping.
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind socket");
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let answer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("stop");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut request)
                .expect("request");
            assert!(request.contains("server.stop_if_boot"));
            stream
                .write_all(b"{\"id\":\"cli:server:stop\",\"result\":{\"type\":\"ok\"}}\n")
                .expect("answer");
            held_tx.send(listener).expect("retain listener");
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || {
            done_tx
                .send(stop_socket_with_timeout(
                    &path,
                    None,
                    Duration::from_secs(2),
                    "test server",
                    Some(&"17-23".parse().expect("boot identity")),
                ))
                .expect("result");
        });
        let held = held_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("listener retained");
        assert!(done_rx.recv_timeout(Duration::from_millis(75)).is_err());
        drop(held);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("stop completes")
            .expect("stopped");
        answer.join().expect("answer thread");
        stop.join().expect("stop thread");
    }

    #[test]
    fn a_conditional_stop_uses_the_lease_deadline_for_the_socket() {
        let scratch = ScratchDir::new("stop-lease-socket");
        let path = scratch.join("server.sock");
        let lease_path = scratch.join("session.lock");
        let held_lease =
            shepr_platform::ipc::acquire_flock_lock(&lease_path, false).expect("lease");
        // Answers the stop, then keeps listening without answering a ping.
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind socket");
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let answer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("stop");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut request)
                .expect("request");
            stream
                .write_all(b"{\"id\":\"cli:server:stop\",\"result\":{\"type\":\"ok\"}}\n")
                .expect("answer");
            held_tx.send(listener).expect("retain listener");
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stop = std::thread::spawn(move || {
            done_tx
                .send(stop_socket_with_timeout(
                    &path,
                    Some((&lease_path, Duration::from_millis(500))),
                    Duration::from_millis(75),
                    "test server",
                    Some(&"17-23".parse().expect("boot identity")),
                ))
                .expect("result");
        });
        let held = held_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("listener retained");
        assert!(done_rx.recv_timeout(Duration::from_millis(125)).is_err());
        drop(held_lease);
        assert!(
            done_rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "lease release alone must not finish the stop"
        );
        drop(held);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("stop completes")
            .expect("stopped");
        answer.join().expect("answer thread");
        stop.join().expect("stop thread");
    }
}
