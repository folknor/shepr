//! The SIGWINCH handler test. It installs a process-wide signal handler, so it
//! runs in a re-executed test process instead of the shared test binary.

use super::*;

#[test]
fn terminal_resize_signal_is_recorded_once_per_delivery() {
    use std::process::Stdio;

    let mut child = shepr_test_support::command_in_scratch(
        std::env::current_exe().expect("test executable"),
        "terminal-resize-signal",
    );
    let status = child
        .args([
            "--exact",
            "resize_signal_tests::terminal_resize_signal_subprocess_entry_point",
            "--ignored",
            "--nocapture",
        ])
        .env("PLATFORM_RESIZE_SIGNAL_TEST_CHILD", "1")
        .stdout(Stdio::null())
        .status()
        .expect("run signal test in an isolated test process");
    assert!(status.success(), "signal subprocess exited with {status}");
}

#[test]
#[ignore = "subprocess entry point, exercised by the signal test"]
#[expect(
    clippy::disallowed_methods,
    reason = "PLATFORM_RESIZE_SIGNAL_TEST_CHILD is this test's own re-exec harness marker"
)]
fn terminal_resize_signal_subprocess_entry_point() {
    // `brokkr test` passes `--include-ignored`, which runs this entry point
    // directly with no parent. Only the re-exec sets the marker.
    if std::env::var_os("PLATFORM_RESIZE_SIGNAL_TEST_CHILD").is_none() {
        return;
    }

    watch_terminal_resize_signal();
    assert!(!take_terminal_resize_signal());

    // SAFETY: raise(3) delivers SIGWINCH to this thread; the handler that
    // `watch_terminal_resize_signal` installed only stores an atomic.
    unsafe {
        libc::raise(libc::SIGWINCH);
    }

    assert!(take_terminal_resize_signal());
    assert!(!take_terminal_resize_signal());
}
