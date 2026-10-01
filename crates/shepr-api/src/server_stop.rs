use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::client::{ApiClient, ApiClientDeadlineError, ApiClientError};

// Stopping a server only connects to sockets (the API socket, to stop or
// probe a server); it never binds one. Binding goes through
// `ipc::bind_private_local_listener` in the server and API, and the peer check
// on accept is theirs, so nothing here needs the staged bind or `SO_PEERCRED`.

use crate::limits::{
    STOP_LEASE_WAIT_TIMEOUT, STOP_STATUS_PROBE_TIMEOUT, STOP_WAIT_POLL, STOP_WAIT_TIMEOUT,
};

/// The exit status `shepr server stop` ends with when no server is running at
/// the address, so a caller that ran it over SSH can tell "the server already
/// exited" from any other failure without parsing stderr.
// limits-exempt: process exit status shared by the server stop command and its SSH caller.
pub const NO_SERVER_EXIT_CODE: i32 = 4;

/// The exit status `shepr server stop --expect-boot` ends with when a different
/// boot is found, either at the stop request or while the named boot shuts
/// down, so an SSH caller can identify a changed occupant without parsing stderr.
// limits-exempt: process exit status shared by the server stop command and its SSH caller.
pub const BOOT_MISMATCH_EXIT_CODE: i32 = 3;

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
        reachable: Vec<PathBuf>,
    },
    /// The server no longer answers, or its sockets disappeared, but a process
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
        expected_boot_id: String,
        /// The server's own words, naming the boot it is.
        detail: String,
    },
    /// A conditional stop was accepted for one boot, but a different boot
    /// answered while the requested boot was shutting down.
    OccupantChanged {
        label: String,
        expected_boot_id: String,
        actual_boot_id: String,
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
                reachable,
            } => write!(
                f,
                "{label} did not stop within {}ms; sockets are still reachable at {}",
                timeout.as_millis(),
                reachable
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::LeaseHeld {
                label,
                timeout,
                path,
            } => write!(
                f,
                "the data directory lease at {} was still held {}ms after {label} stopped answering or its sockets disappeared; another process may still be using it",
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

impl From<ServerStopError> for String {
    fn from(error: ServerStopError) -> Self {
        error.to_string()
    }
}

pub fn active_api_socket_path(paths: &shepr_config::AppPaths) -> PathBuf {
    paths.server_address().api_socket().to_path_buf()
}

/// Stops the server the resolved address names, whatever its build. This never
/// launches a server: with none listening it fails with
/// [`ServerStopError::NotRunning`].
///
/// With `expected_boot_id` (a boot identity read from that server's status) the
/// stop is conditional: the server checks the identity itself, in the same
/// request that stops it, and refuses with [`ServerStopError::BootMismatch`]
/// when it is a different boot. After accepting the request, this function
/// waits for that boot to stop answering, for both server sockets to stop
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
    expected_boot_id: Option<&str>,
) -> Result<(), ServerStopError> {
    stop_active_server_with_timeout(paths, expected_boot_id, STOP_WAIT_TIMEOUT)
}

fn stop_active_server_with_timeout(
    paths: &shepr_config::AppPaths,
    expected_boot_id: Option<&str>,
    timeout: Duration,
) -> Result<(), ServerStopError> {
    let address = paths.server_address();
    let socket_path = address.api_socket().to_path_buf();
    let client_socket_path = address.client_socket().to_path_buf();
    stop_socket_with_timeout(
        &socket_path,
        &[socket_path.clone(), client_socket_path],
        Some((&paths.data_dir_lease_path(), STOP_LEASE_WAIT_TIMEOUT)),
        timeout,
        "server",
        expected_boot_id,
    )
}

/// Stops the server at `socket_path` and waits for the named boot to stop
/// answering and its sockets to stop accepting connections when the request is
/// conditional, or for `stopped_socket_paths` to disappear otherwise. With
/// `lease` (the lease file and how long to wait for it), it also waits for the
/// data-directory lease to become available.
fn stop_socket_with_timeout(
    socket_path: &Path,
    stopped_socket_paths: &[PathBuf],
    lease: Option<(&Path, Duration)>,
    timeout: Duration,
    label: &str,
    expected_boot_id: Option<&str>,
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
                    expected_boot_id: expected_boot_id.into(),
                    actual_boot_id,
                });
            }
            BootStopWait::TimedOut => false,
        }
    } else {
        wait_until_stopped_until(stopped_socket_paths, deadline).map_err(|source| {
            ServerStopError::Io {
                context: format!("could not check whether {label} stopped"),
                source,
            }
        })?
    };
    if !stopped {
        let reachable =
            reachable_socket_paths(stopped_socket_paths).map_err(|source| ServerStopError::Io {
                context: format!("could not check whether {label} stopped"),
                source,
            })?;
        return Err(ServerStopError::TimedOut {
            label: label.into(),
            timeout,
            reachable,
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
                        expected_boot_id: expected_boot_id.into(),
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
        // Lease release may consume its separate budget before the final
        // endpoint disappears, so retain the later deadline for that wait.
        socket_deadline = socket_deadline.max(lease_deadline);
    }
    if let Some(expected_boot_id) = expected_boot_id {
        match wait_until_sockets_stopped_or_new_boot(
            stopped_socket_paths,
            socket_path,
            expected_boot_id,
            socket_deadline,
            label,
        )? {
            BootStopWait::Gone => {}
            BootStopWait::Changed(actual_boot_id) => {
                return Err(ServerStopError::OccupantChanged {
                    label: label.into(),
                    expected_boot_id: expected_boot_id.into(),
                    actual_boot_id,
                });
            }
            BootStopWait::TimedOut => {
                let reachable = reachable_socket_paths(stopped_socket_paths).map_err(|source| {
                    ServerStopError::Io {
                        context: format!("could not check whether {label} stopped"),
                        source,
                    }
                })?;
                return Err(ServerStopError::TimedOut {
                    label: label.into(),
                    timeout,
                    reachable,
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
                    expected_boot_id: expected_boot_id.into(),
                    actual_boot_id,
                });
            }
            BootProbe::Expected => {
                match wait_until_boot_stops(socket_path, expected_boot_id, deadline, label)? {
                    BootStopWait::Gone => {}
                    BootStopWait::Changed(actual_boot_id) => {
                        return Err(ServerStopError::OccupantChanged {
                            label: label.into(),
                            expected_boot_id: expected_boot_id.into(),
                            actual_boot_id,
                        });
                    }
                    BootStopWait::TimedOut => {
                        let reachable =
                            reachable_socket_paths(stopped_socket_paths).map_err(|source| {
                                ServerStopError::Io {
                                    context: format!("could not check whether {label} stopped"),
                                    source,
                                }
                            })?;
                        return Err(ServerStopError::TimedOut {
                            label: label.into(),
                            timeout,
                            reachable,
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
// race a server start before its sockets bind; the client launcher retries a
// daemon that loses the probe. Keeping it lets a stop detect a successor that
// acquired the lease before either endpoint appeared.
fn data_dir_lease_is_free(lease_path: &Path) -> io::Result<bool> {
    match shepr_platform::ipc::acquire_flock_lock(lease_path, false) {
        Ok(_free) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error),
    }
}

enum BootProbe {
    Gone,
    Expected,
    Changed(String),
}

enum BootStopWait {
    Gone,
    Changed(String),
    TimedOut,
}

enum LeaseWait {
    Released,
    Held,
    NewBoot(String),
}

fn probe_boot(
    socket_path: &Path,
    expected_boot_id: &str,
    label: &str,
    deadline: Instant,
) -> Result<BootProbe, ServerStopError> {
    // clock-io-ok: bounds one real status request on the socket.
    let probe_deadline = (Instant::now() + STOP_STATUS_PROBE_TIMEOUT).min(deadline);
    let client = ApiClient::for_socket(socket_path);
    match client.status_until(probe_deadline) {
        Ok(status) if status.boot_id == expected_boot_id => Ok(BootProbe::Expected),
        Ok(status) => Ok(BootProbe::Changed(status.boot_id)),
        Err(error) if status_probe_has_no_answer(&error) => Ok(BootProbe::Gone),
        Err(error) => Err(status_probe_error(error, label)),
    }
}

fn wait_until_boot_stops(
    socket_path: &Path,
    expected_boot_id: &str,
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

/// A conditional stop is complete for launchers only when the socket pair is
/// gone too. The server drops its lease and API listener before its client
/// listener, so the API boot can stop answering while the launcher's first
/// socket is still live. Keep checking the API identity during that interval
/// so a replacement is reported promptly.
fn wait_until_sockets_stopped_or_new_boot(
    socket_paths: &[PathBuf],
    api_socket_path: &Path,
    expected_boot_id: &str,
    deadline: Instant,
    label: &str,
) -> Result<BootStopWait, ServerStopError> {
    loop {
        match probe_boot(api_socket_path, expected_boot_id, label, deadline)? {
            BootProbe::Gone | BootProbe::Expected => {}
            BootProbe::Changed(actual_boot_id) => {
                return Ok(BootStopWait::Changed(actual_boot_id));
            }
        }
        let sockets_stopped =
            server_sockets_are_stopped(socket_paths).map_err(|source| ServerStopError::Io {
                context: format!("could not check whether {label} stopped"),
                source,
            })?;
        if sockets_stopped {
            return Ok(BootStopWait::Gone);
        }
        // clock-io-ok: polls server sockets while the named process exits.
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
    expected_boot_id: &str,
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

/// After the stop request was accepted, a transport close, refusal, or timeout
/// means this socket did not answer the boot probe. Decoded API failures remain
/// errors because they are not evidence that the named boot went away.
fn status_probe_has_no_answer(error: &ApiClientDeadlineError) -> bool {
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
    expected_boot_id: Option<&str>,
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
    match client.request_value_until(request, deadline) {
        Ok(response) if response.get("error").is_some() => {
            let error = &response["error"];
            let refused_boot = error["code"].as_str()
                == Some(crate::error::ApiErrorCode::ServerBootMismatch.as_str());
            match expected_boot_id {
                Some(expected) if refused_boot => Err(ServerStopError::BootMismatch {
                    label: label.into(),
                    expected_boot_id: expected.into(),
                    detail: error["message"].as_str().unwrap_or_default().into(),
                }),
                _ => Err(ServerStopError::Protocol(error.to_string())),
            }
        }
        Err(ApiClientDeadlineError::Connect(error)) => {
            Err(stop_socket_io_error(socket_path, label, error))
        }
        Ok(_) | Err(ApiClientDeadlineError::Request(ApiClientError::EmptyResponse)) => Ok(()),
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
            Err(ServerStopError::Protocol(response.error.message))
        }
        Err(ApiClientDeadlineError::Request(ApiClientError::UnexpectedResult(result))) => {
            Err(ServerStopError::Protocol(result))
        }
    }
}

fn stop_socket_io_error(socket_path: &Path, label: &str, error: io::Error) -> ServerStopError {
    match server_socket_is_live(socket_path) {
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

fn server_stop_request(id: &str, expected_boot_id: Option<&str>) -> crate::schema::Request {
    let method = match expected_boot_id {
        Some(expected_boot_id) => {
            crate::schema::Method::ServerStopIfBoot(crate::schema::ServerStopIfBootParams {
                expected_boot_id: expected_boot_id.to_owned(),
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
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
    )
}

/// Whether a server socket has a live listener. Absent and stale paths mean
/// no listener; inaccessible paths remain errors because they do not prove
/// that starting or stopping another server is safe. The launcher and stop
/// share this mapping so a successful conditional stop satisfies the
/// launcher's socket-presence check.
pub fn server_socket_is_live(socket_path: &Path) -> std::io::Result<bool> {
    running_from_liveness(shepr_platform::ipc::probe(socket_path))
}

fn is_running_at(socket_path: &Path) -> std::io::Result<bool> {
    server_socket_is_live(socket_path)
}

fn running_from_liveness(liveness: shepr_platform::ipc::Liveness) -> std::io::Result<bool> {
    match liveness {
        shepr_platform::ipc::Liveness::Absent | shepr_platform::ipc::Liveness::Stale => Ok(false),
        shepr_platform::ipc::Liveness::Live => Ok(true),
        shepr_platform::ipc::Liveness::Unreachable(error) => Err(error),
    }
}

/// Whether every server endpoint socket has no listener. Launcher and stop
/// use this same predicate before treating a server as gone.
pub fn server_sockets_are_stopped<P: AsRef<Path>>(socket_paths: &[P]) -> std::io::Result<bool> {
    for path in socket_paths {
        if server_socket_is_live(path.as_ref())? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn wait_until_stopped_until(socket_paths: &[PathBuf], deadline: Instant) -> std::io::Result<bool> {
    loop {
        // clock-io-ok: polls another process's sockets while it exits.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return server_sockets_are_stopped(socket_paths);
        }
        if server_sockets_are_stopped(socket_paths)? {
            return Ok(true);
        }
        std::thread::sleep(STOP_WAIT_POLL.min(remaining));
    }
}

fn reachable_socket_paths(socket_paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut reachable = Vec::new();
    for path in socket_paths {
        if is_running_at(path)? {
            reachable.push(path.clone());
        }
    }
    Ok(reachable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::env::EnvVar;
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
        let request = server_stop_request("cli:server:stop", Some("17-23"));

        send_stop_request(
            &socket_path,
            &request,
            Instant::now() + Duration::from_millis(100),
            "test server",
            Some("17-23"),
        )
        .expect("test precondition");
        let received = handle.join().expect("test precondition");
        let received: crate::schema::Request =
            serde_json::from_str(&received).expect("stop request is valid API JSON");
        assert_eq!(
            received,
            server_stop_request("cli:server:stop", Some("17-23"))
        );
        assert_eq!(received.method.traits().name, "server.stop_if_boot");
    }

    #[test]
    fn stop_times_out_when_socket_stays_open_without_response() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().api_socket().to_path_buf();
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

    #[test]
    fn the_active_socket_is_the_build_runtime_socket() {
        let (_env, paths) = isolated_config_env();
        assert_eq!(
            active_api_socket_path(&paths),
            paths.runtime_dir().join("shepr.sock")
        );
    }

    #[test]
    fn env_socket_override_selects_the_active_socket() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, "/tmp/explicit.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            active_api_socket_path(&paths),
            PathBuf::from("/tmp/explicit.sock")
        );
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
        let socket_path = paths.server_address().api_socket().to_path_buf();
        let (keep_running, handle) = serve_reply(
            &socket_path,
            "{\"id\":\"cli:server:stop\",\"error\":{\"code\":\"server_boot_mismatch\",\"message\":\"this server is boot 9-9\"}}\n",
        );

        let error = stop_active_server(&paths, Some("1-1"))
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
                                "old-boot"
                            } else {
                                "new-boot"
                            };
                            format!(
                                "{{\"id\":\"api-client:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.1.0\",\"build_id\":\"build\",\"boot_id\":\"{boot_id}\"}}}}\n"
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
            std::slice::from_ref(&socket_path),
            None,
            Duration::from_secs(2),
            "test server",
            Some("old-boot"),
        )
        .expect_err("a new boot must be reported instead of waiting for its sockets");

        keep_running.store(false, Ordering::Relaxed);
        handle.join().expect("test precondition");
        assert!(
            matches!(
                &error,
                ServerStopError::OccupantChanged {
                    expected_boot_id,
                    actual_boot_id,
                    ..
                } if expected_boot_id == "old-boot" && actual_boot_id == "new-boot"
            ),
            "{error}"
        );
        assert!(error.is_boot_mismatch());
    }

    #[test]
    fn an_unconditional_stop_sends_no_expected_boot_and_never_reads_the_build() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().api_socket().to_path_buf();
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
                .api_socket()
                .try_exists()
                .expect("test precondition"),
            "a stop must not create a server socket"
        );
    }

    #[test]
    fn stop_fails_when_socket_remains_reachable_after_timeout() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().api_socket().to_path_buf();
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
                        drop(stream.write_all(b"{\"id\":\"cli:server:stop\",\"result\":{}}\n"));
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
            matches!(&err, ServerStopError::TimedOut { reachable, .. } if reachable.contains(&socket_path)),
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
    fn socket_liveness_maps_absent_and_stale_to_stopped() {
        assert!(
            !running_from_liveness(shepr_platform::ipc::Liveness::Absent).expect("absent status")
        );
        assert!(
            !running_from_liveness(shepr_platform::ipc::Liveness::Stale).expect("stale status")
        );
        assert!(running_from_liveness(shepr_platform::ipc::Liveness::Live).expect("live status"));

        let error = running_from_liveness(shepr_platform::ipc::Liveness::Unreachable(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        ))
        .expect_err("unreachable sockets remain transport errors");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn server_socket_presence_uses_the_shared_liveness_rule() {
        let scratch = ScratchDir::new("server-socket-presence");
        let socket_path = scratch.join("server.sock");
        assert!(!server_socket_is_live(&socket_path).expect("an absent socket is stopped"));
        assert!(server_sockets_are_stopped(&[socket_path.as_path()]).expect("no listener"));

        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("bind the test server socket");
        assert!(server_socket_is_live(&socket_path).expect("a listener is live"));
        assert!(!server_sockets_are_stopped(&[socket_path.as_path()]).expect("listener is live"));
        drop(listener);
        assert!(!server_socket_is_live(&socket_path).expect("a stale socket is stopped"));
        assert!(server_sockets_are_stopped(&[socket_path.as_path()]).expect("stale is stopped"));
    }

    #[test]
    fn a_conditional_stop_waits_for_the_client_socket_to_disappear() {
        let scratch = ScratchDir::new("stop-client-socket");
        let api_socket = scratch.join("api.sock");
        let client_socket = scratch.join("client.sock");
        let api_listener =
            std::os::unix::net::UnixListener::bind(&api_socket).expect("bind the test API socket");
        let client_listener = std::os::unix::net::UnixListener::bind(&client_socket)
            .expect("bind the test client socket");
        let api_thread = std::thread::spawn(move || {
            let (mut stream, _) = api_listener.accept().expect("accept the stop request");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone the test stream"))
                .read_line(&mut request)
                .expect("read the stop request");
            assert!(request.contains("server.stop_if_boot"), "{request}");
            stream
                .write_all(b"{\"id\":\"cli:server:stop\",\"result\":{\"type\":\"ok\"}}\n")
                .expect("answer the stop request");
        });
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let stop_api_socket = api_socket.clone();
        let stop_paths = vec![api_socket, client_socket];
        let stop_thread = std::thread::spawn(move || {
            let result = stop_socket_with_timeout(
                &stop_api_socket,
                &stop_paths,
                None,
                Duration::from_secs(2),
                "test server",
                Some("old-boot"),
            );
            finished_tx.send(result).expect("send the stop result");
        });

        assert!(
            finished_rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "the stop must wait while the launcher-visible client socket is live"
        );
        drop(client_listener);
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the stop completes when its sockets disappear")
            .expect("the named server stopped");
        api_thread.join().expect("test API thread");
        stop_thread.join().expect("test stop thread");
    }

    #[test]
    fn a_conditional_stop_uses_the_lease_deadline_for_the_last_socket() {
        let scratch = ScratchDir::new("stop-lease-client-socket");
        let api_socket = scratch.join("api.sock");
        let client_socket = scratch.join("client.sock");
        let lease_path = scratch.join("session.lock");
        let api_listener =
            std::os::unix::net::UnixListener::bind(&api_socket).expect("bind the test API socket");
        let client_listener = std::os::unix::net::UnixListener::bind(&client_socket)
            .expect("bind the test client socket");
        let held =
            shepr_platform::ipc::acquire_flock_lock(&lease_path, false).expect("hold the lease");
        let api_thread = std::thread::spawn(move || {
            let (mut stream, _) = api_listener.accept().expect("accept the stop request");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone the test stream"))
                .read_line(&mut request)
                .expect("read the stop request");
            stream
                .write_all(b"{\"id\":\"cli:server:stop\",\"result\":{\"type\":\"ok\"}}\n")
                .expect("answer the stop request");
        });

        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let stop_api_socket = api_socket.clone();
        let stop_paths = vec![api_socket, client_socket];
        let stop_thread = std::thread::spawn(move || {
            let result = stop_socket_with_timeout(
                &stop_api_socket,
                &stop_paths,
                Some((&lease_path, Duration::from_millis(300))),
                Duration::from_millis(75),
                "test server",
                Some("old-boot"),
            );
            finished_tx.send(result).expect("send the stop result");
        });
        api_thread.join().expect("test API thread");

        std::thread::sleep(Duration::from_millis(125));
        drop(held);
        assert!(
            finished_rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "the socket wait must continue while the lease deadline remains"
        );
        drop(client_listener);
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the socket wait completes when the client endpoint disappears")
            .expect("the named server stopped");
        stop_thread.join().expect("test stop thread");
    }
}
