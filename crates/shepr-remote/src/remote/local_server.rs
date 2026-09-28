//! Shared local server startup and build checks for direct and SSH clients.

use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use shepr_core::env::EnvVar;
use tracing::info;

fn client_socket_path(paths: &shepr_config::AppPaths) -> std::path::PathBuf {
    paths.server_address().client_socket().to_path_buf()
}

/// Poll interval when waiting for the server socket to appear.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Timeout for checking the stable JSON API before attaching to the binary protocol socket.
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Server detection
// ---------------------------------------------------------------------------

/// Checks whether a shepr server is currently listening on the client socket.
///
/// This works by attempting to connect to the client socket. If the connection
/// succeeds, a server is running. If the socket path is missing or the
/// connection is refused, no server is running. Other errors are returned so
/// an inaccessible socket is not mistaken for permission to start another
/// daemon.
pub fn is_server_listening(paths: &shepr_config::AppPaths) -> io::Result<bool> {
    is_server_listening_at(&client_socket_path(paths))
}

/// Checks whether a shepr server is listening at a specific socket path.
fn is_server_listening_at(socket_path: &Path) -> io::Result<bool> {
    match shepr_platform::ipc::connect_local_stream(socket_path) {
        Ok(stream) => {
            // Its preamble write fails or its handshake reader sees EOF, so the
            // probe does not occupy the server until the handshake deadline.
            drop(stream);
            Ok(true)
        }
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Ok(false)
        }
        Err(err) => {
            tracing::warn!(path = %socket_path.display(), %err, "failed to check server socket");
            Err(err)
        }
    }
}

fn read_server_status(
    paths: &shepr_config::AppPaths,
) -> io::Result<Option<shepr_api::RuntimeStatus>> {
    shepr_api::read_runtime_status_at(&shepr_api::socket_path(paths), STATUS_REQUEST_TIMEOUT)
}

pub fn validate_running_server_compatibility(paths: &shepr_config::AppPaths) -> io::Result<()> {
    let Some(status) = read_server_status(paths)? else {
        return Err(io::Error::other(format!(
            "a shepr server is listening, but its status API is unavailable.\n\n{}\nIf that fails, stop the old server process manually.",
            shepr_api::session::restart_after_update_guidance_for(paths)
        )));
    };

    if status.build_id == shepr_protocol::BUILD_ID {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "the running shepr server is a different build; restart it before attaching.\n\nserver: v{} build {}\nclient: v{} build {}\n\n{}",
        status.version.as_deref().unwrap_or("unknown"),
        status.build_id,
        shepr_protocol::build_version(),
        shepr_protocol::BUILD_ID,
        shepr_api::session::restart_after_update_guidance_for(paths)
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
/// - Inherits the surrounding environment and gets the already-resolved
///   session and socket target on its child command, including removals for
///   inherited overrides that were superseded by an explicit session.
///
/// Returns the PID of the spawned server process.
pub fn spawn_server_daemon(paths: &shepr_config::AppPaths) -> io::Result<u32> {
    // After an install replaces the binary, raw `current_exe()` names the
    // running one "/…/shepr (deleted)"; this resolves to the new install.
    let exe = shepr_platform::launch_executable().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to determine shepr executable path: {err}"),
        )
    })?;

    info!(exe = %exe.display(), "spawning server daemon");

    let mut command = build_server_daemon_command(&exe, paths.current_dir(), paths);

    let pid = command.spawn().map(|child| child.id()).map_err(|err| {
        io::Error::new(err.kind(), format!("failed to spawn shepr server: {err}"))
    })?;
    info!(pid, "server daemon spawned");

    Ok(pid)
}

fn build_server_daemon_command(
    exe: &Path,
    startup_cwd: Option<&Path>,
    paths: &shepr_config::AppPaths,
) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("server")
        // Redirect stdio to /dev/null
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    shepr_platform::detach_server_daemon_command(&mut command);

    // A private daemon-start hint that seeds a fresh headless server from the
    // directory where the user ran `shepr`.
    if let Some(startup_cwd) = startup_cwd {
        command.env(EnvVar::SheprStartupCwd, startup_cwd);
    } else {
        command.env_remove(EnvVar::SheprStartupCwd);
    }

    paths.session_id().apply_to_child_command(&mut command);
    paths.server_address().apply_to_child_command(&mut command);

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
pub fn wait_for_server_socket(
    socket_path: &Path,
    timeout: Duration,
    paths: &shepr_config::AppPaths,
) -> io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;

    while std::time::Instant::now() < deadline {
        if is_server_listening_at(socket_path)? {
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
            shepr_api::session::data_dir(paths)
                .join("shepr-server.log")
                .display()
        ),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::{IsolatedEnv, ScratchDir};
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    #[test]
    fn is_server_listening_returns_false_for_nonexistent_path() {
        let dir = ScratchDir::new("nonexistent");
        let path = dir.join("s.sock");
        assert!(!is_server_listening_at(&path).expect("socket lookup succeeds"));
    }

    #[test]
    fn is_server_listening_returns_permission_errors_instead_of_false() {
        // SAFETY: geteuid takes no arguments, cannot fail and touches no memory.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let dir = ScratchDir::new("inaccessible");
        let parent = dir.join("private");
        std::fs::create_dir(&parent).expect("create inaccessible directory");
        let mut permissions = std::fs::metadata(&parent)
            .expect("read inaccessible directory metadata")
            .permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&parent, permissions).expect("restrict directory permissions");

        let path = parent.join("s.sock");
        let result = is_server_listening_at(&path);

        let mut permissions = std::fs::metadata(&parent)
            .expect("read inaccessible directory metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&parent, permissions).expect("restore directory permissions");
        assert_eq!(
            result
                .expect_err("permission errors must not mean no server")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn server_daemon_command_clears_socket_overrides_for_explicit_session() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, "/tmp/inherited.sock");
        env.set(EnvVar::SheprClientSocketPath, "/tmp/inherited-client.sock");
        let session = shepr_config::SessionId::parse("work").expect("test precondition");
        let paths = shepr_config::AppPaths::resolve_with_session(Some(session))
            .expect("isolated paths resolve");

        let command = build_server_daemon_command(
            &PathBuf::from("/tmp/shepr-test"),
            Some(Path::new("/home/test")),
            &paths,
        );
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(EnvVar::SheprSocketPath.name()) && value.is_none()
        }));
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(EnvVar::SheprClientSocketPath.name()) && value.is_none()
        }));
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(EnvVar::SheprSession.name()) && value == &Some(OsStr::new("work"))
        }));
    }

    #[test]
    fn server_daemon_command_passes_current_dir_as_startup_cwd() {
        let expected = Path::new("/home/test");
        let paths = shepr_config::AppPaths::default();
        let command =
            build_server_daemon_command(&PathBuf::from("/tmp/shepr-test"), Some(expected), &paths);
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(EnvVar::SheprStartupCwd.name())
                && value == &Some(expected.as_os_str())
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
        shepr_platform::detach_server_daemon_command(&mut command);

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
        assert!(is_server_listening_at(&path).expect("socket lookup succeeds"));
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
        assert!(!is_server_listening_at(&path).expect("socket lookup succeeds"));
    }

    #[test]
    fn is_server_listening_returns_false_when_listener_dropped() {
        let dir = ScratchDir::new("dropped");
        let path = dir.join("s.sock");

        // Bind and immediately drop the listener.
        drop(UnixListener::bind(&path).expect("test precondition"));

        // Socket is stale - should return false.
        assert!(!is_server_listening_at(&path).expect("socket lookup succeeds"));
    }

    #[test]
    fn wait_for_server_socket_succeeds_immediately() {
        let dir = ScratchDir::new("wait-ok");
        let path = dir.join("s.sock");

        let _listener = UnixListener::bind(&path).expect("test precondition");

        // Should succeed immediately (socket is already ready).
        let result = wait_for_server_socket(
            &path,
            Duration::from_millis(100),
            &shepr_config::AppPaths::test_at(dir.path()),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn wait_for_server_socket_times_out() {
        let dir = ScratchDir::new("wait-timeout");
        let path = dir.join("s.sock");

        // No listener - should time out.
        let result = wait_for_server_socket(
            &path,
            Duration::from_millis(50),
            &shepr_config::AppPaths::test_at(dir.path()),
        );
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
        let result = wait_for_server_socket(
            &path,
            Duration::from_secs(2),
            &shepr_config::AppPaths::test_at(dir.path()),
        );
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
                    b"{\"id\":\"autodetect:server:status\",\"result\":{\"type\":\"pong\",\"version\":\"0.5.5\",\"build_id\":\"0123456789abcdef\"}}\n",
                )
                .expect("test precondition");
            stream.flush().expect("test precondition");
        });

        let status = shepr_api::read_runtime_status_at(&path, Duration::from_millis(200))
            .expect("test precondition")
            .expect("test precondition");
        let _ = handle.join();
        assert_eq!(status.version.as_deref(), Some("0.5.5"));
        assert_eq!(status.build_id, "0123456789abcdef");
    }

    #[test]
    fn validate_running_server_compatibility_fails_when_status_api_missing() {
        let env = IsolatedEnv::new();
        let path = env.path().join("api.sock");
        env.set(EnvVar::SheprSocketPath, &path);
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        let err = validate_running_server_compatibility(&paths).expect_err("test precondition");

        assert!(
            err.to_string().contains("status API is unavailable"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_running_server_compatibility_names_session_commands_for_build_mismatch() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSession, "work");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
        let path = shepr_api::session::api_socket_path_for(&paths, paths.session_id());
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
            let other_build = if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
                "0000000000000000"
            } else {
                "ffffffffffffffff"
            };
            let body = format!(
                "{{\"id\":\"autodetect:server:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.5.5\",\"build_id\":\"{other_build}\"}}}}\n"
            );
            stream
                .write_all(body.as_bytes())
                .expect("test precondition");
            stream.flush().expect("test precondition");
        });

        let err = validate_running_server_compatibility(&paths).expect_err("test precondition");
        let message = err.to_string();

        let _ = handle.join();
        assert!(
            message.contains("Stop the running server to use this build"),
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
