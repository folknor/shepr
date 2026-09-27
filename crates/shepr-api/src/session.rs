use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;

use shepr_platform::ipc::LocalStream;

// Session management only connects to sockets (the API socket, to stop or
// probe a server); it never binds one. Binding goes through
// `ipc::bind_private_local_listener` in the server and API, and the peer check
// on accept is theirs, so nothing here needs the staged bind or `SO_PEERCRED`.

use shepr_config::DEFAULT_SESSION_NAME;
#[cfg(test)]
use shepr_config::SESSION_ENV_VAR;
use shepr_config::{SessionId, SessionName, SessionNameError};

const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_WAIT_POLL: Duration = Duration::from_millis(25);
const MIN_SOCKET_TIMEOUT: Duration = Duration::from_millis(1);

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
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(message) => f.write_str(message),
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
            Self::Protocol(message) => f.write_str(message),
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

pub fn restart_after_update_guidance(stop_command: &str, attach_command: Option<&str>) -> String {
    let restart = match attach_command {
        Some(command) => format!("Run `{stop_command}`, then run `{command}` again."),
        None => format!("Run `{stop_command}`, then restart Shepr with the same socket override."),
    };
    format!(
        "Stop the old server to use the new version.\nStopping exits pane processes.\n{restart}"
    )
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

pub fn data_dir_for(paths: &shepr_config::AppPaths, session: &SessionId) -> PathBuf {
    session.data_dir(paths)
}

pub fn sessions_dir(paths: &shepr_config::AppPaths) -> PathBuf {
    sessions_dir_under(paths.state_dir())
}

fn sessions_dir_under(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions")
}

pub fn api_socket_path_for(paths: &shepr_config::AppPaths, session: &SessionId) -> PathBuf {
    session.api_socket_path(paths)
}

pub fn active_api_socket_path(paths: &shepr_config::AppPaths) -> PathBuf {
    paths.server_address().api_socket().to_path_buf()
}

pub fn client_socket_path_for(paths: &shepr_config::AppPaths, session: &SessionId) -> PathBuf {
    session.client_socket_path(paths)
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
    let socket_path = api_socket_path_for(paths, session);
    let session_dir = data_dir_for(paths, session);
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

pub fn stop_session(
    paths: &shepr_config::AppPaths,
    session: &SessionId,
) -> Result<SessionInfo, SessionError> {
    stop_session_with_timeout(paths, session, STOP_WAIT_TIMEOUT)
}

pub fn stop_active_server(paths: &shepr_config::AppPaths) -> Result<(), SessionError> {
    let address = paths.server_address();
    let socket_path = address.api_socket().to_path_buf();
    let client_socket_path = address.client_socket().to_path_buf();
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
    timeout: Duration,
) -> Result<SessionInfo, SessionError> {
    let socket_path = api_socket_path_for(paths, session);
    let client_socket_path = client_socket_path_for(paths, session);
    let label = format!("session {}", session.display_name());
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
    let deadline = Instant::now() + timeout;
    let request = server_stop_request("cli:session:stop");
    let stream = match shepr_platform::ipc::connect_local_stream(socket_path) {
        Ok(stream) => stream,
        Err(error) => {
            return Err(match shepr_platform::ipc::probe(socket_path) {
                shepr_platform::ipc::Liveness::Absent | shepr_platform::ipc::Liveness::Stale => {
                    SessionError::NotRunning {
                        label: label.into(),
                        path: socket_path.into(),
                        source: error,
                    }
                }
                shepr_platform::ipc::Liveness::Live
                | shepr_platform::ipc::Liveness::Unreachable(_) => SessionError::Unreachable {
                    label: label.into(),
                    path: socket_path.into(),
                    source: error,
                },
            });
        }
    };
    let stop_response = send_stop_request(stream, &request, deadline)?;
    if let Some(response) = stop_response
        && let Some(error) = response.get("error")
    {
        return Err(SessionError::Protocol(error.to_string()));
    }
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
    let socket_path = api_socket_path_for(paths, session);
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
    mut stream: LocalStream,
    request: &crate::schema::Request,
    deadline: Instant,
) -> Result<Option<serde_json::Value>, SessionError> {
    let Some(write_timeout) = socket_timeout_until(deadline) else {
        return Ok(None);
    };
    if let Err(err) = stream.set_send_timeout(Some(write_timeout))
        && !stop_timeout_error_allows_wait(&err)
    {
        return Err(err.into());
    }

    let request =
        serde_json::to_vec(request).map_err(|err| SessionError::Protocol(err.to_string()))?;
    let response = send_stop_request_inner(&mut stream, &request, deadline);
    match response {
        Ok(Some(line)) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|err| SessionError::Protocol(err.to_string())),
        Ok(None) => Ok(None),
        Err(err) if stop_request_error_allows_wait(&err) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn send_stop_request_inner(
    stream: &mut LocalStream,
    request: &[u8],
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    stream.write_all(request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let Some(read_timeout) = socket_timeout_until(deadline) else {
        return Ok(None);
    };
    if let Err(err) = stream.set_recv_timeout(Some(read_timeout)) {
        if stop_timeout_error_allows_wait(&err) {
            return Ok(None);
        }
        return Err(err);
    }

    let mut line = String::new();
    let bytes_read = BufReader::new(stream).read_line(&mut line)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

fn server_stop_request(id: &str) -> crate::schema::Request {
    crate::schema::Request {
        id: id.into(),
        method: crate::schema::Method::ServerStop(crate::schema::EmptyParams::default()),
    }
}

fn stop_timeout_error_allows_wait(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::InvalidInput
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
    while Instant::now() < deadline {
        if all_sockets_stopped(socket_paths)? {
            return Ok(true);
        }
        std::thread::sleep(STOP_WAIT_POLL.min(time_until(deadline)));
    }
    all_sockets_stopped(socket_paths)
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

fn time_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn socket_timeout_until(deadline: Instant) -> Option<Duration> {
    socket_timeout_from_remaining(time_until(deadline))
}

fn socket_timeout_from_remaining(remaining: Duration) -> Option<Duration> {
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.max(MIN_SOCKET_TIMEOUT))
}

pub fn validate_name(name: &str) -> Result<(), SessionError> {
    shepr_config::validate_session_name(name)
        .map_err(|SessionNameError(message)| SessionError::InvalidName(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use shepr_test_support::{IsolatedEnv, ScratchDir};
    use std::sync::atomic::Ordering;

    /// A connected socket pair; the socket file lives in the returned scratch
    /// directory.
    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, ScratchDir) {
        let scratch = ScratchDir::new(name);
        let path = scratch.join("s.sock");
        let listener = shepr_platform::ipc::bind_local_listener(&path).expect("test precondition");
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        (client, server, scratch)
    }

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
    fn stop_timeout_invalid_input_waits_for_socket_state() {
        let err = std::io::Error::from(std::io::ErrorKind::InvalidInput);

        assert!(stop_timeout_error_allows_wait(&err));
    }

    #[test]
    fn socket_timeouts_are_never_zero_duration() {
        assert_eq!(socket_timeout_from_remaining(Duration::ZERO), None);
        assert_eq!(
            socket_timeout_from_remaining(Duration::from_nanos(1)),
            Some(MIN_SOCKET_TIMEOUT)
        );
        assert_eq!(
            socket_timeout_from_remaining(Duration::from_millis(10)),
            Some(Duration::from_millis(10))
        );
    }

    #[test]
    fn stop_request_empty_response_waits_for_socket_state() {
        let (client, server, _scratch) = local_stream_pair("stop-empty");
        let handle = std::thread::spawn(move || {
            let mut request = String::new();
            let _ = BufReader::new(server).read_line(&mut request);
            request
        });
        let request = server_stop_request("cli:session:stop");

        assert_eq!(
            send_stop_request(
                client,
                &request,
                Instant::now() + Duration::from_millis(100)
            )
            .expect("test precondition"),
            None
        );
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
        let socket_path = api_socket_path_for(&paths, &session);
        std::fs::create_dir_all(socket_path.parent().expect("test precondition"))
            .expect("test precondition");
        let _ = std::fs::remove_file(&socket_path);
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
                                Ok(0) => continue,
                                Ok(_) if request.contains("server.stop") => {
                                    held_streams.push(stream);
                                }
                                Ok(_) => {}
                                Err(_) => continue,
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

        let err = stop_session_with_timeout(&paths, &session, Duration::from_millis(75))
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
        env.set(SESSION_ENV_VAR, "inherited");
        env.set(shepr_config::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        let requested = SessionId::parse("work").expect("test precondition");

        let (resolved, explicit) = SessionId::resolve(Some(requested.clone()), Some("bad/name"))
            .expect("explicit selection wins");

        assert_eq!(resolved, requested);
        assert!(explicit);
        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("inherited"));
        assert_eq!(
            std::env::var(shepr_config::SOCKET_PATH_ENV_VAR).as_deref(),
            Ok("/tmp/inherited.sock")
        );
    }

    #[test]
    fn invalid_requested_session_is_rejected() {
        assert!(SessionId::parse("../prod").is_err());
        assert!(SessionId::parse("").is_err());
    }

    #[test]
    fn requested_default_session_ignores_inherited_session_and_socket() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "work");
        env.set(shepr_config::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        let paths = shepr_config::AppPaths::resolve_with_session(Some(SessionId::Default))
            .expect("isolated paths resolve");

        assert_eq!(paths.session_id(), &SessionId::Default);
        assert_eq!(
            active_api_socket_path(&paths),
            paths.runtime_dir().join("shepr.sock")
        );
        assert_eq!(
            std::env::var(SESSION_ENV_VAR).as_deref(),
            Ok("work"),
            "resolving a target must leave the parent environment unchanged"
        );
    }

    #[test]
    fn inherited_session_is_resolved_into_paths_once() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "env-session");
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
            data_dir_for(&paths, &named),
            paths.state_dir().join("sessions/work")
        );
        assert_eq!(
            api_socket_path_for(&paths, &named),
            paths.runtime_dir().join("sessions/work/shepr.sock")
        );
        assert_eq!(
            client_socket_path_for(&paths, &named),
            paths.runtime_dir().join("sessions/work/shepr-client.sock")
        );
    }

    #[test]
    fn inherited_default_session_name_resolves_to_default_identity() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, DEFAULT_SESSION_NAME);
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(paths.session_id(), &SessionId::Default);
        assert_eq!(
            active_api_socket_path(&paths),
            paths.runtime_dir().join("shepr.sock")
        );
        assert_eq!(
            std::env::var(SESSION_ENV_VAR).as_deref(),
            Ok(DEFAULT_SESSION_NAME)
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

    #[test]
    fn restart_after_update_guidance_names_stop_and_attach_commands() {
        assert_eq!(
            restart_after_update_guidance(
                "shepr session stop work",
                Some("shepr session attach work")
            ),
            "Stop the old server to use the new version.\nStopping exits pane processes.\nRun `shepr session stop work`, then run `shepr session attach work` again."
        );
    }

    #[test]
    fn restart_after_update_guidance_respects_socket_override() {
        let env = IsolatedEnv::new();
        env.set(shepr_config::SOCKET_PATH_ENV_VAR, "/tmp/custom-shepr.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            restart_after_update_guidance_for(&paths),
            "Stop the old server to use the new version.\nStopping exits pane processes.\nRun `SHEPR_SESSION=default SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr server stop`, then run `SHEPR_SESSION=default SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr` again."
        );
    }

    #[test]
    fn restart_after_update_guidance_preserves_client_socket_override() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "work");
        env.set(
            shepr_config::CLIENT_SOCKET_PATH_ENV_VAR,
            "/tmp/work-client.sock",
        );
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            restart_after_update_guidance_for(&paths),
            "Stop the old server to use the new version.\nStopping exits pane processes.\nRun `SHEPR_SESSION=work SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr server stop`, then run `SHEPR_SESSION=work SHEPR_CLIENT_SOCKET_PATH=/tmp/work-client.sock shepr` again."
        );
    }

    #[test]
    fn explicit_session_socket_ignores_inherited_socket_override() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "work");
        env.set(shepr_config::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
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
        env.set(SESSION_ENV_VAR, "work");
        env.set(shepr_config::SOCKET_PATH_ENV_VAR, "/tmp/explicit.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(
            active_api_socket_path(&paths),
            PathBuf::from("/tmp/explicit.sock")
        );
    }

    #[test]
    fn env_socket_override_does_not_excuse_invalid_env_session() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "bad/name");
        env.set(shepr_config::SOCKET_PATH_ENV_VAR, "/tmp/shepr.sock");
        let errors = shepr_config::AppPaths::resolve()
            .expect_err("a malformed SHEPR_SESSION fails even with a socket override");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("session selection error")),
            "{errors:?}"
        );
        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("bad/name"));
    }

    #[test]
    fn invalid_inherited_session_without_api_override_is_rejected() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "bad/name");

        let error = shepr_config::AppPaths::resolve().expect_err("invalid session name");

        assert!(error.join(" ").contains("session name may only contain"));
        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("bad/name"));
    }

    #[test]
    fn stop_session_fails_when_socket_remains_reachable_after_timeout() {
        let (_env, _) = isolated_config_env();
        let session_name = "slow";
        let session = SessionId::parse(session_name).expect("test precondition");
        let paths = shepr_config::AppPaths::resolve_with_session(Some(session.clone()))
            .expect("isolated paths resolve");
        let socket_path = api_socket_path_for(&paths, &session);
        std::fs::create_dir_all(socket_path.parent().expect("test precondition"))
            .expect("test precondition");
        let _ = std::fs::remove_file(&socket_path);
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
                                Ok(0) => continue,
                                Ok(_) if request.trim().is_empty() => continue,
                                Ok(_) => {}
                                Err(_) => continue,
                            }
                        }
                        let _ = stream.write_all(b"{\"id\":\"cli:session:stop\",\"result\":{}}\n");
                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let err = stop_session_with_timeout(&paths, &session, Duration::from_millis(75))
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
