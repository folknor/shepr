use super::*;
use shepr_test_fixtures::AppPathsFixture as _;
use shepr_test_support::fixture::{self, Held, Step};
use shepr_test_support::{IsolatedEnv, ScratchDir, drop_dac_capabilities_on_this_thread};
use std::cell::Cell;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::UnixListener;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn status_of_build(build_id: &str) -> RuntimeStatus {
    RuntimeStatus {
        version: Some("0.0.0".to_owned()),
        build_id: build_id.to_owned(),
        capabilities: None,
    }
}

fn this_build() -> RuntimeStatus {
    status_of_build(shepr_protocol::BUILD_ID)
}

fn other_build_id() -> &'static str {
    if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
        "0000000000000000"
    } else {
        "ffffffffffffffff"
    }
}

fn other_build() -> RuntimeStatus {
    status_of_build(other_build_id())
}

/// Answers one status request the way a server of `build_id` would.
fn serve_status_once(
    listener: UnixListener,
    build_id: &'static str,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("test precondition");
        let mut request = String::new();
        BufReader::new(stream.try_clone().expect("test precondition"))
            .read_line(&mut request)
            .expect("test precondition");
        assert!(request.contains("ping"));
        let body = format!(
            "{{\"id\":\"autodetect:server:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.5.5\",\"build_id\":\"{build_id}\"}}}}\n"
        );
        stream
            .write_all(body.as_bytes())
            .expect("test precondition");
        stream.flush().expect("test precondition");
    })
}

/// A spawner that runs the fixture as the daemon: detached like the real
/// launch, its stderr the boot log, and its pid recorded for the test.
fn fixture_daemon<'a>(
    steps: &[Step],
    pid: &'a Cell<u32>,
) -> impl FnOnce(Stdio) -> io::Result<Child> + 'a {
    let steps = steps.to_vec();
    move |stderr| {
        let mut command = fixture::command(&steps);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        shepr_platform::detach_server_daemon_command(&mut command);
        let child = command.spawn()?;
        pid.set(child.id());
        Ok(child)
    }
}

/// A daemon that starts a child of its own and then idles, so a kill of its
/// process group is distinguishable from a kill of the leader alone.
fn idle_daemon_steps() -> Vec<Step> {
    vec![
        Step::Spawn {
            argv0: "shepr-grandchild".into(),
            sleep: Duration::from_secs(60),
            held: Held::Nothing,
        },
        Step::Sleep(Duration::from_secs(60)),
    ]
}

/// Runs `launch_with` against the fixture daemon in `dir`, on real time.
fn launch_fixture(
    dir: &ScratchDir,
    steps: &[Step],
    timeout: Duration,
    probe: impl FnMut() -> io::Result<Probed>,
) -> (io::Result<RuntimeStatus>, u32) {
    let server = dir.join("shepr-server");
    let boot_log = dir.join("server-boot.log");
    let server_log = dir.join("shepr-server.log");
    let pid = Cell::new(0);
    let result = launch_with(
        &LaunchFiles {
            server: &server,
            boot_log: &boot_log,
            server_log: &server_log,
        },
        timeout,
        fixture_daemon(steps, &pid),
        probe,
        &mut Instant::now,
        &mut std::thread::sleep,
    );
    (result, pid.get())
}

fn group_is_gone(group: u32) -> bool {
    let group = libc::pid_t::try_from(group).expect("pid fits");
    // SAFETY: kill(2) with signal zero only probes the group.
    let result = unsafe { libc::kill(-group, 0) };
    result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn assert_group_dies(group: u32) {
    assert_ne!(group, 0, "the daemon was never spawned");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !group_is_gone(group) {
        assert!(
            Instant::now() < deadline,
            "the daemon's process group survived the failed launch"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn kill_group(group: u32) {
    let group = libc::pid_t::try_from(group).expect("pid fits");
    // SAFETY: the test owns the daemon this group belongs to.
    let result = unsafe { libc::kill(-group, libc::SIGKILL) };
    assert_eq!(result, 0, "clean up the stand-in daemon");
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

#[test]
fn probing_a_missing_socket_finds_no_server() {
    let dir = ScratchDir::new("probe-missing");
    let probed = probe_server_at(&dir.join("s.sock"), &dir.join("a.sock"))
        .expect("an absent socket is not an error");
    assert!(matches!(probed, Probed::NoServer));
}

#[test]
fn probing_a_stale_socket_finds_no_server() {
    let dir = ScratchDir::new("probe-stale");
    let path = dir.join("s.sock");
    drop(UnixListener::bind(&path).expect("test precondition"));
    let probed =
        probe_server_at(&path, &dir.join("a.sock")).expect("a stale socket is not an error");
    assert!(matches!(probed, Probed::NoServer));
}

#[test]
fn probing_an_inaccessible_socket_is_an_error_not_absence() {
    let dir = ScratchDir::new("probe-inaccessible");
    let parent = dir.join("private");
    std::fs::create_dir(&parent).expect("create inaccessible directory");
    let mut permissions = std::fs::metadata(&parent)
        .expect("read inaccessible directory metadata")
        .permissions();
    permissions.set_mode(0o000);
    std::fs::set_permissions(&parent, permissions).expect("restrict directory permissions");

    let path = parent.join("s.sock");
    let api = dir.join("a.sock");
    let probe = std::thread::spawn(move || {
        drop_dac_capabilities_on_this_thread();
        probe_server_at(&path, &api)
    })
    .join();

    let mut permissions = std::fs::metadata(&parent)
        .expect("read inaccessible directory metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&parent, permissions).expect("restore directory permissions");
    let error = match probe.expect("permission probe thread completes") {
        Ok(_) => panic!("permission errors must not mean no server"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn a_regular_file_at_the_socket_path_is_an_error_not_absence() {
    let dir = ScratchDir::new("probe-regular-file");
    let path = dir.join("s.sock");
    std::fs::write(&path, b"not a socket").expect("test precondition");
    let error = match probe_server_at(&path, &dir.join("a.sock")) {
        Ok(_) => panic!("a non-socket must not read as absence"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("cannot tell"), "{error}");
}

#[test]
fn a_listener_without_a_status_answer_is_unresponsive() {
    let dir = ScratchDir::new("probe-unresponsive");
    let path = dir.join("s.sock");
    let _listener = UnixListener::bind(&path).expect("test precondition");
    let probed = probe_server_at(&path, &dir.join("a.sock")).expect("a live socket probes");
    assert!(matches!(probed, Probed::Unresponsive));
}

#[test]
fn a_live_server_is_probed_for_its_status() {
    let dir = ScratchDir::new("probe-running");
    let client = dir.join("s.sock");
    let api = dir.join("a.sock");
    let _client_listener = UnixListener::bind(&client).expect("test precondition");
    let server = serve_status_once(
        UnixListener::bind(&api).expect("test precondition"),
        shepr_protocol::BUILD_ID,
    );
    let probed = probe_server_at(&client, &api).expect("a live server probes");
    server.join().expect("fake server thread");
    let Probed::Running(status) = probed else {
        panic!("a live server that answers is running");
    };
    assert_eq!(status.version.as_deref(), Some("0.5.5"));
    assert_eq!(status.build_id, shepr_protocol::BUILD_ID);
}

// ---------------------------------------------------------------------------
// The server executable
// ---------------------------------------------------------------------------

#[test]
fn the_server_is_the_sibling_of_the_client() {
    let dir = ScratchDir::new("sibling-ok");
    let installed = fixture::stand_in(dir.path(), SERVER_BINARY_NAME, &[]);
    let found = sibling_server_executable(&dir.join("shepr")).expect("the sibling is installed");
    assert_eq!(found, installed);
}

#[test]
fn a_missing_sibling_is_an_install_error_naming_its_path() {
    let dir = ScratchDir::new("sibling-missing");
    let error = sibling_server_executable(&dir.join("shepr"))
        .expect_err("nothing is installed beside the client");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let message = error.to_string();
    assert!(
        message.contains(&dir.join(SERVER_BINARY_NAME).display().to_string()),
        "{message}"
    );
    assert!(message.contains("install"), "{message}");
}

#[test]
fn a_nonexecutable_sibling_is_an_install_error() {
    let dir = ScratchDir::new("sibling-not-executable");
    let server = dir.join(SERVER_BINARY_NAME);
    std::fs::write(&server, b"not a program\n").expect("test precondition");
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o644))
        .expect("test precondition");
    let error = sibling_server_executable(&dir.join("shepr"))
        .expect_err("a file without execute access cannot be started");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("not executable"), "{error}");
}

#[test]
fn a_directory_named_like_the_server_is_an_install_error() {
    let dir = ScratchDir::new("sibling-directory");
    std::fs::create_dir(dir.join(SERVER_BINARY_NAME)).expect("test precondition");
    let error =
        sibling_server_executable(&dir.join("shepr")).expect_err("a directory is not a server");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

// ---------------------------------------------------------------------------
// The launch lock
// ---------------------------------------------------------------------------

#[test]
fn the_launch_lock_wait_is_bounded_and_its_file_persists() {
    let dir = ScratchDir::new("launch-lock");
    let lock_path = dir.join("runtime/launch.lock");
    let held = shepr_platform::ipc::acquire_flock_lock(&lock_path, false).expect("first holder");
    let inode = std::fs::metadata(&lock_path)
        .expect("lock file exists")
        .ino();

    let started = Instant::now();
    let error = match acquire_launch_lock_with(
        &lock_path,
        Duration::from_millis(150),
        &mut Instant::now,
        &mut std::thread::sleep,
    ) {
        Ok(_) => panic!("a held launch lock must not be granted twice"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait must be bounded"
    );

    drop(held);
    acquire_launch_lock_with(
        &lock_path,
        Duration::from_millis(150),
        &mut Instant::now,
        &mut std::thread::sleep,
    )
    .expect("a released lock is granted");
    assert_eq!(
        std::fs::metadata(&lock_path)
            .expect("lock file persists")
            .ino(),
        inode,
        "contenders always lock the same inode"
    );
}

// ---------------------------------------------------------------------------
// Starting the daemon
// ---------------------------------------------------------------------------

#[test]
fn a_daemon_that_dies_during_boot_reports_its_exit_and_output() {
    let dir = ScratchDir::new("launch-boot-failure");
    let (result, _) = launch_fixture(
        &dir,
        &[
            Step::PrintErr("no such runtime directory\n".into()),
            Step::Exit(shepr_api::daemon_exit::CONFIG_REFUSED_EXIT_CODE),
        ],
        Duration::from_secs(10),
        || Ok(Probed::NoServer),
    );
    let message = result
        .expect_err("a dead daemon is a failed launch")
        .to_string();
    assert!(message.contains("refused its configuration"), "{message}");
    assert!(message.contains("no such runtime directory"), "{message}");
    assert!(message.contains("server-boot.log"), "{message}");
    assert!(message.contains("shepr-server.log"), "{message}");
}

#[test]
fn a_daemon_that_gives_way_to_an_occupant_waits_for_the_occupant() {
    let dir = ScratchDir::new("launch-occupant");
    let calls = Cell::new(0_u32);
    let (result, _) = launch_fixture(
        &dir,
        &[Step::Exit(
            shepr_api::daemon_exit::ALREADY_RUNNING_EXIT_CODE,
        )],
        Duration::from_secs(10),
        || {
            calls.set(calls.get() + 1);
            Ok(if calls.get() < 4 {
                Probed::NoServer
            } else {
                Probed::Running(this_build())
            })
        },
    );
    let status = result.expect("the occupant answers, so the client can attach");
    assert!(shepr_protocol::is_this_build(&status.build_id));
}

#[test]
fn an_occupant_of_another_build_is_handed_back_once_the_daemon_gave_way() {
    let dir = ScratchDir::new("launch-occupant-mismatch");
    let calls = Cell::new(0_u32);
    let (result, _) = launch_fixture(
        &dir,
        &[Step::Exit(
            shepr_api::daemon_exit::ALREADY_RUNNING_EXIT_CODE,
        )],
        Duration::from_secs(10),
        || {
            calls.set(calls.get() + 1);
            // Long enough that the daemon has exited before the occupant answers.
            Ok(if calls.get() < 8 {
                Probed::NoServer
            } else {
                Probed::Running(other_build())
            })
        },
    );
    let status = result.expect("the caller's policy decides about the occupant");
    assert!(!shepr_protocol::is_this_build(&status.build_id));
}

#[test]
fn an_occupant_that_never_answers_ends_in_a_timeout() {
    let dir = ScratchDir::new("launch-occupant-silent");
    let (result, _) = launch_fixture(
        &dir,
        &[Step::Exit(
            shepr_api::daemon_exit::ALREADY_RUNNING_EXIT_CODE,
        )],
        Duration::from_millis(400),
        || Ok(Probed::Unresponsive),
    );
    let error = result.expect_err("a silent occupant is never attached to");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(
        error.to_string().contains("another server already running"),
        "{error}"
    );
}

#[test]
fn the_timeout_kills_the_daemons_whole_process_group() {
    let dir = ScratchDir::new("launch-timeout");
    let (result, group) = launch_fixture(
        &dir,
        &idle_daemon_steps(),
        Duration::from_millis(300),
        || Ok(Probed::NoServer),
    );
    let error = result.expect_err("a daemon that never answers is a failed launch");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(
        error.to_string().contains("did not become ready"),
        "{error}"
    );
    assert_group_dies(group);
}

#[test]
fn a_probe_failure_kills_the_daemon() {
    let dir = ScratchDir::new("launch-probe-failure");
    let (result, group) =
        launch_fixture(&dir, &idle_daemon_steps(), Duration::from_secs(10), || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the socket is served by another user",
            ))
        });
    let error = result.expect_err("an untrusted socket fails the launch");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_group_dies(group);
}

#[test]
fn a_sibling_of_another_build_is_killed_and_reported() {
    let dir = ScratchDir::new("launch-sibling-mismatch");
    let (result, group) =
        launch_fixture(&dir, &idle_daemon_steps(), Duration::from_secs(10), || {
            Ok(Probed::Running(other_build()))
        });
    let message = result
        .expect_err("a server of another build is not this client's")
        .to_string();
    assert!(message.contains("different build"), "{message}");
    assert!(message.contains("install"), "{message}");
    assert_group_dies(group);
}

#[test]
fn a_daemon_that_proves_its_build_is_kept_running() {
    let dir = ScratchDir::new("launch-ready");
    let calls = Cell::new(0_u32);
    let (result, group) =
        launch_fixture(&dir, &idle_daemon_steps(), Duration::from_secs(10), || {
            calls.set(calls.get() + 1);
            Ok(match calls.get() {
                1 => Probed::NoServer,
                2 => Probed::Unresponsive,
                _ => Probed::Running(this_build()),
            })
        });
    result.expect("a booting daemon that answers is a successful launch");
    assert_ne!(group, 0);
    assert!(
        !group_is_gone(group),
        "the launched daemon must keep running"
    );
    kill_group(group);
}

#[test]
fn a_symlinked_boot_log_refuses_the_launch_before_spawning() {
    let dir = ScratchDir::new("launch-boot-log-link");
    let target = dir.join("target");
    std::fs::write(&target, b"keep").expect("test precondition");
    let boot_log = dir.join("server-boot.log");
    std::os::unix::fs::symlink(&target, &boot_log).expect("plant a link");
    let server = dir.join("shepr-server");
    let server_log = dir.join("shepr-server.log");

    let result = launch_with(
        &LaunchFiles {
            server: &server,
            boot_log: &boot_log,
            server_log: &server_log,
        },
        Duration::from_secs(1),
        |_stderr| -> io::Result<Child> { panic!("no daemon may start with an unsafe boot log") },
        || Ok(Probed::NoServer),
        &mut Instant::now,
        &mut std::thread::sleep,
    );
    result.expect_err("a symlinked boot log is refused");
    assert_eq!(std::fs::read(&target).expect("target kept"), b"keep");
}

// ---------------------------------------------------------------------------
// Rendezvous
// ---------------------------------------------------------------------------

/// Client and API sockets of the default runtime address, with the runtime
/// directory created.
fn runtime_sockets(paths: &shepr_config::AppPaths) -> (PathBuf, PathBuf) {
    std::fs::create_dir_all(paths.runtime_dir()).expect("create the runtime directory");
    (
        paths.server_address().client_socket().to_path_buf(),
        shepr_api::socket_path(paths),
    )
}

fn assert_nothing_was_launched(paths: &shepr_config::AppPaths) {
    for name in [LAUNCH_LOCK_FILE_NAME, BOOT_LOG_FILE_NAME] {
        assert!(
            !paths
                .runtime_dir()
                .join(name)
                .try_exists()
                .expect("stat the runtime file"),
            "{name} must not exist: no launch was attempted"
        );
    }
}

#[test]
fn a_socket_override_never_starts_a_server() {
    let env = IsolatedEnv::new();
    let socket = env.path().join("named.sock");
    env.set(EnvVar::SheprSocketPath, &socket);
    let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

    let error = ensure_running(&paths, Duration::from_secs(1), BuildCheck::BeforeAttach)
        .expect_err("an override only reaches a running server");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let message = error.to_string();
    assert!(message.contains("no shepr server is running"), "{message}");
    assert!(message.contains("SHEPR_SOCKET_PATH"), "{message}");
    assert_nothing_was_launched(&paths);
}

#[test]
fn a_listener_that_does_not_answer_is_never_replaced() {
    let _env = IsolatedEnv::new();
    let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
    let (client, _api) = runtime_sockets(&paths);
    let _listener = UnixListener::bind(&client).expect("test precondition");

    let error = ensure_running(
        &paths,
        Duration::from_secs(1),
        BuildCheck::AtClientHandshake,
    )
    .expect_err("an unresponsive listener is a failure");
    assert!(error.to_string().contains("not answering"), "{error}");
    assert_nothing_was_launched(&paths);
}

#[test]
fn a_running_server_of_this_build_is_used_without_a_launch() {
    let _env = IsolatedEnv::new();
    let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
    let (client, api) = runtime_sockets(&paths);
    let _client_listener = UnixListener::bind(&client).expect("test precondition");
    let server = serve_status_once(
        UnixListener::bind(&api).expect("test precondition"),
        shepr_protocol::BUILD_ID,
    );

    ensure_running(&paths, Duration::from_secs(1), BuildCheck::BeforeAttach)
        .expect("a healthy server of this build is used");
    server.join().expect("fake server thread");
    assert_nothing_was_launched(&paths);
}

#[test]
fn a_running_server_of_another_build_is_refused_before_attaching() {
    let _env = IsolatedEnv::new();
    let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
    let (client, api) = runtime_sockets(&paths);
    let _client_listener = UnixListener::bind(&client).expect("test precondition");
    let server = serve_status_once(
        UnixListener::bind(&api).expect("test precondition"),
        other_build_id(),
    );

    let error = ensure_running(&paths, Duration::from_secs(1), BuildCheck::BeforeAttach)
        .expect_err("a server of another build is refused");
    server.join().expect("fake server thread");
    let message = error.to_string();
    assert!(message.contains("different build"), "{message}");
    assert_nothing_was_launched(&paths);
}

#[test]
fn the_bridge_leaves_a_running_mismatch_to_the_handshake() {
    let _env = IsolatedEnv::new();
    let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
    let (client, api) = runtime_sockets(&paths);
    let _client_listener = UnixListener::bind(&client).expect("test precondition");
    let server = serve_status_once(
        UnixListener::bind(&api).expect("test precondition"),
        other_build_id(),
    );

    ensure_running(
        &paths,
        Duration::from_secs(1),
        BuildCheck::AtClientHandshake,
    )
    .expect("the typed handshake reports the mismatch, not the launcher");
    server.join().expect("fake server thread");
    assert_nothing_was_launched(&paths);
}

// ---------------------------------------------------------------------------
// The daemon command
// ---------------------------------------------------------------------------

#[test]
fn server_daemon_command_marks_the_client_spawn_and_nothing_else() {
    let paths = shepr_config::AppPaths::test_default();
    let command = build_server_daemon_command(
        &PathBuf::from("/tmp/shepr-server-test"),
        Path::new("/"),
        None,
        &paths,
    );
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(args, [OsStr::new(CLIENT_SPAWNED_FLAG)]);
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
        *key == OsStr::new(EnvVar::SheprStartupCwd.name()) && value == &Some(expected.as_os_str())
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
