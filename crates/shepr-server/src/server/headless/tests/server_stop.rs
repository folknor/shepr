//! `server.stop` against an idle running server. The server installs the
//! process-wide file logger and signal handler, so the run happens in a
//! re-executed test process instead of the shared test binary.

use std::time::Duration;

use crate::server::headless::run_server;
use crate::test_support::{
    AppPathsFixture as _, IsolatedEnv, ScratchDir, ValidatedConfigFixture as _,
};

const CHILD_MARKER: &str = "SERVER_STOP_TEST_CHILD";
const ENTRY_POINT: &str =
    "server::headless::tests::server_stop_tests::idle_server_stop_subprocess_entry_point";

/// Far below the stopping client's own wait, and far above what an idle
/// server needs to shut down once it notices the request.
const STOP_BUDGET: Duration = Duration::from_secs(5);

#[test]
fn an_idle_server_acts_on_server_stop_at_once() {
    let output = shepr_test_support::command_in_scratch(
        std::env::current_exe().expect("test executable"),
        "idle-server-stop",
    )
    .args(["--exact", ENTRY_POINT, "--ignored", "--nocapture"])
    .env(CHILD_MARKER, "1")
    .output()
    .expect("run the server in an isolated test process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "child exited with {}:\n{stdout}\n{stderr}",
        output.status
    );
    // A filter that matched nothing would pass without running anything.
    assert!(
        stdout.contains("1 passed"),
        "child ran no test:\n{stdout}\n{stderr}"
    );
}

#[test]
#[ignore = "subprocess entry point, exercised by an_idle_server_acts_on_server_stop_at_once"]
fn idle_server_stop_subprocess_entry_point() {
    // `brokkr test` passes `--include-ignored`, which runs this entry point
    // directly in the shared test process. Only the re-exec sets the marker.
    #[expect(
        clippy::disallowed_methods,
        reason = "the marker is this test's own re-exec harness probe, not a shepr variable"
    )]
    if std::env::var_os(CHILD_MARKER).is_none() {
        return;
    }

    let _env = IsolatedEnv::new();
    let scratch = ScratchDir::new("idle-server-stop");
    let paths = shepr_config::AppPaths::test_at(&scratch);
    let config = shepr_config::ValidatedConfig::test_from_config_with_paths(
        shepr_config::Config::default(),
        None,
        paths.clone(),
    );

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = run_server(&config, &paths, |ready| {
            // The stop goes out once the server is up and has nothing else
            // to do, so only the stop request itself can wake its loop.
            let api_socket = ready.api_socket.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                let request = shepr_api::schema::Request {
                    id: "idle-stop".into(),
                    method: shepr_api::schema::Method::ServerStop(
                        shepr_api::schema::ServerStopParams::default(),
                    ),
                };
                shepr_api::client::ApiClient::for_socket(api_socket)
                    .request(&request)
                    .expect("the server accepts the stop");
            });
        });
        // The receiver is gone only once the test has already failed.
        drop(done_tx.send(result.map_err(|error| error.to_string())));
    });

    let result = done_rx
        .recv_timeout(STOP_BUDGET)
        .expect("the idle server kept running after server.stop");
    result.expect("the server stopped cleanly");
}
