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
        version: "0.0.0".to_owned(),
        build_id: build_id.parse().expect("build identity"),
        boot_id: "4242-1700000000".parse().expect("boot identity"),
        lifecycle: crate::status::RuntimeLifecycle::Running,
    }
}

fn this_build() -> RuntimeStatus {
    status_of_build(shepr_protocol::BUILD_ID)
}

use shepr_test_fixtures::other_build_id;

fn other_build() -> RuntimeStatus {
    status_of_build(other_build_id())
}

/// Answers one status request the way a server of `build_id` would.
fn serve_status_once(
    listener: UnixListener,
    build_id: &'static str,
) -> std::thread::JoinHandle<()> {
    serve_pong_once(listener, build_id, false, false)
}

/// Answers one status request the way a server of `build_id` would, saying
/// whether it is stopping or starting. Bare connects that send nothing are
/// liveness probes, not requests, and are skipped.
fn serve_pong_once(
    listener: UnixListener,
    build_id: &'static str,
    stopping: bool,
    starting: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut stream = loop {
            let (stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut request)
                .expect("request");
            if request.is_empty() {
                continue;
            }
            assert!(request.contains("ping"));
            break stream;
        };
        let body = format!(
            "{{\"id\":\"{}\",\"result\":{{\"type\":\"pong\",\"version\":\"0.5.5\",\"build_id\":\"{build_id}\",\"boot_id\":\"4242-1700000000\",\"stopping\":{stopping},\"starting\":{starting}}}}}\n",
            shepr_api::schema::RequestId::StatusPing.as_str(),
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
) -> impl FnMut(Stdio) -> io::Result<Child> + 'a {
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
) -> (Result<RuntimeStatus, LaunchError>, FixtureDaemonGuard) {
    let server = dir.join("shepr-server");
    let boot_log = shepr_paths::boot_log_path(dir.path());
    let server_log = shepr_paths::server_log_path(dir.path());
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
    (result, FixtureDaemonGuard::new(pid.get()))
}

/// Owns a detached fixture daemon's process group until the test has finished
/// inspecting it. `SpawnedDaemon::disarm` transfers the leader to a background
/// reaper; this guard also kills and waits for fixture descendants.
struct FixtureDaemonGuard {
    process_group: u32,
}

impl FixtureDaemonGuard {
    fn new(process_group: u32) -> Self {
        Self { process_group }
    }

    fn process_group(&self) -> u32 {
        self.process_group
    }
}

impl Drop for FixtureDaemonGuard {
    fn drop(&mut self) {
        if self.process_group == 0 {
            return;
        }
        let group = libc::pid_t::try_from(self.process_group).expect("fixture pid fits");
        // SAFETY: the test daemon called setsid, so its pid is the process
        // group id; the guard owns that fixture group until this drop.
        let result = unsafe { libc::kill(-group, libc::SIGKILL) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            if !std::thread::panicking() {
                panic!("could not kill fixture process group {group}: {error}");
            }
            return;
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while !group_is_gone(self.process_group) {
            if Instant::now() >= deadline {
                if !std::thread::panicking() {
                    panic!("fixture process group {group} survived cleanup");
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
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

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

#[test]
fn probing_a_missing_socket_finds_no_server() {
    let dir = ScratchDir::new("probe-missing");
    let probed = probe_server_at(&dir.join("s.sock")).expect("an absent socket is not an error");
    assert!(matches!(probed, Probed::NoServer));
}

#[test]
fn probing_a_stale_socket_finds_no_server() {
    let dir = ScratchDir::new("probe-stale");
    let path = dir.join("s.sock");
    drop(UnixListener::bind(&path).expect("test precondition"));
    let probed = probe_server_at(&path).expect("a stale socket is not an error");
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
    let probe = std::thread::spawn(move || {
        drop_dac_capabilities_on_this_thread();
        probe_server_at(&path)
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
    let error = match probe_server_at(&path) {
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
    let probed = probe_server_at(&path).expect("a live socket probes");
    assert!(matches!(probed, Probed::Unresponsive));
}

#[test]
fn a_socket_answering_starting_is_a_startup_transition() {
    let dir = ScratchDir::new("probe-starting");
    let socket = dir.join("server.sock");
    let server = serve_pong_once(
        UnixListener::bind(&socket).expect("bind"),
        shepr_protocol::BUILD_ID,
        false,
        true,
    );
    assert!(matches!(
        probe_server_at(&socket).expect("probe"),
        Probed::Starting
    ));
    server.join().expect("server");
}

/// Answers starting until the test releases it, skipping liveness connects.
fn serve_starting_until_released(
    path: &Path,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let listener = UnixListener::bind(path).expect("bind starting socket");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let (release, stop) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        while stop.try_recv().is_err() {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .expect("read bound");
                    let mut request = String::new();
                    BufReader::new(stream.try_clone().expect("clone"))
                        .read_line(&mut request)
                        .expect("request");
                    if request.is_empty() {
                        continue;
                    }
                    let body = serde_json::json!({"id":shepr_api::schema::RequestId::StatusPing.as_str(),"result":{"type":"pong","version":"0.1.0","build_id":shepr_protocol::BUILD_ID,"boot_id":"17-23","starting":true}});
                    writeln!(stream, "{body}").expect("pong");
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
    });
    (release, thread)
}

#[test]
fn repeated_socket_transitions_share_one_wait_deadline() {
    let _env = IsolatedEnv::new();
    let paths = shepr_paths::AppPaths::resolve().expect("paths");
    let timeout = LIFECYCLE_POLL_INTERVAL * 10;
    let transition_after = LIFECYCLE_POLL_INTERVAL * 7;
    let start = Instant::now();
    let elapsed = Cell::new(Duration::ZERO);
    let mut now = || start + elapsed.get();
    let mut sleep = |duration| elapsed.set(elapsed.get() + duration);
    let deadline = start + timeout;
    let mut first_probe = || {
        Ok(if elapsed.get() < transition_after {
            Probed::Starting
        } else {
            Probed::NoServer
        })
    };
    assert!(matches!(
        wait_for_server_socket_to_settle_until_with(
            &paths,
            deadline,
            timeout,
            &mut first_probe,
            &mut now,
            &mut sleep,
        )
        .expect("first transition ends"),
        SettledServer::NoServer
    ));
    assert_eq!(elapsed.get(), transition_after);
    let mut second_probe = || Ok(Probed::Starting);
    let error = wait_for_server_socket_to_settle_until_with(
        &paths,
        deadline,
        timeout,
        &mut second_probe,
        &mut now,
        &mut sleep,
    )
    .expect_err("the second transition shares the first wait's deadline");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(elapsed.get(), timeout);
}

#[test]
fn a_live_server_is_probed_for_its_status() {
    let dir = ScratchDir::new("probe-running");
    let socket = dir.join("server.sock");
    let server = serve_status_once(
        UnixListener::bind(&socket).expect("test precondition"),
        shepr_protocol::BUILD_ID,
    );
    let probed = probe_server_at(&socket).expect("a live server probes");
    server.join().expect("fake server thread");
    let Probed::Running(status) = probed else {
        panic!("a live server that answers is running");
    };
    assert_eq!(status.version, "0.5.5");
    assert_eq!(status.build_id.to_string(), shepr_protocol::BUILD_ID);
}

#[test]
fn a_server_that_answers_it_is_stopping_is_not_running() {
    // Its socket stays up through its final save, but it accepts no TUI
    // connection, so it is not a server to attach to.
    let dir = ScratchDir::new("probe-stopping");
    let socket = dir.join("server.sock");
    let server = serve_pong_once(
        UnixListener::bind(&socket).expect("test precondition"),
        shepr_protocol::BUILD_ID,
        true,
        false,
    );
    let probed = probe_server_at(&socket).expect("a stopping server probes");
    server.join().expect("fake server thread");
    assert!(matches!(probed, Probed::Stopping));
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

#[test]
fn the_sibling_version_is_read_from_its_first_output_line() {
    let dir = ScratchDir::new("sibling-version");
    let server = fixture::stand_in(
        dir.path(),
        SERVER_BINARY_NAME,
        &[
            Step::Print("shepr-server 0.6.0+0123456789abcdef\nnoise\n".into()),
            Step::Exit(0),
        ],
    );
    let line = read_server_version_line(&server, Duration::from_secs(5)).expect("version runs");
    assert_eq!(line, "shepr-server 0.6.0+0123456789abcdef");
}

#[test]
fn a_failing_sibling_version_is_an_error() {
    let dir = ScratchDir::new("sibling-version-fails");
    let server = fixture::stand_in(dir.path(), SERVER_BINARY_NAME, &[Step::Exit(3)]);
    let error = read_server_version_line(&server, Duration::from_secs(5))
        .expect_err("a nonzero exit is not an identity");
    assert!(error.to_string().contains("--version failed"), "{error}");
}

#[test]
fn a_hung_sibling_version_is_cut_off_at_the_deadline() {
    let dir = ScratchDir::new("sibling-version-hangs");
    let server = fixture::stand_in(
        dir.path(),
        SERVER_BINARY_NAME,
        &[Step::Sleep(Duration::from_secs(60))],
    );
    let error = read_server_version_line(&server, Duration::from_millis(200))
        .expect_err("a hung binary times out");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

// ---------------------------------------------------------------------------
// The launch lock
// ---------------------------------------------------------------------------

#[test]
fn the_launch_lock_wait_is_bounded_and_its_file_persists() {
    let dir = ScratchDir::new("launch-lock");
    let runtime = dir.join("runtime");
    let lock_path = shepr_paths::launch_lock_path(&runtime);
    let held = shepr_platform::ipc::acquire_flock_lock(
        &lock_path,
        shepr_platform::ipc::LockWait::FailIfHeld,
    )
    .expect("first holder");
    let inode = std::fs::metadata(&lock_path)
        .expect("lock file exists")
        .ino();

    let wait = LIFECYCLE_POLL_INTERVAL * 3;
    let start = Instant::now();
    let time = Cell::new(start);
    let mut now = || time.get();
    let mut sleep = |duration| time.set(time.get() + duration);
    let error = match acquire_launch_lock_with(&lock_path, wait, &mut now, &mut sleep) {
        Ok(_) => panic!("a held launch lock must not be granted twice"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(time.get(), start + wait);
    // A lock another launcher holds is waited out, not repaired.
    assert_eq!(
        LaunchError::LaunchLock(error).remote_failure_class(),
        RemoteFailureClass::Retry
    );

    drop(held);
    acquire_launch_lock_with(&lock_path, wait, &mut now, &mut sleep)
        .expect("a released lock is granted");
    assert_eq!(
        std::fs::metadata(&lock_path)
            .expect("lock file persists")
            .ino(),
        inode,
        "contenders always lock the same inode"
    );
}

#[test]
fn the_running_server_status_never_starts_a_server() {
    let _env = IsolatedEnv::new();
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    assert!(
        running_server_status(&paths, || {})
            .expect("an absent server is no error")
            .is_none()
    );
    assert_nothing_was_launched(&paths);

    let socket = runtime_socket(&paths);
    let server = serve_status_once(
        UnixListener::bind(&socket).expect("test precondition"),
        other_build_id(),
    );
    let status = running_server_status(&paths, || {})
        .expect("a live server answers")
        .expect("a server is running");
    server.join().expect("fake server thread");
    assert_eq!(status.build_id.to_string(), other_build_id());
}

#[test]
fn a_silent_listener_reads_as_the_launchs_unresponsive_error() {
    let _env = IsolatedEnv::new();
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    let _listener = UnixListener::bind(runtime_socket(&paths)).expect("test precondition");
    let error = running_server_status(&paths, || {}).expect_err("a silent listener is no absence");
    assert!(
        matches!(error, LaunchError::Unresponsive { .. }),
        "{error:?}"
    );
    let stop_command = format!("`{} stop`", crate::guidance::operator_entrypoint());
    assert!(error.to_string().contains(&stop_command), "{error}");
    assert_nothing_was_launched(&paths);
}

#[test]
fn a_stopping_server_is_offered_no_restart() {
    let _env = IsolatedEnv::new();
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    let socket = runtime_socket(&paths);
    let server = serve_pong_once(
        UnixListener::bind(&socket).expect("test precondition"),
        other_build_id(),
        true,
        false,
    );
    let status = running_server_status(&paths, || {}).expect("a stopping server answers");
    server.join().expect("fake server thread");
    assert!(status.is_none(), "it is already going: {status:?}");
    assert_nothing_was_launched(&paths);
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
            Step::Exit(crate::daemon_exit::CONFIG_REFUSED_EXIT_CODE),
        ],
        Duration::from_secs(10),
        || Ok(Probed::NoServer),
    );
    let error = result.expect_err("a dead daemon is a failed launch");
    assert_eq!(
        match &error {
            LaunchError::DaemonFailed { class, .. } => Some(*class),
            _ => None,
        },
        Some(crate::daemon_exit::DaemonExit::ConfigRefused)
    );
    let message = error.to_string();
    assert!(message.contains("refused its configuration"), "{message}");
    assert!(message.contains("no such runtime directory"), "{message}");
    assert!(
        message.contains(shepr_paths::BOOT_LOG_FILE_NAME),
        "{message}"
    );
    assert!(
        message.contains(shepr_paths::SERVER_LOG_FILE_NAME),
        "{message}"
    );
}

#[test]
fn a_daemon_that_gives_way_to_an_occupant_waits_for_the_occupant() {
    let dir = ScratchDir::new("launch-occupant");
    let calls = Cell::new(0_u32);
    let (result, _) = launch_fixture(
        &dir,
        &[Step::Exit(crate::daemon_exit::ALREADY_RUNNING_EXIT_CODE)],
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
    assert!(status.build_id.is_this_build());
}

#[test]
fn an_occupant_of_another_build_is_handed_back_once_the_daemon_gave_way() {
    let dir = ScratchDir::new("launch-occupant-mismatch");
    let calls = Cell::new(0_u32);
    let (result, _) = launch_fixture(
        &dir,
        &[Step::Exit(crate::daemon_exit::ALREADY_RUNNING_EXIT_CODE)],
        Duration::from_secs(10),
        || {
            calls.set(calls.get() + 1);
            // Long enough that the daemon has exited before the occupant
            // answers, and short of the interval that starts it again.
            Ok(if calls.get() < 4 {
                Probed::NoServer
            } else {
                Probed::Running(other_build())
            })
        },
    );
    let status = result.expect("the caller's policy decides about the occupant");
    assert!(!status.build_id.is_this_build());
}

#[test]
fn an_occupant_that_never_answers_ends_in_a_timeout() {
    let dir = ScratchDir::new("launch-occupant-silent");
    let (result, _) = launch_fixture(
        &dir,
        &[Step::Exit(crate::daemon_exit::ALREADY_RUNNING_EXIT_CODE)],
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

/// Runs `launch_with` where the first daemon gives way (as one that met a
/// lease still held by a stopping server does) and later ones idle; returns
/// the result, how many daemons were started, and the last daemon's group.
fn launch_after_a_refused_first_daemon(
    dir: &ScratchDir,
    timeout: Duration,
    probe: impl FnMut(u32) -> io::Result<Probed>,
) -> (Result<RuntimeStatus, LaunchError>, u32, FixtureDaemonGuard) {
    launch_after_a_refused_first_daemon_with(
        dir,
        timeout,
        probe,
        &mut Instant::now,
        &mut std::thread::sleep,
    )
}

fn launch_after_a_refused_first_daemon_with(
    dir: &ScratchDir,
    timeout: Duration,
    mut probe: impl FnMut(u32) -> io::Result<Probed>,
    now: &mut impl FnMut() -> Instant,
    sleep: &mut impl FnMut(Duration),
) -> (Result<RuntimeStatus, LaunchError>, u32, FixtureDaemonGuard) {
    let server = dir.join("shepr-server");
    let boot_log = shepr_paths::boot_log_path(dir.path());
    let server_log = shepr_paths::server_log_path(dir.path());
    let spawned = Cell::new(0_u32);
    let group = Cell::new(0_u32);
    let refused = [Step::Exit(crate::daemon_exit::ALREADY_RUNNING_EXIT_CODE)];
    let idle = idle_daemon_steps();
    let result = launch_with(
        &LaunchFiles {
            server: &server,
            boot_log: &boot_log,
            server_log: &server_log,
        },
        timeout,
        |stderr| {
            spawned.set(spawned.get() + 1);
            let steps = if spawned.get() == 1 {
                &refused[..]
            } else {
                &idle[..]
            };
            let mut child = fixture_daemon(steps, &group)(stderr)?;
            if spawned.get() == 1 {
                child.wait()?;
            }
            Ok(child)
        },
        || probe(spawned.get()),
        now,
        sleep,
    );
    (result, spawned.get(), FixtureDaemonGuard::new(group.get()))
}

#[test]
fn a_daemon_refused_by_a_leaving_holder_is_started_again_once_nothing_listens() {
    let dir = ScratchDir::new("launch-lease-outlives-socket");
    let (result, spawned, group) =
        launch_after_a_refused_first_daemon(&dir, Duration::from_secs(10), |spawned| {
            // The holder's socket is already gone; the second daemon, which
            // finds the lease free, is what answers.
            Ok(if spawned < 2 {
                Probed::NoServer
            } else {
                Probed::Running(this_build())
            })
        });
    let status = result.expect("the second daemon owns the directory and answers");
    assert!(status.build_id.is_this_build());
    assert_eq!(spawned, 2, "one restart, not one per poll");
    assert!(
        !group_is_gone(group.process_group()),
        "the answering daemon is kept running"
    );
}

#[test]
fn a_stopping_occupant_is_outlasted_rather_than_attached_to() {
    let dir = ScratchDir::new("launch-occupant-stopping");
    let calls = Cell::new(0_u32);
    let (result, spawned, group) =
        launch_after_a_refused_first_daemon(&dir, Duration::from_secs(10), |spawned| {
            calls.set(calls.get() + 1);
            // The occupant answers that it is stopping while its final save
            // runs, then its socket goes and the second daemon answers.
            Ok(if calls.get() <= 3 {
                Probed::Stopping
            } else if spawned < 2 {
                Probed::NoServer
            } else {
                Probed::Running(this_build())
            })
        });
    let status = result.expect("the daemon started after the occupant left answers");
    assert!(status.build_id.is_this_build());
    assert_eq!(status.lifecycle, crate::status::RuntimeLifecycle::Running);
    assert_eq!(
        spawned, 2,
        "the occupant's successor, not a daemon per poll"
    );
    assert!(
        !group_is_gone(group.process_group()),
        "the answering daemon is kept running"
    );
}

#[test]
fn a_refused_daemon_is_not_started_again_while_an_occupant_listens() {
    let dir = ScratchDir::new("launch-occupant-not-restarted");
    let (result, spawned, _) =
        launch_after_a_refused_first_daemon(&dir, DAEMON_RESTART_INTERVAL * 3, |_| {
            Ok(Probed::Unresponsive)
        });
    assert_eq!(
        result
            .expect_err("a silent occupant is never attached to")
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert_eq!(
        spawned, 1,
        "a listener is an occupant, not a leaving holder"
    );
}

#[test]
fn a_holder_that_never_leaves_ends_the_launch_in_a_timeout_with_restarts_paced() {
    let dir = ScratchDir::new("launch-lease-held-forever");
    let start = Instant::now();
    let elapsed = Cell::new(Duration::ZERO);
    let mut now = || start + elapsed.get();
    let mut sleep = |duration| elapsed.set(elapsed.get() + duration);
    let (result, spawned, group) = launch_after_a_refused_first_daemon_with(
        &dir,
        DAEMON_RESTART_INTERVAL * 3,
        |_| Ok(Probed::NoServer),
        &mut now,
        &mut sleep,
    );
    let error = result.expect_err("the launch stays bounded");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    // Only a daemon that gave way is started again: the refused first one is
    // replaced once the restart interval has passed, and its idling successor
    // is waited on until the deadline rather than replaced.
    assert_eq!(spawned, 2, "one paced restart, not one per poll");
    assert_eq!(elapsed.get(), DAEMON_RESTART_INTERVAL * 3);
    assert_group_dies(group.process_group());
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
    assert_group_dies(group.process_group());
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
    assert_group_dies(group.process_group());
}

/// Runs `launch_with` against the fixture daemon in `dir` with a probe that
/// answers as a server of another build whose boot id names `answering_pid`
/// (given the spawned daemon's pid, 0 before the spawn).
fn launch_against_other_build(
    dir: &ScratchDir,
    steps: &[Step],
    timeout: Duration,
    answering_pid: impl Fn(u32) -> u32,
) -> (Result<RuntimeStatus, LaunchError>, FixtureDaemonGuard) {
    let server = dir.join("shepr-server");
    let boot_log = shepr_paths::boot_log_path(dir.path());
    let server_log = shepr_paths::server_log_path(dir.path());
    let pid = Cell::new(0);
    let result = launch_with(
        &LaunchFiles {
            server: &server,
            boot_log: &boot_log,
            server_log: &server_log,
        },
        timeout,
        fixture_daemon(steps, &pid),
        || {
            Ok(Probed::Running(RuntimeStatus {
                boot_id: shepr_protocol::BootId::from_process_clock(
                    answering_pid(pid.get()),
                    Ok(Duration::from_secs(1_700_000_000)),
                ),
                ..other_build()
            }))
        },
        &mut Instant::now,
        &mut std::thread::sleep,
    );
    (result, FixtureDaemonGuard::new(pid.get()))
}

#[test]
fn a_sibling_of_another_build_is_killed_and_reported() {
    let dir = ScratchDir::new("launch-sibling-mismatch");
    let (result, group) = launch_against_other_build(
        &dir,
        &idle_daemon_steps(),
        Duration::from_secs(10),
        |daemon| daemon,
    );
    let message = result
        .expect_err("a server of another build is not this client's")
        .to_string();
    assert!(message.contains("different build"), "{message}");
    assert!(message.contains("install"), "{message}");
    assert_group_dies(group.process_group());
}

#[test]
fn another_builds_server_answering_for_a_live_daemon_is_not_blamed_on_it() {
    let dir = ScratchDir::new("launch-external-occupant");
    // The daemon is still booting when a server it did not start answers; it
    // then gives way on its own, as one that finds the socket taken does.
    let (result, group) = launch_against_other_build(
        &dir,
        &[Step::Sleep(Duration::from_millis(300))],
        Duration::from_secs(10),
        |daemon| daemon.wrapping_add(1),
    );
    let status = result.expect("the external occupant is handed back to the caller");
    assert_eq!(status.build_id.to_string(), other_build_id());
    assert_ne!(group.process_group(), 0);
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
    assert_ne!(group.process_group(), 0);
    assert!(
        !group_is_gone(group.process_group()),
        "the launched daemon must keep running"
    );
    let process_group = group.process_group();
    drop(group);
    assert_group_dies(process_group);
}

#[test]
fn a_symlinked_boot_log_refuses_the_launch_before_spawning() {
    let dir = ScratchDir::new("launch-boot-log-link");
    let target = dir.join("target");
    std::fs::write(&target, b"keep").expect("test precondition");
    let boot_log = shepr_paths::boot_log_path(dir.path());
    std::os::unix::fs::symlink(&target, &boot_log).expect("plant a link");
    let server = dir.join("shepr-server");
    let server_log = shepr_paths::server_log_path(dir.path());

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

/// The server socket with its runtime directory created.
fn runtime_socket(paths: &shepr_paths::AppPaths) -> PathBuf {
    std::fs::create_dir_all(paths.runtime_dir()).expect("create runtime");
    paths.server_address().socket().to_path_buf()
}

fn assert_nothing_was_launched(paths: &shepr_paths::AppPaths) {
    for path in [paths.launch_lock_path(), paths.boot_log_path()] {
        assert!(
            !path.try_exists().expect("stat the runtime file"),
            "{} must not exist: no launch was attempted",
            path.display()
        );
    }
}

#[test]
fn a_socket_override_never_starts_a_server() {
    let env = IsolatedEnv::new();
    let socket = env.path().join("named.sock");
    env.set(EnvVar::SheprSocketPath, &socket);
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");

    let error = ensure_running(&paths, Duration::from_secs(1), BuildCheck::BeforeAttach)
        .expect_err("an override only reaches a running server");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let message = error.to_string();
    assert!(message.contains("no shepr server is running"), "{message}");
    assert!(message.contains("SHEPR_SOCKET_PATH"), "{message}");
    assert_nothing_was_launched(&paths);
}

#[test]
fn a_starting_server_at_a_socket_override_is_waited_on_not_refused() {
    let env = IsolatedEnv::new();
    let socket = env.path().join("starting.sock");
    env.set(EnvVar::SheprSocketPath, &socket);
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    let (release, server) = serve_starting_until_released(&socket);

    let error = ensure_running(&paths, Duration::from_millis(300), BuildCheck::BeforeAttach)
        .expect_err("a server that never finishes starting times out");
    release.send(()).expect("release");
    server.join().expect("server");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    assert!(
        error.to_string().contains("did not finish starting"),
        "{error}"
    );
    assert_nothing_was_launched(&paths);
}

#[test]
fn a_listener_that_does_not_answer_is_never_replaced() {
    let _env = IsolatedEnv::new();
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    let socket = runtime_socket(&paths);
    let _listener = UnixListener::bind(&socket).expect("test precondition");

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
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    let socket = runtime_socket(&paths);
    let server = serve_status_once(
        UnixListener::bind(&socket).expect("test precondition"),
        shepr_protocol::BUILD_ID,
    );

    let ready = ensure_running(&paths, Duration::from_secs(1), BuildCheck::BeforeAttach)
        .expect("a healthy server of this build is used");
    assert!(ready.boot_notice.is_none(), "nothing was launched");
    server.join().expect("fake server thread");
    assert_nothing_was_launched(&paths);
}

#[test]
fn a_running_server_of_another_build_is_refused_before_attaching() {
    let _env = IsolatedEnv::new();
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    let socket = runtime_socket(&paths);
    let server = serve_status_once(
        UnixListener::bind(&socket).expect("test precondition"),
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
fn a_mismatched_server_at_a_socket_override_is_not_promised_a_restart() {
    let env = IsolatedEnv::new();
    let socket = env.path().join("custom.sock");
    env.set(EnvVar::SheprSocketPath, &socket);
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    let server = serve_status_once(
        UnixListener::bind(&socket).expect("test precondition"),
        other_build_id(),
    );

    let error = ensure_running(&paths, Duration::from_secs(1), BuildCheck::BeforeAttach)
        .expect_err("a different build remains incompatible at an override");
    server.join().expect("fake server thread");
    let message = error.to_string();
    assert!(
        message.contains("cannot start a replacement at the selected socket override"),
        "{message}"
    );
    assert!(message.contains("SHEPR_SOCKET_PATH"), "{message}");
    assert!(
        !message.contains("restart it before attaching"),
        "{message}"
    );
    assert_nothing_was_launched(&paths);
}

#[test]
fn ensure_running_hands_back_a_running_mismatch_for_the_bridge() {
    let _env = IsolatedEnv::new();
    let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
    let socket = runtime_socket(&paths);
    let server = serve_status_once(
        UnixListener::bind(&socket).expect("test precondition"),
        other_build_id(),
    );

    let ready = ensure_running(
        &paths,
        Duration::from_secs(1),
        BuildCheck::AtClientHandshake,
    )
    .expect("the typed handshake reports the mismatch, not the launcher");
    assert_eq!(ready.status.build_id.to_string(), other_build_id());
    assert!(ready.boot_notice.is_none(), "nothing was launched");
    server.join().expect("fake server thread");
    assert_nothing_was_launched(&paths);
}

// ---------------------------------------------------------------------------
// The daemon command
// ---------------------------------------------------------------------------

#[test]
fn server_daemon_command_marks_the_client_spawn_and_nothing_else() {
    let command = build_server_daemon_command(
        Path::new("/nonexistent/shepr-server-test"),
        Path::new("/"),
        None,
    );
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(args, [OsStr::new(crate::invocation::CLIENT_SPAWNED_FLAG)]);
}

#[test]
fn server_daemon_command_passes_current_dir_as_startup_cwd() {
    let expected = Path::new("/nonexistent/test-home");
    let command = build_server_daemon_command(
        Path::new("/nonexistent/shepr-test"),
        Path::new("/"),
        Some(expected),
    );
    let envs: Vec<_> = command.get_envs().collect();

    assert!(envs.iter().any(|(key, value)| {
        *key == OsStr::new(EnvVar::SheprStartupCwd.name()) && value == &Some(expected.as_os_str())
    }));
}

#[test]
fn server_daemon_runs_in_its_home_not_the_launch_directory() {
    let scratch = ScratchDir::new("daemon-working-dir");
    let home_dir = scratch.join("home");
    let launch_dir = scratch.join("launch");
    let paths =
        shepr_paths::AppPaths::rooted_at(scratch.path(), Some(&home_dir), Some(&launch_dir))
            .expect("test paths");
    let command = build_server_daemon_command(
        Path::new("/nonexistent/shepr-test"),
        &server_daemon_working_dir(&paths),
        Some(&launch_dir),
    );
    assert_eq!(command.get_current_dir(), Some(home_dir.as_path()));
    assert_ne!(command.get_current_dir(), Some(launch_dir.as_path()));
}

#[test]
fn every_launch_failure_reaches_a_remote_client_with_its_operator_action() {
    use RemoteFailureClass::{Repair, Retry};
    let daemon_failed = |class: DaemonExit| LaunchError::DaemonFailed {
        class,
        message: format!("shepr-server {}", class.describe_boot_end()),
    };
    let message = || "detail".to_owned();
    for (error, class) in [
        (LaunchError::Unresponsive { message: message() }, Repair),
        (LaunchError::DifferentBuild { message: message() }, Repair),
        (LaunchError::OverrideMissing { message: message() }, Repair),
        (LaunchError::TransitionTimeout { message: message() }, Retry),
        (daemon_failed(DaemonExit::ConfigRefused), Repair),
        (daemon_failed(DaemonExit::Failed), Repair),
        (daemon_failed(DaemonExit::Clean), Retry),
        (daemon_failed(DaemonExit::AlreadyRunning), Retry),
        (LaunchError::BootLogOverflow { message: message() }, Repair),
        (LaunchError::BootTimeout { message: message() }, Retry),
        (
            LaunchError::SiblingBuildMismatch { message: message() },
            Repair,
        ),
        (
            LaunchError::Executable(io::Error::from(io::ErrorKind::NotFound)),
            Repair,
        ),
        (
            LaunchError::LaunchLock(io::Error::from(io::ErrorKind::PermissionDenied)),
            Repair,
        ),
        (
            LaunchError::LaunchLock(io::Error::from(io::ErrorKind::TimedOut)),
            Retry,
        ),
        (
            LaunchError::Io(io::Error::from(io::ErrorKind::PermissionDenied)),
            Repair,
        ),
        (
            LaunchError::Io(io::Error::from(io::ErrorKind::TimedOut)),
            Retry,
        ),
        (
            LaunchError::Io(io::Error::from(io::ErrorKind::ConnectionReset)),
            Retry,
        ),
        (
            LaunchError::Io(io::Error::from(io::ErrorKind::ConnectionRefused)),
            Retry,
        ),
        (
            LaunchError::Io(io::Error::from(io::ErrorKind::ConnectionAborted)),
            Retry,
        ),
        (
            LaunchError::Io(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Retry,
        ),
        (
            LaunchError::Io(io::Error::from(io::ErrorKind::BrokenPipe)),
            Retry,
        ),
    ] {
        assert_eq!(error.remote_failure_class(), class, "{error:?}");
    }
}

#[test]
fn an_undecodable_older_status_keeps_stop_guidance() {
    let scratch = ScratchDir::new("launch-old-pong");
    let paths = shepr_paths::AppPaths::test_at(scratch.path());
    std::fs::create_dir_all(paths.runtime_dir()).expect("runtime directory");
    let socket = paths.server_address().socket().to_path_buf();
    let listener = shepr_platform::ipc::bind_private_local_listener(&socket).expect("bind");
    let server = std::thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut line)
                .expect("request");
            if line.is_empty() {
                continue;
            }
            // Older status answers have no boot identity and cannot be used
            // for a conditional restart, but operators still need stop guidance.
            let response = serde_json::json!({
                "id": shepr_api::schema::RequestId::StatusPing.as_str(),
                "result": {"type": "pong", "version": "0.0.0", "build_id": "0123456789abcdef"}
            });
            writeln!(stream, "{response}").expect("response");
            break;
        }
    });
    let error = probe_server(&paths).expect_err("boot identity required");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    let message = error.to_string();
    assert!(message.contains("did not give a usable status answer"));
    assert!(message.contains(&guidance::build_mismatch_guidance(paths.server_address())));
    server.join().expect("server");
}

#[test]
fn a_ready_daemon_keeps_what_it_left_in_the_boot_log() {
    let dir = ScratchDir::new("launch-ready-boot-log");
    let boot_log = shepr_paths::boot_log_path(dir.path());
    let calls = Cell::new(0_u32);
    let (result, _group) =
        launch_fixture(&dir, &idle_daemon_steps(), Duration::from_secs(10), || {
            calls.set(calls.get() + 1);
            if calls.get() < 2 {
                return Ok(Probed::NoServer);
            }
            // What a server whose own log file could not be opened writes
            // before it answers.
            let mut log = std::fs::OpenOptions::new()
                .append(true)
                .open(&boot_log)
                .expect("the launch opened the boot log");
            log.write_all(b"logs: unavailable, could not open the server log\n")
                .expect("write the ready notice");
            Ok(Probed::Running(this_build()))
        });
    result.expect("the daemon answers");
    let kept = std::fs::read_to_string(&boot_log).expect("read the boot log");
    assert!(kept.contains("logs: unavailable"), "{kept}");
    // What ensure_running hands the TUI to print before it takes the terminal.
    let notice = ready_boot_notice(&boot_log).expect("a non-empty boot log is reported");
    assert!(notice.contains("logs: unavailable"), "{notice}");
    assert!(notice.contains(&boot_log.display().to_string()), "{notice}");
    std::fs::write(&boot_log, b"").expect("empty the boot log");
    assert_eq!(
        ready_boot_notice(&boot_log),
        None,
        "an empty boot log says nothing"
    );
}
