use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::client::{ApiClient, ApiClientDeadlineError, ApiClientError};

// Stopping a server only connects to sockets (the API socket, to stop or
// probe a server); it never binds one. Binding goes through
// `ipc::bind_private_local_listener` in the server and API, and the peer check
// on accept is theirs, so nothing here needs the staged bind or `SO_PEERCRED`.

use crate::limits::{STOP_LEASE_WAIT_TIMEOUT, STOP_WAIT_POLL, STOP_WAIT_TIMEOUT};

/// The exit status `shepr server stop` ends with when no server is running at
/// the address, so a caller that ran it over SSH can tell "the server already
/// exited" from any other failure without parsing stderr.
// limits-exempt: process exit status shared by the server stop command and its SSH caller.
pub const NO_SERVER_EXIT_CODE: i32 = 4;

/// The exit status `shepr server stop --expect-boot` ends with when the server
/// that answered is not the boot it named, so a caller that ran it over SSH can
/// tell "the occupant changed" from any other failure without parsing stderr.
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
    /// The sockets are gone but the server still holds its data directory lease:
    /// it is still saving its layout, or is stuck.
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
}

impl ServerStopError {
    /// The API error code the CLI reports this failure under.
    pub fn error_code(&self) -> crate::error::ApiErrorCode {
        match self {
            Self::BootMismatch { .. } => crate::error::ApiErrorCode::ServerBootMismatch,
            _ => crate::error::ApiErrorCode::ServerStopFailed,
        }
    }

    /// Whether the stop was refused because the server is not the expected
    /// boot; nothing was stopped.
    pub fn is_boot_mismatch(&self) -> bool {
        matches!(self, Self::BootMismatch { .. })
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
                "{label} closed its sockets but still held {} {}ms later; it may still be saving its layout",
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

/// Guidance for a build meeting a running server of another build.
///
/// A dev and a release build keep separate runtime directories, so the server
/// met here is one another build of the same profile started. The only way
/// forward is to stop that server, which exits its panes, with the plain
/// `server stop` command: it stops whatever server answers, whatever its build.
fn restart_after_update_guidance(stop_command: &str, attach_command: Option<&str>) -> String {
    crate::guidance::operator_guidance(crate::guidance::OperatorGuidance::LocalBuildMismatch {
        stop_command,
        attach_command,
    })
}

pub fn restart_after_update_guidance_for(paths: &shepr_config::AppPaths) -> String {
    let address = paths.server_address();
    let stop_command = address.stop_command();
    let attach_command = address.attach_command();
    restart_after_update_guidance(&stop_command, Some(&attach_command))
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
/// when it is a different boot, so a server that replaced the observed one is
/// never stopped. Nothing here reads the status first, because a separate read
/// could not close that race. The stop is the same for a remote server: the
/// remote `shepr server stop --expect-boot <id>` runs this function on its own
/// host.
///
/// # Errors
///
/// When there is no server to stop, the stop request fails, the server does not
/// stop in time, or `expected_boot_id` names another boot.
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

/// Stops the server at `socket_path` and waits until `stopped_socket_paths`
/// are gone and, with `lease` (the lease file and how long to wait for it), the
/// server has released its data directory lease as well.
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
    let stopped = wait_until_stopped_until(stopped_socket_paths, deadline).map_err(|source| {
        ServerStopError::Io {
            context: format!("could not check whether {label} stopped"),
            source,
        }
    })?;
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
    if let Some((lease_path, lease_timeout)) = lease {
        // clock-io-ok: the lease wait polls another process's lock.
        let lease_deadline = Instant::now() + lease_timeout;
        let released = wait_for_lease_release(lease_path, lease_deadline).map_err(|source| {
            ServerStopError::Io {
                context: format!(
                    "could not check whether {label} released {}",
                    lease_path.display()
                ),
                source,
            }
        })?;
        if !released {
            return Err(ServerStopError::LeaseHeld {
                label: label.into(),
                timeout: lease_timeout,
                path: lease_path.into(),
            });
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
        match shepr_platform::ipc::acquire_flock_lock(lease_path, false) {
            Ok(_free) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        // clock-io-ok: polls another process's lock while it shuts down.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(STOP_WAIT_POLL.min(remaining));
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
        return Ok(());
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
    match shepr_platform::ipc::probe(socket_path) {
        shepr_platform::ipc::Liveness::Absent | shepr_platform::ipc::Liveness::Stale => {
            ServerStopError::NotRunning {
                label: label.into(),
                path: socket_path.into(),
                source: error,
            }
        }
        shepr_platform::ipc::Liveness::Live | shepr_platform::ipc::Liveness::Unreachable(_) => {
            ServerStopError::Unreachable {
                label: label.into(),
                path: socket_path.into(),
                source: error,
            }
        }
    }
}

fn server_stop_request(id: &str, expected_boot_id: Option<&str>) -> crate::schema::Request {
    crate::schema::Request {
        id: id.into(),
        method: crate::schema::Method::ServerStop(crate::schema::ServerStopParams {
            expected_boot_id: expected_boot_id.map(str::to_owned),
        }),
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

fn is_running_at(socket_path: &Path) -> std::io::Result<bool> {
    running_from_liveness(shepr_platform::ipc::probe(socket_path))
}

fn running_from_liveness(liveness: shepr_platform::ipc::Liveness) -> std::io::Result<bool> {
    match liveness {
        shepr_platform::ipc::Liveness::Absent | shepr_platform::ipc::Liveness::Stale => Ok(false),
        shepr_platform::ipc::Liveness::Live => Ok(true),
        shepr_platform::ipc::Liveness::Unreachable(error) => Err(error),
    }
}

fn all_sockets_stopped(socket_paths: &[PathBuf]) -> std::io::Result<bool> {
    for path in socket_paths {
        if is_running_at(path)? {
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
            return all_sockets_stopped(socket_paths);
        }
        if all_sockets_stopped(socket_paths)? {
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
    use interprocess::local_socket::traits::Listener as _;
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
            let server = listener.accept().expect("accept stop request");
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
        assert_eq!(received.method.traits().name, "server.stop");
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

    const KEEP_GUIDANCE: &str = "To keep the running server and its panes, keep using the shepr build that started it.\nTo use this build here instead, stop the running server; stopping exits its pane processes.";

    #[test]
    fn restart_after_update_guidance_names_the_plain_stop_and_attach_commands() {
        assert_eq!(
            restart_after_update_guidance("shepr server stop", Some("shepr")),
            format!("{KEEP_GUIDANCE} Run `shepr server stop`, then run `shepr` again.")
        );
        assert!(
            restart_after_update_guidance("shepr server stop", None)
                .contains("Run `shepr server stop`, then restart Shepr")
        );
    }

    #[test]
    fn guidance_never_mentions_named_sessions() {
        let (_env, paths) = isolated_config_env();
        let guidance = restart_after_update_guidance_for(&paths);
        assert!(!guidance.contains("--session"), "{guidance}");
        assert!(!guidance.contains("SHEPR_SESSION"), "{guidance}");
        assert!(
            guidance.contains("Run `shepr server stop`, then run `shepr` again."),
            "{guidance}"
        );
        assert!(!guidance.contains("--force"), "{guidance}");
    }

    #[test]
    fn restart_after_update_guidance_respects_socket_override() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, "/tmp/custom-shepr.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            restart_after_update_guidance_for(&paths),
            format!(
                "{KEEP_GUIDANCE} Run `SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr server stop`, then run `SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr` again."
            )
        );
    }

    #[test]
    fn restart_after_update_guidance_preserves_client_socket_override() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprClientSocketPath, "/tmp/work-client.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            restart_after_update_guidance_for(&paths),
            format!(
                "{KEEP_GUIDANCE} Run `SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr server stop`, then run `SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr` again."
            )
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
}
