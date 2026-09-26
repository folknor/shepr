//! Auto-detect launch behavior for the `shepr` command.
//!
//! When the user runs `shepr` with no subcommand:
//! 1. Check if a server is already listening on the client socket
//! 2. If no server → spawn one as a background daemon → wait for socket readiness (up to 15s)
//! 3. Attach as a client to the server

use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tracing::info;

use super::socket_paths::client_socket_path;

/// Maximum time to wait for the server's client socket to become ready
/// after spawning the server process.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll interval when waiting for the server socket to appear.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Timeout for checking the stable JSON API before attaching to the binary protocol socket.
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Private daemon-start hint used to seed a fresh headless server from the
/// directory where the user ran `shepr`.
pub(crate) const STARTUP_CWD_ENV_VAR: &str = "SHEPR_STARTUP_CWD";

// ---------------------------------------------------------------------------
// Server detection
// ---------------------------------------------------------------------------

/// Checks whether a shepr server is currently listening on the client socket.
///
/// This works by attempting to connect to the client socket. If the connection
/// succeeds, a server is running. If the socket file doesn't exist or the
/// connection is refused, no server is running. Stale sockets (from a crashed
/// server) are detected because connect returns `ConnectionRefused`
/// when nobody is listening.
pub fn is_server_listening() -> bool {
    is_server_listening_at(&client_socket_path())
}

/// Checks whether a shepr server is listening at a specific socket path.
fn is_server_listening_at(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }

    match crate::ipc::connect_local_stream(socket_path) {
        Ok(_) => {
            // Server is listening. Close the test connection immediately.
            // The server's handshake handler will time out on this connection
            // since we don't send a handshake, which is fine.
            true
        }
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
            ) =>
        {
            // Socket file exists but nobody is listening - stale socket.
            false
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Socket file disappeared between exists() and connect().
            false
        }
        Err(err) => {
            // Other errors (permission denied, etc.) - assume not listening.
            tracing::warn!(err = %err, "unexpected error checking server socket");
            false
        }
    }
}

fn read_server_status() -> io::Result<Option<crate::api::RuntimeStatus>> {
    crate::api::read_runtime_status_at(&crate::api::socket_path(), STATUS_REQUEST_TIMEOUT)
}

fn validate_running_server_compatibility() -> io::Result<()> {
    let Some(status) = read_server_status()? else {
        return Err(io::Error::other(format!(
            "a shepr server is listening, but its status API is unavailable.\n\n{}\nIf that fails, stop the old server process manually.",
            crate::session::active_restart_after_update_guidance()
        )));
    };

    if status.protocol == Some(crate::protocol::PROTOCOL_VERSION) {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "the running shepr server is a different build; restart it before attaching.\n\nserver: v{} protocol {}\nclient: v{} protocol {}\n\n{}",
        status.version.as_deref().unwrap_or("unknown"),
        status
            .protocol
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unavailable".to_string()),
        crate::build_info::version(),
        crate::protocol::PROTOCOL_VERSION,
        crate::session::active_restart_after_update_guidance()
    )))
}

// ---------------------------------------------------------------------------
// Server spawning
// ---------------------------------------------------------------------------

/// Spawns the shepr server as a background daemon process.
///
/// The server process is fully detached:
/// - Runs in its own session (setsid) so it survives the client exiting
/// - Stdin/stdout/stderr are redirected to /dev/null
/// - Inherits relevant environment variables (`XDG_CONFIG_HOME`, `SHEPR_SESSION`,
///   socket overrides, etc.), except inherited socket overrides are cleared when
///   this CLI invocation explicitly selected a session.
///
/// Returns the PID of the spawned server process.
pub fn spawn_server_daemon() -> io::Result<u32> {
    // After an install replaces the binary, raw `current_exe()` names the
    // running one "/…/shepr (deleted)"; this resolves to the new install.
    let exe = crate::platform::launch_executable().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to determine shepr executable path: {err}"),
        )
    })?;

    info!(exe = %exe.display(), "spawning server daemon");

    let mut command = build_server_daemon_command(&exe);

    let pid = command.spawn().map(|child| child.id()).map_err(|err| {
        io::Error::new(err.kind(), format!("failed to spawn shepr server: {err}"))
    })?;
    info!(pid, "server daemon spawned");

    Ok(pid)
}

fn build_server_daemon_command(exe: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("server")
        // Redirect stdio to /dev/null
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::platform::detach_server_daemon_command(&mut command);

    match std::env::current_dir() {
        Ok(cwd) => {
            command.env(STARTUP_CWD_ENV_VAR, cwd);
        }
        Err(_) => {
            command.env_remove(STARTUP_CWD_ENV_VAR);
        }
    }

    if crate::session::explicit_session_requested() {
        command
            .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
            .env_remove("SHEPR_CLIENT_SOCKET_PATH");
    }

    command
}

// ---------------------------------------------------------------------------
// Socket readiness
// ---------------------------------------------------------------------------

/// Waits for the server's client socket to become ready for connections.
///
/// Polls the socket path at regular intervals until a connection succeeds
/// or the timeout elapses. Returns an error if the server doesn't become
/// ready within the timeout.
pub fn wait_for_server_socket(socket_path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;

    while std::time::Instant::now() < deadline {
        if is_server_listening_at(socket_path) {
            info!(path = %socket_path.display(), "server socket ready");
            return Ok(());
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "server did not become ready within {}s (socket: {}). The background server may still be starting; try `shepr` again, or check {}",
            timeout.as_secs(),
            socket_path.display(),
            crate::session::data_dir()
                .join("shepr-server.log")
                .display()
        ),
    ))
}

// ---------------------------------------------------------------------------
// Auto-detect launch
// ---------------------------------------------------------------------------

/// Performs auto-detect launch: check for server, spawn if needed, then
/// attach as a thin client.
///
/// This is the entry point called from `main.rs` when the user runs `shepr`
/// without a subcommand.
///
/// Flow:
/// 1. Check if a server is listening on the client socket
/// 2. If no server → spawn server daemon → wait for socket readiness
/// 3. Run the thin client (which connects to the server)
pub fn auto_detect_launch(
    saved_federation: bool,
    config: &crate::config::Config,
) -> io::Result<()> {
    // The client requires terminal geometry before it can attach. Reject an
    // unusable terminal before socket lookup creates directories or starts a daemon.
    crate::platform::terminal_grid_size().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot attach without a usable terminal: {err}; run inside a terminal"),
        )
    })?;
    let socket_path = client_socket_path();
    info!(path = %socket_path.display(), "auto-detect launch starting");

    // The running server is checked whether or not saved machines are
    // enabled. With saved machines a mismatch only downgrades to a warning
    // below, so they stay reachable; the Local endpoint's own handshake then
    // rejects the different build with the build-identity preamble error.
    let startup = if is_server_listening_at(&socket_path) {
        info!("server already running, attaching as client");
        validate_running_server_compatibility()
    } else {
        info!("no server running, spawning server daemon");
        spawn_server_daemon()
            .and_then(|_| wait_for_server_socket(&socket_path, SERVER_READY_TIMEOUT))
    };
    if let Err(error) = startup {
        if !saved_federation {
            return Err(error);
        }
        tracing::warn!(%error, "Local startup failed; keeping saved machines available");
    }

    // Now attach as a thin client.
    crate::client::run_client(config)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{IsolatedEnv, ScratchDir};
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    #[test]
    fn is_server_listening_returns_false_for_nonexistent_path() {
        let dir = ScratchDir::new("nonexistent");
        let path = dir.join("s.sock");
        assert!(!is_server_listening_at(&path));
    }

    #[test]
    fn server_daemon_command_clears_socket_overrides_for_explicit_session() {
        let env = IsolatedEnv::new();
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        env.set("SHEPR_CLIENT_SOCKET_PATH", "/tmp/inherited-client.sock");
        crate::session::configure(Some("work")).expect("test precondition");

        let command = build_server_daemon_command(&PathBuf::from("/tmp/shepr-test"));
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(crate::api::SOCKET_PATH_ENV_VAR) && value.is_none()
        }));
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new("SHEPR_CLIENT_SOCKET_PATH") && value.is_none()
        }));
    }

    #[test]
    fn server_daemon_command_passes_current_dir_as_startup_cwd() {
        let expected = std::env::current_dir().expect("test precondition");
        let command = build_server_daemon_command(&PathBuf::from("/tmp/shepr-test"));
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(STARTUP_CWD_ENV_VAR) && value == &Some(expected.as_os_str())
        }));
    }

    #[test]
    fn server_daemon_detach_creates_new_session() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            r#"sid=$(ps -o sid= -p $$ | tr -d ' ')
test "$sid" = "$$"
"#,
        );
        crate::platform::detach_server_daemon_command(&mut command);

        let status = command.status().expect("test precondition");
        assert!(
            status.success(),
            "detached server child should be its own session leader"
        );
    }

    #[test]
    fn is_server_listening_returns_true_for_live_socket() {
        let dir = ScratchDir::new("live");
        let path = dir.join("s.sock");

        let _listener = UnixListener::bind(&path).expect("test precondition");
        assert!(is_server_listening_at(&path));
    }

    #[test]
    fn is_server_listening_returns_false_for_stale_socket() {
        let dir = ScratchDir::new("stale");
        let path = dir.join("s.sock");

        // Create a socket and immediately drop the listener.
        // This leaves a stale socket file with nobody listening.
        {
            let _listener = UnixListener::bind(&path).expect("test precondition");
        }

        // The socket file exists but nobody is listening.
        assert!(!is_server_listening_at(&path));
    }

    #[test]
    fn is_server_listening_returns_false_when_listener_dropped() {
        let dir = ScratchDir::new("dropped");
        let path = dir.join("s.sock");

        // Bind and immediately drop the listener.
        drop(UnixListener::bind(&path).expect("test precondition"));

        // Socket is stale - should return false.
        assert!(!is_server_listening_at(&path));
    }

    #[test]
    fn wait_for_server_socket_succeeds_immediately() {
        let dir = ScratchDir::new("wait-ok");
        let path = dir.join("s.sock");

        let _listener = UnixListener::bind(&path).expect("test precondition");

        // Should succeed immediately (socket is already ready).
        let result = wait_for_server_socket(&path, Duration::from_millis(100));
        assert!(result.is_ok());
    }

    #[test]
    fn wait_for_server_socket_times_out() {
        let dir = ScratchDir::new("wait-timeout");
        let path = dir.join("s.sock");

        // No listener - should time out.
        let result = wait_for_server_socket(&path, Duration::from_millis(50));
        assert!(result.is_err());
        assert_eq!(
            result.expect_err("test precondition").kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn wait_for_server_socket_succeeds_after_delay() {
        let dir = ScratchDir::new("wait-delay");
        let path = dir.join("s.sock");

        // Spawn a thread that will create the listener after a short delay.
        let path_clone = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _listener = UnixListener::bind(&path_clone).expect("test precondition");
            // Keep the listener alive for a bit.
            std::thread::sleep(Duration::from_secs(1));
        });

        // Wait with a generous timeout - should succeed.
        let result = wait_for_server_socket(&path, Duration::from_secs(2));
        assert!(result.is_ok());
    }

    #[test]
    fn read_server_status_at_reads_ping_response() {
        let dir = ScratchDir::new("status");
        let path = dir.join("api.sock");
        let listener = UnixListener::bind(&path).expect("test precondition");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test precondition");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("test precondition"))
                .read_line(&mut request)
                .expect("test precondition");
            assert!(request.contains("ping"));
            stream
                .write_all(
                    b"{\"id\":\"autodetect:server:status\",\"result\":{\"type\":\"pong\",\"version\":\"0.5.5\",\"protocol\":2}}\n",
                )
                .expect("test precondition");
            stream.flush().expect("test precondition");
        });

        let status = crate::api::read_runtime_status_at(&path, Duration::from_millis(200))
            .expect("test precondition")
            .expect("test precondition");
        let _ = handle.join();
        assert_eq!(status.version.as_deref(), Some("0.5.5"));
        assert_eq!(status.protocol, Some(2));
    }

    #[test]
    fn validate_running_server_compatibility_fails_when_status_api_missing() {
        let env = IsolatedEnv::new();
        let path = env.path().join("api.sock");
        env.set(crate::api::SOCKET_PATH_ENV_VAR, &path);

        let err = validate_running_server_compatibility().expect_err("test precondition");

        assert!(
            err.to_string().contains("status API is unavailable"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_running_server_compatibility_names_session_commands_for_protocol_mismatch() {
        let env = IsolatedEnv::new();
        env.set("XDG_CONFIG_HOME", env.path());
        env.set(crate::session::SESSION_ENV_VAR, "work");
        let path = crate::session::api_socket_path_for(Some("work"));
        std::fs::create_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
        let listener = UnixListener::bind(&path).expect("test precondition");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test precondition");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("test precondition"))
                .read_line(&mut request)
                .expect("test precondition");
            assert!(request.contains("ping"));
            let body = format!(
                "{{\"id\":\"autodetect:server:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.5.5\",\"protocol\":{}}}}}\n",
                crate::protocol::PROTOCOL_VERSION + 1
            );
            stream
                .write_all(body.as_bytes())
                .expect("test precondition");
            stream.flush().expect("test precondition");
        });

        let err = validate_running_server_compatibility().expect_err("test precondition");
        let message = err.to_string();

        let _ = handle.join();
        assert!(
            message.contains("Stop the old server to use the new version"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("Run `shepr session stop work`"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("then run `shepr session attach work` again"),
            "unexpected error: {message}"
        );
    }
}
