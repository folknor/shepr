use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::client::{ApiClient, ApiClientDeadlineError, ApiClientError};

// Session management only connects to sockets (the API socket, to stop or
// probe a server); it never binds one. Binding goes through
// `ipc::bind_private_local_listener` in the server and API, and the peer check
// on accept is theirs, so nothing here needs the staged bind or `SO_PEERCRED`.

use crate::limits::{STOP_STATUS_TIMEOUT, STOP_WAIT_POLL, STOP_WAIT_TIMEOUT};
use shepr_config::DEFAULT_SESSION_NAME;
use shepr_config::{SessionId, SessionName, SessionNameError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub default: bool,
    pub running: bool,
    pub socket_path: PathBuf,
    pub session_dir: PathBuf,
}

#[derive(Debug)]
pub enum SessionError {
    InvalidName(String),
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
    Running {
        name: String,
    },
    DefaultDelete,
    NameMismatch {
        name: String,
    },
    Io {
        context: String,
        source: io::Error,
    },
    Protocol(String),
    /// A stop aimed at a server of another build, or one whose build could not
    /// be read, without [`FORCE_STOP_FLAG`].
    BuildMismatch {
        label: String,
        running: String,
        force_command: String,
    },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(message) | Self::Protocol(message) => f.write_str(message),
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
            Self::Running { name } => {
                write!(f, "session {name} is running; stop it before deleting")
            }
            Self::DefaultDelete => f.write_str("deleting the default session is not supported"),
            Self::NameMismatch { name } => write!(
                f,
                "session {name} does not match an exact session name; use the spelling shown by `shepr session list`"
            ),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::BuildMismatch {
                label,
                running,
                force_command,
            } => write!(
                f,
                "refusing to stop {label}: it runs a different shepr build (running build {running}; this is build {}). Stopping it exits its pane processes. If that is the server you mean to stop, run `{force_command}`. To run this build alongside it instead, give it a session of its own with `--session <name>`.",
                shepr_protocol::BUILD_ID
            ),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotRunning { source, .. }
            | Self::Unreachable { source, .. }
            | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<SessionNameError> for SessionError {
    fn from(SessionNameError(message): SessionNameError) -> Self {
        Self::InvalidName(message)
    }
}

impl From<io::Error> for SessionError {
    fn from(source: io::Error) -> Self {
        Self::Io {
            context: "session operation failed".into(),
            source,
        }
    }
}

impl From<SessionError> for String {
    fn from(error: SessionError) -> Self {
        error.to_string()
    }
}

/// Guidance for a build meeting a running server of another build.
///
/// Two ways out, and the non-destructive one first: that server may be the
/// installed one with every live agent in it, and a dev run landing on its
/// session is the ordinary way to get here. Running this build in a session of
/// its own leaves it alone; stopping it exits its panes, and because the stop
/// is refused against a mismatched server without [`FORCE_STOP_FLAG`], the
/// command named here carries the flag.
fn restart_after_update_guidance(stop_command: &str, attach_command: Option<&str>) -> String {
    crate::guidance::operator_guidance(crate::guidance::OperatorGuidance::LocalBuildMismatch {
        stop_command,
        attach_command,
    })
}

pub fn restart_after_update_guidance_for(paths: &shepr_config::AppPaths) -> String {
    let address = paths.server_address();
    let session = paths.session_id();
    let stop_command = address.stop_command(session);
    let attach_command = address.attach_command(session);
    restart_after_update_guidance(&stop_command, Some(&attach_command))
}

pub fn data_dir(paths: &shepr_config::AppPaths) -> PathBuf {
    paths.session_id().data_dir(paths)
}

pub fn sessions_dir(paths: &shepr_config::AppPaths) -> PathBuf {
    sessions_dir_under(paths.state_dir())
}

fn sessions_dir_under(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions")
}

pub fn active_api_socket_path(paths: &shepr_config::AppPaths) -> PathBuf {
    paths.server_address().api_socket().to_path_buf()
}

pub fn list_sessions(paths: &shepr_config::AppPaths) -> std::io::Result<Vec<SessionInfo>> {
    let mut sessions = vec![session_info(paths, &SessionId::Default)?];
    let sessions_dir = sessions_dir(paths);
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(err) => return Err(err),
    };

    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name != DEFAULT_SESSION_NAME
            && let Ok(name) = SessionName::parse(&name)
        {
            names.push(name);
        }
    }
    names.sort();
    for name in &names {
        sessions.push(session_info(paths, &SessionId::Named(name.clone()))?);
    }
    Ok(sessions)
}

pub fn session_info(
    paths: &shepr_config::AppPaths,
    session: &SessionId,
) -> std::io::Result<SessionInfo> {
    let default = session.is_default();
    let display_name = session.display_name().to_string();
    let socket_path = session.api_socket_path(paths);
    let session_dir = session.data_dir(paths);
    Ok(SessionInfo {
        name: display_name,
        default,
        running: is_running_at(&socket_path)?,
        socket_path,
        session_dir,
    })
}

pub fn parse_target_name(name: &str) -> Result<SessionId, SessionError> {
    SessionId::parse(name).map_err(Into::into)
}

/// The flag that lets `server stop` and `session stop` stop a server of
/// another build.
pub const FORCE_STOP_FLAG: &str = "--force";

/// What a stop learned about the build of the server it is about to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopTargetBuild {
    /// Nothing is listening, or the socket cannot be reached. The stop request
    /// itself reports that, and there is no server it could wrongly stop.
    NotListening,
    /// The server states this exact build.
    ThisBuild,
    /// The server states another build, or could not say which build it is.
    Other { running: String },
}

impl StopTargetBuild {
    /// The classification of a build a server stated.
    pub fn from_running_build(build_id: &str) -> Self {
        if shepr_protocol::is_this_build(build_id) {
            Self::ThisBuild
        } else {
            Self::Other {
                running: build_id.to_owned(),
            }
        }
    }
}

/// Asks the server listening at `socket_path` which build it runs.
pub fn stop_target_build(socket_path: &Path) -> StopTargetBuild {
    match shepr_platform::ipc::probe(socket_path) {
        shepr_platform::ipc::Liveness::Absent
        | shepr_platform::ipc::Liveness::Stale
        | shepr_platform::ipc::Liveness::Unreachable(_) => StopTargetBuild::NotListening,
        shepr_platform::ipc::Liveness::Live => {
            match crate::read_runtime_status_at(socket_path, STOP_STATUS_TIMEOUT) {
                Ok(Some(status)) => StopTargetBuild::from_running_build(&status.build_id),
                Ok(None) => StopTargetBuild::Other {
                    running: "unknown (the server did not answer the status request)".into(),
                },
                Err(error) => StopTargetBuild::Other {
                    running: format!("unknown ({error})"),
                },
            }
        }
    }
}

/// Refuses to stop a server of another build unless the operator stated the
/// intent with [`FORCE_STOP_FLAG`].
///
/// `server stop` and `session stop` cannot use the per-command build check,
/// because the mismatch guidance tells the operator to stop exactly such a
/// server. Skipping the check silently, though, let a dev build's `server
/// stop` without `--session` stop the installed server and every pane in it.
/// So the mismatch is refused, naming both builds and the forced command.
///
/// # Errors
///
/// [`SessionError::BuildMismatch`] when `target` is another build and `force`
/// is not set.
pub fn guard_mismatched_stop(
    label: &str,
    target: &StopTargetBuild,
    force: bool,
    force_command: &str,
) -> Result<(), SessionError> {
    match target {
        StopTargetBuild::Other { running } if !force => Err(SessionError::BuildMismatch {
            label: label.into(),
            running: running.clone(),
            force_command: force_command.into(),
        }),
        _ => Ok(()),
    }
}

/// `shepr session stop --force <name>`, with an option terminator before a
/// name that starts with `-`, which session names may.
fn forced_session_stop_command(name: &str) -> String {
    let terminator = if name.starts_with('-') { "-- " } else { "" };
    format!("shepr session stop {FORCE_STOP_FLAG} {terminator}{name}")
}

/// Stops a session. A server of another build is stopped only with `force`.
///
/// # Errors
///
/// As [`guard_mismatched_stop`], or when the stop request fails.
pub fn stop_session(
    paths: &shepr_config::AppPaths,
    session: &SessionId,
    force: bool,
) -> Result<SessionInfo, SessionError> {
    stop_session_with_timeout(paths, session, force, STOP_WAIT_TIMEOUT)
}

/// Stops the server the resolved address names. A server of another build is
/// stopped only with `force`.
///
/// # Errors
///
/// As [`guard_mismatched_stop`], or when the stop request fails.
pub fn stop_active_server(paths: &shepr_config::AppPaths, force: bool) -> Result<(), SessionError> {
    let address = paths.server_address();
    let socket_path = address.api_socket().to_path_buf();
    let client_socket_path = address.client_socket().to_path_buf();
    let force_command = format!(
        "{} {FORCE_STOP_FLAG}",
        address.stop_command(paths.session_id())
    );
    guard_mismatched_stop(
        "the server",
        &stop_target_build(&socket_path),
        force,
        &force_command,
    )?;
    stop_socket_with_timeout(
        &socket_path,
        &[socket_path.clone(), client_socket_path],
        STOP_WAIT_TIMEOUT,
        "server",
    )
}

fn stop_session_with_timeout(
    paths: &shepr_config::AppPaths,
    session: &SessionId,
    force: bool,
    timeout: Duration,
) -> Result<SessionInfo, SessionError> {
    let socket_path = session.api_socket_path(paths);
    let client_socket_path = session.client_socket_path(paths);
    let label = format!("session {}", session.display_name());
    guard_mismatched_stop(
        &label,
        &stop_target_build(&socket_path),
        force,
        &forced_session_stop_command(session.display_name()),
    )?;
    stop_socket_with_timeout(
        &socket_path,
        &[socket_path.clone(), client_socket_path],
        timeout,
        &label,
    )?;
    session_info(paths, session).map_err(SessionError::from)
}

fn stop_socket_with_timeout(
    socket_path: &Path,
    stopped_socket_paths: &[PathBuf],
    timeout: Duration,
    label: &str,
) -> Result<(), SessionError> {
    // clock-io-ok: one deadline bounds the real stop request's socket reads
    // and the server process's exit, so it must share their real clock.
    let deadline = Instant::now() + timeout;
    let request = server_stop_request("cli:session:stop");
    send_stop_request(socket_path, &request, deadline, label)?;
    let stopped = wait_until_stopped_until(stopped_socket_paths, deadline).map_err(|source| {
        SessionError::Io {
            context: format!("could not check whether {label} stopped"),
            source,
        }
    })?;
    if !stopped {
        let reachable =
            reachable_socket_paths(stopped_socket_paths).map_err(|source| SessionError::Io {
                context: format!("could not check whether {label} stopped"),
                source,
            })?;
        return Err(SessionError::TimedOut {
            label: label.into(),
            timeout,
            reachable,
        });
    }
    Ok(())
}

pub fn delete_session(
    paths: &shepr_config::AppPaths,
    session: &SessionId,
) -> Result<SessionInfo, SessionError> {
    let SessionId::Named(name) = session else {
        return Err(SessionError::DefaultDelete);
    };
    let name = name.as_str();
    let Some(dir) = exact_session_dir_for_delete(paths, name)? else {
        return session_info(paths, session).map_err(SessionError::from);
    };
    let socket_path = session.api_socket_path(paths);
    if is_running_at(&socket_path).map_err(|source| SessionError::Io {
        context: format!(
            "failed to inspect session {name} socket {}",
            socket_path.display()
        ),
        source,
    })? {
        return Err(SessionError::Running { name: name.into() });
    }
    let info = session_info(paths, session).map_err(|source| SessionError::Io {
        context: format!(
            "failed to inspect session {name} socket {}",
            socket_path.display()
        ),
        source,
    })?;
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(info),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(info),
        Err(source) => Err(SessionError::Io {
            context: format!("could not delete session {name}"),
            source,
        }),
    }
}

fn exact_session_dir_for_delete(
    paths: &shepr_config::AppPaths,
    name: &str,
) -> Result<Option<PathBuf>, SessionError> {
    let sessions_dir = sessions_dir(paths);
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name() == std::ffi::OsStr::new(name) {
            return Ok(Some(entry.path()));
        }
    }

    // A path lookup alone can resolve a different spelling on case-insensitive
    // filesystems. Never probe its socket or delete it without an exact entry.
    match std::fs::symlink_metadata(sessions_dir.join(name)) {
        Ok(_) => Err(SessionError::NameMismatch { name: name.into() }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn send_stop_request(
    socket_path: &Path,
    request: &crate::schema::Request,
    deadline: Instant,
    label: &str,
) -> Result<(), SessionError> {
    // clock-io-ok: the deadline is the one the real socket reader below keeps.
    if deadline.saturating_duration_since(Instant::now()).is_zero() {
        return Ok(());
    }
    let client = ApiClient::for_socket(socket_path);
    match client.request_value_until(request, deadline) {
        Ok(response) if response.get("error").is_some() => {
            Err(SessionError::Protocol(response["error"].to_string()))
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
            Err(SessionError::Protocol(error.to_string()))
        }
        Err(ApiClientDeadlineError::Request(ApiClientError::ErrorResponse(response))) => {
            Err(SessionError::Protocol(response.error.message))
        }
        Err(ApiClientDeadlineError::Request(ApiClientError::UnexpectedResult(result))) => {
            Err(SessionError::Protocol(result))
        }
    }
}

fn stop_socket_io_error(socket_path: &Path, label: &str, error: io::Error) -> SessionError {
    match shepr_platform::ipc::probe(socket_path) {
        shepr_platform::ipc::Liveness::Absent | shepr_platform::ipc::Liveness::Stale => {
            SessionError::NotRunning {
                label: label.into(),
                path: socket_path.into(),
                source: error,
            }
        }
        shepr_platform::ipc::Liveness::Live | shepr_platform::ipc::Liveness::Unreachable(_) => {
            SessionError::Unreachable {
                label: label.into(),
                path: socket_path.into(),
                source: error,
            }
        }
    }
}

fn server_stop_request(id: &str) -> crate::schema::Request {
    crate::schema::Request {
        id: id.into(),
        method: crate::schema::Method::ServerStop(crate::schema::EmptyParams::default()),
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

pub fn validate_name(name: &str) -> Result<(), SessionError> {
    shepr_config::validate_session_name(name)
        .map_err(|SessionNameError(message)| SessionError::InvalidName(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use shepr_core::env::EnvVar;
    use shepr_test_fixtures::AppPathsFixture as _;
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
        let request = server_stop_request("cli:session:stop");

        send_stop_request(
            &socket_path,
            &request,
            Instant::now() + Duration::from_millis(100),
            "test session",
        )
        .expect("test precondition");
        let received = handle.join().expect("test precondition");
        let received: crate::schema::Request =
            serde_json::from_str(&received).expect("stop request is valid API JSON");
        assert_eq!(received, server_stop_request("cli:session:stop"));
        assert_eq!(received.method.traits().name, "server.stop");
    }

    #[test]
    fn stop_session_times_out_when_socket_stays_open_without_response() {
        let (_env, _) = isolated_config_env();
        let session_name = "silent";
        let session = SessionId::parse(session_name).expect("test precondition");
        let paths = shepr_config::AppPaths::resolve_with_session(Some(session.clone()))
            .expect("isolated paths resolve");
        let socket_path = session.api_socket_path(&paths);
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

        // Forced: this fake never answers the build probe, and the timeout is
        // what is under test.
        let err = stop_session_with_timeout(&paths, &session, true, Duration::from_millis(75))
            .expect_err("silent session should fail after timeout");

        assert!(matches!(err, SessionError::TimedOut { .. }), "{err}");
        keep_running.store(false, Ordering::Relaxed);
        handle.join().expect("test precondition");
    }

    // Which argv words name the session is the parser's job (tests in
    // `cli.rs`); here the parsed identity and address are resolved once.

    #[test]
    fn requested_session_does_not_mutate_the_parent_environment() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "inherited");
        env.set(EnvVar::SheprSocketPath, "/tmp/inherited.sock");
        let requested = SessionId::parse("work").expect("test precondition");

        let (resolved, explicit) = SessionId::resolve(Some(requested.clone()), Some("bad/name"))
            .expect("explicit selection wins");

        assert_eq!(resolved, requested);
        assert!(explicit);
        assert_eq!(
            env_text(&env, EnvVar::SheprSession).as_deref(),
            Some("inherited")
        );
        assert_eq!(
            env_text(&env, EnvVar::SheprSocketPath).as_deref(),
            Some("/tmp/inherited.sock")
        );
    }

    /// A variable's raw value as text, for asserting what the environment
    /// still carries.
    fn env_text(env: &IsolatedEnv, var: EnvVar) -> Option<String> {
        env.get(var).and_then(|value| value.into_string().ok())
    }

    #[test]
    fn invalid_requested_session_is_rejected() {
        assert!(SessionId::parse("../prod").is_err());
        assert!(SessionId::parse("").is_err());
    }

    #[test]
    fn requested_default_session_ignores_inherited_session_and_socket() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "work");
        env.set(EnvVar::SheprSocketPath, "/tmp/inherited.sock");
        let paths = shepr_config::AppPaths::resolve_with_session(Some(SessionId::Default))
            .expect("isolated paths resolve");

        assert_eq!(paths.session_id(), &SessionId::Default);
        assert_eq!(
            active_api_socket_path(&paths),
            paths.runtime_dir().join("shepr.sock")
        );
        assert_eq!(
            env_text(&env, EnvVar::SheprSession).as_deref(),
            Some("work"),
            "resolving a target must leave the parent environment unchanged"
        );
    }

    #[test]
    fn inherited_session_is_resolved_into_paths_once() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "env-session");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(paths.session_id().name(), Some("env-session"));
        assert_eq!(
            active_api_socket_path(&paths),
            paths
                .runtime_dir()
                .join("sessions")
                .join("env-session")
                .join("shepr.sock")
        );
    }

    #[test]
    fn session_files_and_sockets_use_separate_xdg_roots() {
        let (_env, paths) = isolated_config_env();
        let named = SessionId::parse("work").expect("valid name");
        assert_eq!(data_dir(&paths), paths.state_dir());
        assert_eq!(
            named.data_dir(&paths),
            paths.state_dir().join("sessions/work")
        );
        assert_eq!(
            named.api_socket_path(&paths),
            paths.runtime_dir().join("sessions/work/shepr.sock")
        );
        assert_eq!(
            named.client_socket_path(&paths),
            paths.runtime_dir().join("sessions/work/shepr-client.sock")
        );
    }

    #[test]
    fn inherited_default_session_name_resolves_to_default_identity() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, DEFAULT_SESSION_NAME);
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(paths.session_id(), &SessionId::Default);
        assert_eq!(
            active_api_socket_path(&paths),
            paths.runtime_dir().join("shepr.sock")
        );
        assert_eq!(
            env_text(&env, EnvVar::SheprSession).as_deref(),
            Some(DEFAULT_SESSION_NAME)
        );
    }

    #[test]
    fn session_commands_use_typed_identity() {
        let default = SessionId::Default;
        let named = SessionId::parse("work").expect("test precondition");

        assert_eq!(default.attach_command(), "shepr");
        assert_eq!(default.stop_command(), "shepr server stop");
        assert_eq!(named.attach_command(), "shepr session attach work");
        assert_eq!(named.stop_command(), "shepr session stop work");
    }

    const KEEP_GUIDANCE: &str = "To keep the running server and its panes, run this build in a session of its own: pass `--session <name>` with a name no running server uses.\nTo use this build here instead, stop the running server; stopping exits its pane processes.";

    #[test]
    fn restart_after_update_guidance_names_stop_and_attach_commands() {
        assert_eq!(
            restart_after_update_guidance(
                "shepr session stop work",
                Some("shepr session attach work")
            ),
            format!(
                "{KEEP_GUIDANCE} Run `shepr session stop work --force`, then run `shepr session attach work` again."
            )
        );
    }

    /// The non-destructive way out is offered first, and the stop it names is
    /// the forced one, since a mismatched stop is refused without the flag.
    #[test]
    fn restart_after_update_guidance_offers_a_separate_session_before_stopping() {
        let guidance = restart_after_update_guidance("shepr server stop", Some("shepr"));
        let session = guidance
            .find("--session <name>")
            .expect("the separate-session option is offered");
        let stop = guidance
            .find("shepr server stop --force")
            .expect("the forced stop is named");
        assert!(session < stop, "{guidance}");
        assert!(
            restart_after_update_guidance("shepr server stop", None)
                .contains("Run `shepr server stop --force`, then restart Shepr")
        );
    }

    #[test]
    fn restart_after_update_guidance_respects_socket_override() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, "/tmp/custom-shepr.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            restart_after_update_guidance_for(&paths),
            format!(
                "{KEEP_GUIDANCE} Run `SHEPR_SESSION=default SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr server stop --force`, then run `SHEPR_SESSION=default SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr` again."
            )
        );
    }

    #[test]
    fn restart_after_update_guidance_preserves_client_socket_override() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "work");
        env.set(EnvVar::SheprClientSocketPath, "/tmp/work-client.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            restart_after_update_guidance_for(&paths),
            format!(
                "{KEEP_GUIDANCE} Run `SHEPR_SESSION=work SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr server stop --force`, then run `SHEPR_SESSION=work SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr` again."
            )
        );
    }

    /// A build id that is not this build's.
    fn other_build() -> &'static str {
        if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
            "0000000000000000"
        } else {
            "ffffffffffffffff"
        }
    }

    #[test]
    fn a_mismatched_stop_is_refused_without_force_naming_both_builds() {
        let other = StopTargetBuild::from_running_build(other_build());
        let error = guard_mismatched_stop("the server", &other, false, "shepr server stop --force")
            .expect_err("a mismatched stop needs explicit intent");
        assert!(matches!(error, SessionError::BuildMismatch { .. }));
        let message = error.to_string();
        for expected in [
            other_build(),
            shepr_protocol::BUILD_ID,
            "`shepr server stop --force`",
            "--session <name>",
            "exits its pane processes",
        ] {
            assert!(message.contains(expected), "{expected}: {message}");
        }

        assert!(guard_mismatched_stop("the server", &other, true, "unused").is_ok());
        for passes in [
            StopTargetBuild::ThisBuild,
            StopTargetBuild::NotListening,
            StopTargetBuild::from_running_build(shepr_protocol::BUILD_ID),
        ] {
            assert!(guard_mismatched_stop("the server", &passes, false, "unused").is_ok());
        }
    }

    #[test]
    fn the_forced_session_stop_command_survives_a_dash_name() {
        assert_eq!(
            forced_session_stop_command("work"),
            "shepr session stop --force work"
        );
        assert_eq!(
            forced_session_stop_command("-work"),
            "shepr session stop --force -- -work"
        );
    }

    /// Serves `ping` with `build_id` at `socket_path` and records every
    /// request line, so a test can prove no `server.stop` was sent.
    fn serve_build(
        socket_path: &Path,
        build_id: &'static str,
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
                        if BufReader::new(reader).read_line(&mut request).is_err() {
                            continue;
                        }
                        if request.contains("ping") {
                            // A client may close before reading this reply; the test
                            // asserts on the requests recorded, not on the reply
                            // reaching every one of them.
                            drop(stream.write_all(
                                format!(
                                    "{{\"id\":\"runtime:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.0.0\",\"build_id\":\"{build_id}\"}}}}\n"
                                )
                                .as_bytes(),
                            ));
                        }
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
    fn server_stop_refuses_a_server_of_another_build_without_sending_stop() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().api_socket().to_path_buf();
        let (keep_running, handle) = serve_build(&socket_path, other_build());

        let error = stop_active_server(&paths, false).expect_err("a mismatched stop is refused");

        keep_running.store(false, Ordering::Relaxed);
        let requests = handle.join().expect("test precondition");
        assert!(
            matches!(&error, SessionError::BuildMismatch { running, force_command, .. }
                if running == other_build() && force_command == "shepr server stop --force"),
            "{error}"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("server.stop")),
            "{requests:?}"
        );
    }

    #[test]
    fn session_stop_refuses_a_server_of_another_build_without_sending_stop() {
        let (_env, paths) = isolated_config_env();
        let session = SessionId::parse("work").expect("test precondition");
        let socket_path = session.api_socket_path(&paths);
        let (keep_running, handle) = serve_build(&socket_path, other_build());

        let error =
            stop_session(&paths, &session, false).expect_err("a mismatched stop is refused");

        keep_running.store(false, Ordering::Relaxed);
        let requests = handle.join().expect("test precondition");
        assert!(
            matches!(&error, SessionError::BuildMismatch { label, force_command, .. }
                if label == "session work" && force_command == "shepr session stop --force work"),
            "{error}"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("server.stop")),
            "{requests:?}"
        );
    }

    #[test]
    fn the_stop_probe_reads_the_running_build() {
        let (_env, paths) = isolated_config_env();
        let socket_path = paths.server_address().api_socket().to_path_buf();
        assert_eq!(
            stop_target_build(&socket_path),
            StopTargetBuild::NotListening
        );

        let (keep_running, handle) = serve_build(&socket_path, shepr_protocol::BUILD_ID);
        let this_build = stop_target_build(&socket_path);
        keep_running.store(false, Ordering::Relaxed);
        handle.join().expect("test precondition");
        assert_eq!(this_build, StopTargetBuild::ThisBuild);
    }

    #[test]
    fn explicit_session_socket_ignores_inherited_socket_override() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "work");
        env.set(EnvVar::SheprSocketPath, "/tmp/inherited.sock");
        let paths = shepr_config::AppPaths::resolve_with_session(Some(
            SessionId::parse("work").expect("test precondition"),
        ))
        .expect("isolated paths resolve");
        let path = active_api_socket_path(&paths);

        assert_eq!(
            path,
            paths
                .runtime_dir()
                .join("sessions")
                .join("work")
                .join("shepr.sock")
        );
    }

    #[test]
    fn env_socket_override_wins_without_explicit_session() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "work");
        env.set(EnvVar::SheprSocketPath, "/tmp/explicit.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            active_api_socket_path(&paths),
            PathBuf::from("/tmp/explicit.sock")
        );
    }

    #[test]
    fn env_socket_override_does_not_excuse_invalid_env_session() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "bad/name");
        env.set(EnvVar::SheprSocketPath, "/tmp/shepr.sock");
        let errors = shepr_config::AppPaths::resolve()
            .expect_err("a malformed SHEPR_SESSION fails even with a socket override");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("session selection error")),
            "{errors:?}"
        );
        assert_eq!(
            env_text(&env, EnvVar::SheprSession).as_deref(),
            Some("bad/name")
        );
    }

    #[test]
    fn invalid_inherited_session_without_api_override_is_rejected() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "bad/name");

        let error = shepr_config::AppPaths::resolve().expect_err("invalid session name");

        assert!(error.join(" ").contains("session name may only contain"));
        assert_eq!(
            env_text(&env, EnvVar::SheprSession).as_deref(),
            Some("bad/name")
        );
    }

    #[test]
    fn stop_session_fails_when_socket_remains_reachable_after_timeout() {
        let (_env, _) = isolated_config_env();
        let session_name = "slow";
        let session = SessionId::parse(session_name).expect("test precondition");
        let paths = shepr_config::AppPaths::resolve_with_session(Some(session.clone()))
            .expect("isolated paths resolve");
        let socket_path = session.api_socket_path(&paths);
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
                        drop(stream.write_all(b"{\"id\":\"cli:session:stop\",\"result\":{}}\n"));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        // Forced: this fake answers every request with an empty result, not a
        // build, and the timeout is what is under test.
        let err = stop_session_with_timeout(&paths, &session, true, Duration::from_millis(75))
            .expect_err("still-running session should fail");

        assert!(
            matches!(&err, SessionError::TimedOut { reachable, .. } if reachable.contains(&socket_path)),
            "{err}"
        );
        keep_running.store(false, Ordering::Relaxed);
        handle.join().expect("test precondition");
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(validate_name("../prod").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("work session").is_err());
    }

    #[test]
    fn session_socket_liveness_maps_absent_and_stale_to_stopped() {
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
    fn parse_default_target_name_maps_to_default_session() {
        assert_eq!(
            parse_target_name(DEFAULT_SESSION_NAME).expect("test precondition"),
            SessionId::Default
        );
        assert_eq!(
            parse_target_name("work").expect("test precondition"),
            SessionId::parse("work").expect("test precondition")
        );
    }

    #[test]
    fn delete_default_session_is_rejected() {
        let scratch = ScratchDir::new("delete-default-session");
        let paths = shepr_config::AppPaths::test_at(scratch.path());
        assert!(delete_session(&paths, &SessionId::Default).is_err());
    }

    #[test]
    fn list_sessions_skips_reserved_default_directory() {
        let (_env, paths) = isolated_config_env();
        let sessions_dir = paths.state_dir().join("sessions");
        std::fs::create_dir_all(sessions_dir.join(DEFAULT_SESSION_NAME))
            .expect("test precondition");
        std::fs::create_dir_all(sessions_dir.join("work")).expect("test precondition");

        let sessions = list_sessions(&paths).expect("test precondition");
        let names: Vec<_> = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect();

        assert_eq!(names, vec![DEFAULT_SESSION_NAME, "work"]);
    }
}
