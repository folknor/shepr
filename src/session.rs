use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;

use crate::ipc::LocalStream;

// Session management only connects to sockets (the API socket, to stop or
// probe a server); it never binds one. Binding goes through
// `ipc::bind_private_local_listener` in the server and API, and the peer check
// on accept is theirs, so nothing here needs the staged bind or `SO_PEERCRED`.

pub const SESSION_ENV_VAR: &str = "SHEPR_SESSION";
pub const DEFAULT_SESSION_NAME: &str = "default";

const MAX_SESSION_NAME_LEN: usize = 64;
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_WAIT_POLL: Duration = Duration::from_millis(25);
const MIN_SOCKET_TIMEOUT: Duration = Duration::from_millis(1);

static EXPLICIT_SESSION_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionInfo {
    pub name: String,
    pub default: bool,
    pub running: bool,
    pub socket_path: String,
    pub session_dir: String,
}

/// Selects the session for this process. `requested` is the session named on
/// the command line (`--session NAME` before the subcommand, or
/// `session attach NAME`); without one, an inherited socket override or
/// `SHEPR_SESSION` decides, and neither counts as an explicit request.
pub fn configure(requested: Option<&str>) -> Result<(), String> {
    if let Some(session) = requested {
        apply_explicit_name(session)?;
    } else if std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR).is_some() {
        EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
    } else if let Ok(session) = std::env::var(SESSION_ENV_VAR) {
        if normalize_name(&session)?.is_none() {
            // SAFETY: `main` calls `configure` before it starts any thread,
            // so nothing reads the environment concurrently; tests call it
            // while holding the crate-wide environment lock.
            unsafe { std::env::remove_var(SESSION_ENV_VAR) };
        }
        EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
    } else {
        EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
    }

    Ok(())
}

pub fn active_name() -> Option<String> {
    std::env::var(SESSION_ENV_VAR)
        .ok()
        .filter(|name| name != DEFAULT_SESSION_NAME)
        .filter(|name| validate_name(name).is_ok())
}

pub fn local_attach_command() -> String {
    match active_name() {
        Some(name) => format!("shepr session attach {name}"),
        None => "shepr".to_string(),
    }
}

pub fn local_stop_command() -> String {
    stop_command_for(active_name().as_deref())
}

pub fn stop_command_for(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("shepr session stop {name}"),
        None => "shepr server stop".to_string(),
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

pub fn active_restart_after_update_guidance() -> String {
    if !explicit_session_requested()
        && let Ok(socket_path) = std::env::var(crate::api::SOCKET_PATH_ENV_VAR)
    {
        return restart_after_update_guidance(
            &format!(
                "{}={} shepr server stop",
                crate::api::SOCKET_PATH_ENV_VAR,
                socket_path
            ),
            None,
        );
    }

    restart_after_update_guidance(&local_stop_command(), Some(&local_attach_command()))
}

pub fn explicit_session_requested() -> bool {
    EXPLICIT_SESSION_REQUESTED.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn clear_explicit_session_for_test() {
    EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
}

pub fn data_dir() -> PathBuf {
    data_dir_for(active_name().as_deref())
}

pub fn data_dir_for(name: Option<&str>) -> PathBuf {
    let config_dir = crate::config::config_dir();
    match name {
        Some(name) => config_dir.join("sessions").join(name),
        None => config_dir,
    }
}

pub fn api_socket_path_for(name: Option<&str>) -> PathBuf {
    data_dir_for(name).join("shepr.sock")
}

pub fn active_api_socket_path() -> PathBuf {
    if explicit_session_requested() {
        return api_socket_path_for(active_name().as_deref());
    }
    if let Ok(path) = std::env::var(crate::api::SOCKET_PATH_ENV_VAR) {
        return PathBuf::from(path);
    }
    api_socket_path_for(active_name().as_deref())
}

pub fn client_socket_path_for(name: Option<&str>) -> PathBuf {
    data_dir_for(name).join("shepr-client.sock")
}

pub fn list_sessions() -> std::io::Result<Vec<SessionInfo>> {
    let mut sessions = vec![session_info(None)];
    let sessions_dir = crate::config::config_dir().join("sessions");
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
        if name != DEFAULT_SESSION_NAME && validate_name(&name).is_ok() {
            names.push(name);
        }
    }
    names.sort();
    sessions.extend(names.iter().map(|name| session_info(Some(name))));
    Ok(sessions)
}

pub fn session_info(name: Option<&str>) -> SessionInfo {
    let default = name.is_none();
    let display_name = name.unwrap_or(DEFAULT_SESSION_NAME).to_string();
    let socket_path = api_socket_path_for(name);
    let session_dir = data_dir_for(name);
    SessionInfo {
        name: display_name,
        default,
        running: is_running_at(&socket_path),
        socket_path: socket_path.display().to_string(),
        session_dir: session_dir.display().to_string(),
    }
}

pub fn parse_target_name(name: &str) -> Result<Option<String>, String> {
    normalize_name(name)
}

pub fn stop_session(name: Option<&str>) -> Result<SessionInfo, String> {
    stop_session_with_timeout(name, STOP_WAIT_TIMEOUT)
}

pub(crate) fn stop_active_server() -> Result<(), String> {
    let socket_path = active_api_socket_path();
    let client_socket_path = crate::server::socket_paths::client_socket_path();
    stop_socket_with_timeout(
        &socket_path,
        &[socket_path.clone(), client_socket_path],
        STOP_WAIT_TIMEOUT,
        "server",
    )
}

fn stop_session_with_timeout(name: Option<&str>, timeout: Duration) -> Result<SessionInfo, String> {
    let socket_path = api_socket_path_for(name);
    let client_socket_path = client_socket_path_for(name);
    let label = format!("session {}", name.unwrap_or(DEFAULT_SESSION_NAME));
    stop_socket_with_timeout(
        &socket_path,
        &[socket_path.clone(), client_socket_path],
        timeout,
        &label,
    )?;
    Ok(session_info(name))
}

fn stop_socket_with_timeout(
    socket_path: &Path,
    stopped_socket_paths: &[PathBuf],
    timeout: Duration,
    label: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let request = serde_json::json!({
        "id": "cli:session:stop",
        "method": "server.stop",
        "params": {}
    });
    let stream = crate::ipc::connect_local_stream(socket_path).map_err(|err| {
        format!(
            "{label} is not running or cannot be reached at {}: {err}",
            socket_path.display()
        )
    })?;
    let stop_response = send_stop_request(stream, &request, deadline)?;
    if let Some(response) = stop_response
        && let Some(error) = response.get("error")
    {
        return Err(error.to_string());
    }
    if !wait_until_stopped_until(stopped_socket_paths, deadline) {
        let reachable = reachable_socket_paths(stopped_socket_paths);
        return Err(format!(
            "{label} did not stop within {}ms; sockets are still reachable at {}",
            timeout.as_millis(),
            reachable
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

pub fn delete_session(name: &str) -> Result<SessionInfo, String> {
    if name == DEFAULT_SESSION_NAME {
        return Err("deleting the default session is not supported".to_string());
    }
    validate_name(name)?;
    let Some(dir) = exact_session_dir_for_delete(name)? else {
        return Ok(session_info(Some(name)));
    };
    let socket_path = dir.join("shepr.sock");
    if is_running_at(&socket_path) {
        return Err(format!(
            "session {name} is running; stop it before deleting"
        ));
    }
    let info = session_info(Some(name));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(info),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(info),
        Err(err) => Err(err.to_string()),
    }
}

fn exact_session_dir_for_delete(name: &str) -> Result<Option<PathBuf>, String> {
    let sessions_dir = crate::config::config_dir().join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        if entry.file_name() == std::ffi::OsStr::new(name) {
            return Ok(Some(entry.path()));
        }
    }

    // A path lookup alone can resolve a different spelling on case-insensitive
    // filesystems. Never probe its socket or delete it without an exact entry.
    match std::fs::symlink_metadata(sessions_dir.join(name)) {
        Ok(_) => Err(format!(
            "session {name} does not match an exact session name; use the spelling shown by `shepr session list`"
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn send_stop_request(
    mut stream: LocalStream,
    request: &serde_json::Value,
    deadline: Instant,
) -> Result<Option<serde_json::Value>, String> {
    let Some(write_timeout) = socket_timeout_until(deadline) else {
        return Ok(None);
    };
    if let Err(err) = stream.set_send_timeout(Some(write_timeout))
        && !stop_timeout_error_allows_wait(&err)
    {
        return Err(err.to_string());
    }

    let response = send_stop_request_inner(&mut stream, request, deadline);
    match response {
        Ok(Some(line)) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|err| err.to_string()),
        Ok(None) => Ok(None),
        Err(err) if stop_request_error_allows_wait(&err) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn send_stop_request_inner(
    stream: &mut LocalStream,
    request: &serde_json::Value,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    stream.write_all(request.to_string().as_bytes())?;
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

fn is_running_at(socket_path: &Path) -> bool {
    socket_path.exists() && crate::ipc::connect_local_stream(socket_path).is_ok()
}

fn wait_until_stopped_until(socket_paths: &[PathBuf], deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if socket_paths.iter().all(|path| !is_running_at(path)) {
            return true;
        }
        std::thread::sleep(STOP_WAIT_POLL.min(time_until(deadline)));
    }
    socket_paths.iter().all(|path| !is_running_at(path))
}

fn reachable_socket_paths(socket_paths: &[PathBuf]) -> Vec<PathBuf> {
    socket_paths
        .iter()
        .filter(|path| is_running_at(path))
        .cloned()
        .collect()
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

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name cannot be empty".to_string());
    }
    if name.len() > MAX_SESSION_NAME_LEN {
        return Err(format!(
            "session name cannot be longer than {MAX_SESSION_NAME_LEN} bytes"
        ));
    }
    if name == "." || name == ".." {
        return Err("session name cannot be . or ..".to_string());
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(
            "session name may only contain ASCII letters, numbers, '.', '_' and '-'".to_string(),
        );
    }
    Ok(())
}

fn apply_explicit_name(name: &str) -> Result<(), String> {
    let session = normalize_name(name)?;
    if let Some(session) = session {
        // SAFETY: only `configure` calls this, and `main` calls `configure`
        // before it starts any thread; tests call it while holding the
        // crate-wide environment lock.
        unsafe { std::env::set_var(SESSION_ENV_VAR, session) };
    } else {
        // SAFETY: as above.
        unsafe { std::env::remove_var(SESSION_ENV_VAR) };
    }
    EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);
    Ok(())
}

fn normalize_name(name: &str) -> Result<Option<String>, String> {
    if name == DEFAULT_SESSION_NAME {
        return Ok(None);
    }
    validate_name(name)?;
    Ok(Some(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{IsolatedEnv, ScratchDir};
    use interprocess::local_socket::traits::Listener as _;

    /// A connected socket pair; the socket file lives in the returned scratch
    /// directory.
    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, ScratchDir) {
        let scratch = ScratchDir::new(name);
        let path = scratch.join("s.sock");
        let listener = crate::ipc::bind_local_listener(&path).expect("test precondition");
        let client = crate::ipc::connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        (client, server, scratch)
    }

    /// An isolated environment whose config directory is `config` inside the
    /// test's scratch directory.
    fn isolated_config_env() -> (IsolatedEnv, PathBuf) {
        let env = IsolatedEnv::new();
        let config_home = env.path().join("config");
        env.set("XDG_CONFIG_HOME", &config_home);
        (env, config_home)
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
        let request = serde_json::json!({
            "id": "cli:session:stop",
            "method": "server.stop",
            "params": {}
        });

        assert_eq!(
            send_stop_request(
                client,
                &request,
                Instant::now() + Duration::from_millis(100)
            )
            .expect("test precondition"),
            None
        );
        assert!(
            handle
                .join()
                .expect("test precondition")
                .contains("server.stop")
        );
    }

    #[test]
    fn stop_session_times_out_when_socket_stays_open_without_response() {
        let (_env, _config_home) = isolated_config_env();
        let session_name = "silent";
        let socket_path = api_socket_path_for(Some(session_name));
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

        let err = stop_session_with_timeout(Some(session_name), Duration::from_millis(75))
            .expect_err("silent session should fail after timeout");

        assert!(err.contains("did not stop"), "{err}");
        keep_running.store(false, Ordering::Relaxed);
        handle.join().expect("test precondition");
    }

    // Which argv words name the session is the parser's job (tests in
    // `cli.rs`); these cover what a requested session does to the process.

    #[test]
    fn configure_applies_requested_session() {
        let _env = IsolatedEnv::new();

        configure(Some("work")).expect("test precondition");

        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("work"));
        assert!(explicit_session_requested());
    }

    #[test]
    fn configure_rejects_invalid_requested_session() {
        let _env = IsolatedEnv::new();

        assert!(configure(Some("../prod")).is_err());
        assert!(configure(Some("")).is_err());
        assert!(std::env::var(SESSION_ENV_VAR).is_err());
        assert!(!explicit_session_requested());
    }

    #[test]
    fn configure_requested_session_overrides_inherited_env_and_socket() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "bad/name");
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");

        configure(Some("work")).expect("test precondition");

        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("work"));
        assert!(explicit_session_requested());
    }

    #[test]
    fn configure_maps_default_session_name_to_default_path() {
        let (env, config_home) = isolated_config_env();
        env.set(SESSION_ENV_VAR, "work");
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");

        configure(Some(DEFAULT_SESSION_NAME)).expect("test precondition");

        assert!(std::env::var(SESSION_ENV_VAR).is_err());
        assert!(explicit_session_requested());
        assert_eq!(
            active_api_socket_path(),
            config_home
                .join(crate::config::app_dir_name())
                .join("shepr.sock")
        );
    }

    #[test]
    fn env_session_does_not_mark_session_explicit() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "env-session");
        EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);

        configure(None).expect("test precondition");

        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("env-session"));
        assert!(!explicit_session_requested());
    }

    #[test]
    fn env_default_session_name_uses_default_path() {
        let (env, config_home) = isolated_config_env();
        env.set(SESSION_ENV_VAR, DEFAULT_SESSION_NAME);
        EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);

        configure(None).expect("test precondition");

        assert!(std::env::var(SESSION_ENV_VAR).is_err());
        assert!(!explicit_session_requested());
        assert_eq!(
            active_api_socket_path(),
            config_home
                .join(crate::config::app_dir_name())
                .join("shepr.sock")
        );
    }

    #[test]
    fn local_attach_command_uses_default_launch_for_default_session() {
        let _env = IsolatedEnv::new();

        assert_eq!(local_attach_command(), "shepr");
    }

    #[test]
    fn local_attach_command_uses_session_attach_for_named_session() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "work");

        assert_eq!(local_attach_command(), "shepr session attach work");
    }

    #[test]
    fn local_stop_command_uses_server_stop_for_default_session() {
        let _env = IsolatedEnv::new();

        assert_eq!(local_stop_command(), "shepr server stop");
    }

    #[test]
    fn local_stop_command_uses_session_stop_for_named_session() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "work");

        assert_eq!(local_stop_command(), "shepr session stop work");
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
    fn active_restart_after_update_guidance_respects_socket_override() {
        let env = IsolatedEnv::new();
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/custom-shepr.sock");

        assert_eq!(
            active_restart_after_update_guidance(),
            "Stop the old server to use the new version.\nStopping exits pane processes.\nRun `SHEPR_SOCKET_PATH=/tmp/custom-shepr.sock shepr server stop`, then restart Shepr with the same socket override."
        );
    }

    #[test]
    fn explicit_session_socket_ignores_inherited_socket_override() {
        let (env, config_home) = isolated_config_env();
        env.set(SESSION_ENV_VAR, "work");
        EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");

        let path = active_api_socket_path();

        assert_eq!(
            path,
            config_home
                .join(crate::config::app_dir_name())
                .join("sessions")
                .join("work")
                .join("shepr.sock")
        );
    }

    #[test]
    fn env_socket_override_wins_without_explicit_session() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "work");
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/explicit.sock");

        assert_eq!(
            active_api_socket_path(),
            PathBuf::from("/tmp/explicit.sock")
        );
    }

    #[test]
    fn env_socket_override_skips_invalid_env_session_validation_without_explicit_session() {
        let env = IsolatedEnv::new();
        env.set(SESSION_ENV_VAR, "bad/name");
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/shepr.sock");

        configure(None).expect("test precondition");

        assert!(!explicit_session_requested());
        assert_eq!(active_api_socket_path(), PathBuf::from("/tmp/shepr.sock"));
        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("bad/name"));
    }

    #[test]
    fn stop_session_fails_when_socket_remains_reachable_after_timeout() {
        let (_env, _config_home) = isolated_config_env();
        let session_name = "slow";
        let socket_path = api_socket_path_for(Some(session_name));
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

        let err = stop_session_with_timeout(Some(session_name), Duration::from_millis(75))
            .expect_err("still-running session should fail");

        assert!(err.contains("did not stop"), "{err}");
        assert!(
            err.contains(socket_path.to_string_lossy().as_ref()),
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
    fn parse_default_target_name_maps_to_default_session() {
        assert_eq!(
            parse_target_name(DEFAULT_SESSION_NAME).expect("test precondition"),
            None
        );
        assert_eq!(
            parse_target_name("work").expect("test precondition"),
            Some("work".to_string())
        );
    }

    #[test]
    fn delete_default_session_is_rejected() {
        assert!(delete_session(DEFAULT_SESSION_NAME).is_err());
    }

    #[test]
    fn list_sessions_skips_reserved_default_directory() {
        let (_env, config_home) = isolated_config_env();
        let sessions_dir = config_home
            .join(crate::config::app_dir_name())
            .join("sessions");
        std::fs::create_dir_all(sessions_dir.join(DEFAULT_SESSION_NAME))
            .expect("test precondition");
        std::fs::create_dir_all(sessions_dir.join("work")).expect("test precondition");

        let sessions = list_sessions().expect("test precondition");
        let names: Vec<_> = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect();

        assert_eq!(names, vec![DEFAULT_SESSION_NAME, "work"]);
    }
}
