//! Shared local server startup and build checks for direct and SSH clients.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use shepr_core::env::EnvVar;
use tracing::info;

use crate::limits::{SOCKET_POLL_INTERVAL, STATUS_REQUEST_TIMEOUT};

pub use crate::limits::SERVER_READY_TIMEOUT;

fn client_socket_path(paths: &shepr_config::AppPaths) -> std::path::PathBuf {
    paths.server_address().client_socket().to_path_buf()
}

/// A direct client checks the build before attaching. An SSH bridge leaves the
/// check to the client's typed protocol handshake so mismatch errors retain it.
#[derive(Clone, Copy)]
pub enum BuildCheck {
    BeforeAttach,
    AtClientHandshake,
}

/// Ensures a server is listening, with the caller's build-check policy.
pub fn ensure_running(
    paths: &shepr_config::AppPaths,
    timeout: Duration,
    build_check: BuildCheck,
) -> io::Result<()> {
    if is_server_listening(paths)? {
        info!("server already running");
        return match build_check {
            BuildCheck::BeforeAttach => validate_running_server_compatibility(paths),
            BuildCheck::AtClientHandshake => Ok(()),
        };
    }
    info!("no server running, spawning server daemon");
    spawn_server_daemon(paths)?;
    wait_for_server_socket(&client_socket_path(paths), timeout, paths)
}

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
    match shepr_platform::ipc::probe(socket_path) {
        shepr_platform::ipc::Liveness::Live => Ok(true),
        shepr_platform::ipc::Liveness::Absent | shepr_platform::ipc::Liveness::Stale => Ok(false),
        shepr_platform::ipc::Liveness::Unreachable(err) => {
            tracing::warn!(path = %socket_path.display(), error = %err, "failed to check server socket");
            Err(err)
        }
    }
}

fn read_server_status(
    paths: &shepr_config::AppPaths,
) -> io::Result<Option<shepr_api::RuntimeStatus>> {
    shepr_api::read_runtime_status_at(&shepr_api::socket_path(paths), STATUS_REQUEST_TIMEOUT)
}

/// Checks a local server before attaching. Saved-machine status queries the
/// remote server through `check_saved_ssh` and its status JSON parser.
pub fn validate_running_server_compatibility(paths: &shepr_config::AppPaths) -> io::Result<()> {
    let Some(status) = read_server_status(paths)? else {
        return Err(io::Error::other(format!(
            "a shepr server is listening, but its status API is unavailable, so its build cannot be confirmed.\n\n{}\nIf that fails, stop the server process manually.",
            shepr_api::session::restart_after_update_guidance_for(paths)
        )));
    };

    if shepr_protocol::is_this_build(&status.build_id) {
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
///   socket target on its child command, including removals for inherited
///   overrides that were superseded.
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

    let mut command = build_server_daemon_command(
        &exe,
        &server_daemon_working_dir(paths),
        paths.current_dir(),
        paths,
    );

    let pid = command.spawn().map(|child| child.id()).map_err(|err| {
        io::Error::new(err.kind(), format!("failed to spawn shepr server: {err}"))
    })?;
    info!(pid, "server daemon spawned");

    Ok(pid)
}

/// The working directory the server daemon runs in: the user's home
/// directory, or `/` when there is none. Not the launching shell's directory:
/// the daemon outlives that shell, and inheriting it would pin the directory
/// for the server's whole life (an unmount fails with EBUSY, a deleted
/// directory stays referenced). The directory the user launched from still
/// reaches the server, as `SHEPR_STARTUP_CWD`, and is the server's resolved
/// current directory (`AppPaths::resolve_for_server`): new terminals,
/// `new_terminal_cwd = "current"` and a relative `new_terminal_cwd` resolve
/// against it, not against this working directory. Home rather than `/` in
/// case a launch hands over no directory, since the server then falls back
/// to its own.
fn server_daemon_working_dir(paths: &shepr_config::AppPaths) -> PathBuf {
    paths
        .home_dir()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

fn build_server_daemon_command(
    exe: &Path,
    working_dir: &Path,
    startup_cwd: Option<&Path>,
    paths: &shepr_config::AppPaths,
) -> Command {
    let mut command = shepr_platform::child_command(exe, working_dir);
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
fn wait_for_server_socket(
    socket_path: &Path,
    timeout: Duration,
    paths: &shepr_config::AppPaths,
) -> io::Result<()> {
    wait_for_server_socket_with(
        socket_path,
        timeout,
        paths,
        // clock-io-ok: the production adapter measures actual socket readiness time.
        std::time::Instant::now,
        is_server_listening_at,
        std::thread::sleep,
    )
}

fn wait_for_server_socket_with(
    socket_path: &Path,
    timeout: Duration,
    paths: &shepr_config::AppPaths,
    mut now: impl FnMut() -> std::time::Instant,
    mut probe: impl FnMut(&Path) -> io::Result<bool>,
    mut sleep: impl FnMut(Duration),
) -> io::Result<()> {
    let deadline = now() + timeout;

    while now() < deadline {
        if probe(socket_path)? {
            info!(path = %socket_path.display(), "server socket ready");
            return Ok(());
        }
        sleep(SOCKET_POLL_INTERVAL);
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "server did not become ready within {}s (socket: {}). The background server may still be starting; try `shepr` again, or check {}",
            timeout.as_secs(),
            socket_path.display(),
            paths.data_dir().join("shepr-server.log").display()
        ),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::AppPathsFixture as _;
    use shepr_test_support::{IsolatedEnv, ScratchDir, drop_dac_capabilities_on_this_thread};
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
        let dir = ScratchDir::new("inaccessible");
        let parent = dir.join("private");
        std::fs::create_dir(&parent).expect("create inaccessible directory");
        let mut permissions = std::fs::metadata(&parent)
            .expect("read inaccessible directory metadata")
            .permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&parent, permissions).expect("restrict directory permissions");

        let path = parent.join("s.sock");
        let probe = std::thread::spawn(move || {
            drop_dac_capabilities_on_this_thread();
            is_server_listening_at(&path)
        })
        .join();

        let mut permissions = std::fs::metadata(&parent)
            .expect("read inaccessible directory metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&parent, permissions).expect("restore directory permissions");
        let result = probe.expect("permission probe thread completes");
        assert_eq!(
            result
                .expect_err("permission errors must not mean no server")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn server_daemon_command_clears_superseded_socket_overrides() {
        let env = IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, "/tmp/inherited.sock");
        env.set(EnvVar::SheprClientSocketPath, "/tmp/inherited-client.sock");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        let command = build_server_daemon_command(
            &PathBuf::from("/tmp/shepr-test"),
            Path::new("/"),
            Some(Path::new("/home/test")),
            &paths,
        );
        let envs: Vec<_> = command.get_envs().collect();

        // The API override outranks the client one, so the child gets the API
        // override as resolved and the superseded client override is removed.
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(EnvVar::SheprSocketPath.name())
                && *value == Some(OsStr::new("/tmp/inherited.sock"))
        }));
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(EnvVar::SheprClientSocketPath.name()) && value.is_none()
        }));
    }

    #[test]
    fn server_daemon_command_passes_current_dir_as_startup_cwd() {
        let expected = Path::new("/home/test");
        let paths = shepr_config::AppPaths::test_default();
        let command = build_server_daemon_command(
            &PathBuf::from("/tmp/shepr-test"),
            Path::new("/"),
            Some(expected),
            &paths,
        );
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(EnvVar::SheprStartupCwd.name())
                && value == &Some(expected.as_os_str())
        }));
    }

    #[test]
    fn server_daemon_runs_in_home_not_the_launch_directory() {
        let scratch = ScratchDir::new("daemon-working-dir");
        let paths = shepr_config::AppPaths::test_at(scratch.path());
        let working_dir = server_daemon_working_dir(&paths);
        assert_eq!(
            Some(working_dir.as_path()),
            paths.home_dir().or(Some(Path::new("/")))
        );

        let launch_dir = scratch.join("launch");
        let command = build_server_daemon_command(
            &PathBuf::from("/tmp/shepr-test"),
            &working_dir,
            Some(&launch_dir),
            &paths,
        );
        assert_eq!(command.get_current_dir(), Some(working_dir.as_path()));
        assert_ne!(command.get_current_dir(), Some(launch_dir.as_path()));
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
        use std::cell::Cell;

        let dir = ScratchDir::new("wait-delay");
        let path = dir.join("s.sock");
        let clock = Cell::new(std::time::Instant::now());
        let probes = Cell::new(0);
        let result = wait_for_server_socket_with(
            &path,
            Duration::from_secs(2),
            &shepr_config::AppPaths::test_at(dir.path()),
            || clock.get(),
            |_| {
                probes.set(probes.get() + 1);
                Ok(probes.get() == 2)
            },
            |duration| clock.set(clock.get() + duration),
        );
        assert!(result.is_ok());
        assert_eq!(probes.get(), 2);
    }

    #[test]
    fn wait_for_server_socket_stops_probing_when_the_clock_reaches_the_deadline() {
        use std::cell::Cell;

        let dir = ScratchDir::new("wait-deadline");
        let path = dir.join("s.sock");
        let timeout = SOCKET_POLL_INTERVAL * 4;
        let clock = Cell::new(std::time::Instant::now());
        let probes = Cell::new(0_u32);
        let result = wait_for_server_socket_with(
            &path,
            timeout,
            &shepr_config::AppPaths::test_at(dir.path()),
            || clock.get(),
            |_| {
                probes.set(probes.get() + 1);
                Ok(false)
            },
            |duration| clock.set(clock.get() + duration),
        );
        assert_eq!(
            result.expect_err("the socket never becomes ready").kind(),
            io::ErrorKind::TimedOut
        );
        // One probe per poll interval before the deadline, none at it.
        assert_eq!(probes.get(), 4);
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
        handle.join().expect("fake server thread");
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
    fn validate_running_server_compatibility_names_the_restart_for_build_mismatch() {
        let _env = IsolatedEnv::new();
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
        let path = shepr_api::socket_path(&paths);
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

        handle.join().expect("fake server thread");
        assert!(
            message.contains("different build"),
            "unexpected error: {message}"
        );
    }
}
