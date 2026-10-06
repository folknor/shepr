//! Runs a shipped agent hook against a stand-in API socket, for the agent and
//! server tests whose subject is the hook script itself.
//!
//! It lives here rather than in `shepr-test-fixtures` because it needs nothing
//! but the standard library, and the agent crate's own tests take it: the
//! fixtures crate depends on `shepr-config`, which depends on `shepr-agent`, so
//! those tests could not take it without linking a second copy of the agent
//! crate.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::Duration;

/// How long the stand-in socket waits for an accepted hook's request line.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(1);

/// How long the stand-in socket sleeps between polls of an empty backlog.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Variables an agent sets in the processes it runs, which the shipped hooks
/// read to decide whose session they report. A test run from inside one of
/// those agents would inherit them: under Cursor or inside a Claude background
/// session the Claude hook stays silent, Grok's id overrides the scripted one,
/// inside a Codex thread the Codex hook drops every report for another
/// session, and a nested OMP extension stays passive.
pub const INHERITED_AGENT_VARIABLES: [&str; 6] = [
    "CLAUDE_CODE_SESSION_KIND",
    "CLAUDE_JOB_DIR",
    "CODEX_THREAD_ID",
    "CURSOR_VERSION",
    "GROK_SESSION_ID",
    "OMPCODE",
];

/// The exit result and newline-framed requests captured from one hook process.
pub struct HookCapture {
    /// The hook process exit status.
    pub status: ExitStatus,
    /// Bytes written to the hook's stderr.
    pub stderr: Vec<u8>,
    /// API request lines captured from the hook's socket connections, each
    /// with its trailing newline.
    pub requests: Vec<String>,
}

/// Runs `command` (a hook asset invocation) with `input` on stdin, the shepr
/// pane environment pointing at a stand-in API socket bound at `socket_path`,
/// `TMPDIR` at `scratch_dir`, and [`INHERITED_AGENT_VARIABLES`] removed. Each
/// connection gets an empty JSON reply.
///
/// A hook that never connects cannot block the call: the socket is polled,
/// and the poll stops once the hook has exited and the backlog is drained. A
/// hook that connects but sends no line fails the call after
/// [`REQUEST_READ_DEADLINE`].
///
/// # Panics
///
/// When the socket cannot be bound, the hook cannot be started, or a captured
/// request is not line framed.
pub fn capture_hook(
    mut command: Command,
    socket_path: &Path,
    scratch_dir: &Path,
    pane_id: &str,
    input: &[u8],
) -> HookCapture {
    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove old hook socket {}: {error}", socket_path.display()),
    }
    let listener = UnixListener::bind(socket_path)
        .unwrap_or_else(|error| panic!("bind hook socket {}: {error}", socket_path.display()));
    listener
        .set_nonblocking(true)
        .expect("make hook socket nonblocking");
    let (stop, stopped) = mpsc::channel();
    let server = thread::spawn(move || capture_requests(&listener, &stopped));

    command
        .env(
            shepr_core::env::EnvVar::SheprEnv.name(),
            shepr_core::env::SHEPR_ENV_IN_PANE,
        )
        .env(shepr_core::env::EnvVar::SheprSocketPath.name(), socket_path)
        .env(shepr_core::env::ChildEnv::SheprPaneId.name(), pane_id)
        .env("TMPDIR", scratch_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for name in INHERITED_AGENT_VARIABLES {
        command.env_remove(name);
    }
    let mut child = command.spawn().expect("start hook asset");
    let mut stdin = child.stdin.take().expect("hook stdin is piped");
    // A hook that rejects its arguments exits before reading its input.
    if let Err(error) = stdin.write_all(input) {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe,
            "write hook input: {error}"
        );
    }
    drop(stdin);
    let output = child.wait_with_output().expect("wait for hook asset");

    stop.send(()).expect("stop fake API socket");
    let requests = server.join().expect("join fake API socket thread");
    std::fs::remove_file(socket_path).expect("remove fake API socket");
    HookCapture {
        status: output.status,
        stderr: output.stderr,
        requests,
    }
}

fn capture_requests(listener: &UnixListener, stopped: &mpsc::Receiver<()>) -> Vec<String> {
    // A hook writes before it exits, even when its reply wait times out, so
    // the backlog is drained before the stop is honoured. A panicking caller
    // drops the sender, which also ends the loop.
    let mut requests = Vec::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("make accepted stream blocking");
                stream
                    .set_read_timeout(Some(REQUEST_READ_DEADLINE))
                    .expect("set fake socket read deadline");
                let mut line = String::new();
                BufReader::new(stream.try_clone().expect("clone accepted stream"))
                    .read_line(&mut line)
                    .expect("read captured hook request");
                assert!(line.ends_with('\n'), "hook request was not line framed");
                requests.push(line);
                // A hook whose reply wait already timed out has closed its
                // end; its request is captured, and the reply has no reader.
                if let Err(error) = stream.write_all(b"{}\n") {
                    assert!(
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        ),
                        "write fake API reply: {error}"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                match stopped.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                }
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => panic!("accept fake API connection: {error}"),
        }
    }
    requests
}
