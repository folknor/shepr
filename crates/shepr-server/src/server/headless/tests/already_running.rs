//! `run_server` against a socket or session data lease another server already
//! holds. The server installs the process-wide file logger, so each run
//! happens in a re-executed test process instead of the shared test binary.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::server::headless::{RunServerError, ServerSocket, run_server};
use crate::server::socket_paths::client_socket_path;
use crate::test_support::{
    AppPathsFixture as _, IsolatedEnv, ScratchDir, ValidatedConfigFixture as _,
};

/// Names what the re-executed child holds before it starts a server: a
/// socket, or the session data-directory lease.
const CHILD_MARKER: &str = "SERVER_ALREADY_RUNNING_TEST_CHILD";
const ENTRY_POINT: &str =
    "server::headless::tests::already_running_tests::already_running_subprocess_entry_point";

#[test]
fn run_server_refuses_a_busy_socket_or_session_lease_as_already_running() {
    for held in ["api", "client", "session"] {
        let output = shepr_test_support::command_in_scratch(
            std::env::current_exe().expect("test executable"),
            "already-running",
        )
        .args(["--exact", ENTRY_POINT, "--ignored", "--nocapture"])
        .env(CHILD_MARKER, held)
        .output()
        .expect("run the server in an isolated test process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{held} child exited with {}:\n{stdout}\n{stderr}",
            output.status
        );
        // A filter that matched nothing would pass without running anything.
        assert!(
            stdout.contains("1 passed"),
            "{held} child ran no test:\n{stdout}\n{stderr}"
        );
    }
}

#[test]
#[ignore = "subprocess entry point, exercised by run_server_refuses_a_busy_socket_or_session_lease_as_already_running"]
fn already_running_subprocess_entry_point() {
    // `brokkr test` passes `--include-ignored`, which runs this entry point
    // directly in the shared test process. Only the re-exec sets the marker.
    #[expect(
        clippy::disallowed_methods,
        reason = "the marker is this test's own re-exec harness probe, not a shepr variable"
    )]
    let Some(marker) = std::env::var_os(CHILD_MARKER) else {
        return;
    };
    let busy = match marker.to_str() {
        Some("api") => ServerSocket::Api,
        Some("client") => ServerSocket::Client,
        Some("session") => return refuse_a_held_session_lease(),
        other => panic!("unknown {CHILD_MARKER} value {other:?}"),
    };

    let _env = IsolatedEnv::new();
    let scratch = ScratchDir::new("already-running-server");
    let paths = shepr_config::AppPaths::test_at(&scratch);
    let config = shepr_config::ValidatedConfig::test_from_config_with_paths(
        shepr_config::Config::default(),
        None,
        paths.clone(),
    );
    let api_socket = shepr_api::socket_path(&paths);
    let client_socket = client_socket_path(&paths);
    let held_path = match busy {
        ServerSocket::Api => &api_socket,
        ServerSocket::Client => &client_socket,
    };
    // What a running server holds: the startup lock and a live listener.
    let _held = shepr_platform::ipc::bind_private_socket(held_path).expect("hold the socket");

    let ready = AtomicBool::new(false);
    let error = run_server(&config, &paths, |_| ready.store(true, Ordering::Relaxed))
        .expect_err("a server holding the socket refuses the second");

    match error {
        RunServerError::AlreadyRunning { socket, path } => {
            assert_eq!(socket, busy);
            assert_eq!(&path, held_path);
        }
        other => panic!("expected AlreadyRunning for the {busy}, got {other:?}"),
    }
    assert!(
        !ready.load(Ordering::Relaxed),
        "a refused server never reports ready"
    );
    if busy == ServerSocket::Client {
        // The API socket bound before the refusal is released with it.
        assert!(
            !api_socket.try_exists().expect("stat the api socket"),
            "the refused server left its api socket behind"
        );
    }
}

/// The session data lease is taken before either socket, so a server that
/// finds it held refuses without binding anything.
fn refuse_a_held_session_lease() {
    let _env = IsolatedEnv::new();
    let scratch = ScratchDir::new("already-running-session");
    let paths = shepr_config::AppPaths::test_at(&scratch);
    let config = shepr_config::ValidatedConfig::test_from_config_with_paths(
        shepr_config::Config::default(),
        None,
        paths.clone(),
    );
    // What a running server holds: the lease on the session data directory.
    let held = shepr_mux::persist::DataDirLease::acquire(&shepr_api::session::data_dir(&paths))
        .expect("hold the session lease");

    let ready = AtomicBool::new(false);
    let error = run_server(&config, &paths, |_| ready.store(true, Ordering::Relaxed))
        .expect_err("a server holding the session lease refuses the second");

    match error {
        RunServerError::SessionDataHeld { directory } => {
            assert_eq!(directory, held.directory());
        }
        other => panic!("expected SessionDataHeld, got {other:?}"),
    }
    assert!(
        !ready.load(Ordering::Relaxed),
        "a refused server never reports ready"
    );
    for socket in [shepr_api::socket_path(&paths), client_socket_path(&paths)] {
        assert!(
            !socket.try_exists().expect("stat the socket"),
            "the refused server bound {}",
            socket.display()
        );
    }
}
