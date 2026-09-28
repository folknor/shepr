//! The fixture program as tests elsewhere use it. Being an integration test
//! is also what makes cargo build the binary whenever this package is tested.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use shepr_test_support::ScratchDir;
use shepr_test_support::fixture::{self, Held, Signal, Step};

fn stdout_of(mut command: std::process::Command) -> (std::process::ExitStatus, String) {
    let output = command.output().expect("the fixture runs");
    (
        output.status,
        String::from_utf8(output.stdout).expect("UTF-8 output"),
    )
}

#[test]
fn tests_find_the_fixture_cargo_built() {
    assert_eq!(
        fixture::path(),
        Path::new(env!("CARGO_BIN_EXE_shepr-fixture"))
    );
}

#[test]
fn a_direct_script_runs_in_order_and_sets_the_exit_code() {
    let mut command = fixture::command(&[
        Step::Print("a ".into()),
        Step::PrintArg(2),
        Step::Print("\n".into()),
        Step::PrintArgs,
        Step::Exit(7),
        Step::Print("unreached".into()),
    ]);
    command.args(["--", "one", "two"]);
    let (status, stdout) = stdout_of(command);
    assert_eq!(status.code(), Some(7));
    assert_eq!(stdout, "a two\none\ntwo\n");
}

#[test]
fn a_malformed_script_fails_with_the_fixture_code() {
    let mut command = std::process::Command::new(fixture::path());
    command.arg("no-such-step").stderr(Stdio::piped());
    let output = command.output().expect("the fixture runs");
    assert_eq!(output.status.code(), Some(fixture::FIXTURE_FAILURE_EXIT));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown step"));
}

/// A stand-in takes its script from beside itself and its operands from the
/// argv it was started with, whether it is reached by path or through `PATH`.
#[test]
fn a_stand_in_runs_its_script_on_whatever_argv_it_is_given() {
    let dir = ScratchDir::new("fixture-stand-in");
    let program = fixture::stand_in(
        &dir,
        "remote-shepr",
        &[
            Step::When {
                operands: vec!["status".into(), "--json".into()],
                steps: vec![Step::Print("{}".into()), Step::Exit(0)],
            },
            Step::PrintArgs,
            Step::Exit(64),
        ],
    );

    let mut by_path = std::process::Command::new(&program);
    by_path.args(["status", "--json"]);
    assert_eq!(stdout_of(by_path), (success(), "{}".to_owned()));

    let mut by_name = std::process::Command::new("remote-shepr");
    by_name.arg("other").env("PATH", dir.path());
    let (status, stdout) = stdout_of(by_name);
    assert_eq!(status.code(), Some(64));
    assert_eq!(stdout, "other\n");
}

fn success() -> std::process::ExitStatus {
    std::os::unix::process::ExitStatusExt::from_raw(0)
}

/// The kernel names a stand-in's process after the stand-in, which is what
/// process detection reads.
#[test]
fn a_stand_in_process_carries_its_own_name() {
    let dir = ScratchDir::new("fixture-name");
    let program = fixture::stand_in(&dir, "droid", &[Step::Sleep(Duration::from_secs(30))]);
    let mut child = std::process::Command::new(&program)
        .arg("999")
        .spawn()
        .expect("the stand-in starts");
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", child.id()));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(comm.expect("the process is visible").trim_end(), "droid");
}

#[test]
fn output_can_be_pointed_at_a_file_and_stdin_copied() {
    use std::io::Write as _;

    let dir = ScratchDir::new("fixture-to");
    let target = dir.join("out");
    let mut child = fixture::command(&[Step::To(target.clone()), Step::Cat, Step::PrintPid])
        .stdin(Stdio::piped())
        .spawn()
        .expect("the fixture runs");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"payload:")
        .expect("stdin accepts the payload");
    let pid = child.id();
    assert!(child.wait().expect("the fixture exits").success());
    assert_eq!(
        std::fs::read_to_string(target).expect("the file was written"),
        format!("payload:{pid}")
    );
}

#[test]
fn a_session_leader_reports_its_own_pid_as_its_session() {
    use std::os::unix::process::CommandExt as _;

    let mut command = fixture::command(&[Step::PrintSid]);
    // SAFETY: setsid(2) is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .stdout(Stdio::piped())
        .spawn()
        .expect("the fixture runs");
    let pid = child.id();
    let output = child.wait_with_output().expect("the fixture exits");
    assert_eq!(String::from_utf8_lossy(&output.stdout), pid.to_string());
}

/// A child holding only stderr leaves stdout to reach end of output as soon
/// as the fixture exits.
#[test]
fn a_spawned_child_holds_only_the_streams_it_is_given() {
    let mut child = fixture::command(&[
        Step::Spawn {
            argv0: "holder".into(),
            sleep: Duration::from_secs(2),
            held: Held::Stderr,
        },
        Step::Print("done".into()),
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("the fixture runs");
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().expect("piped stdout"), &mut stdout)
        .expect("stdout reaches end of output");
    assert!(child.wait().expect("the fixture exits").success());
    assert_eq!(stdout, "done");
}

#[test]
fn a_raised_kill_is_a_signal_death() {
    use std::os::unix::process::ExitStatusExt as _;

    let status = fixture::command(&[Step::Raise(Signal::Kill)])
        .status()
        .expect("the fixture runs");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
}
