use std::collections::VecDeque;
use std::io::{self, Read};
use std::process::Output;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::limits::{
    PIPE_DRAIN_GRACE, SSH_CHILD_PROCESS_POLL_INTERVAL, SSH_PIPE_READ_BUFFER_BYTES,
    SSH_STDERR_CAPTURE_LIMIT, SSH_STDOUT_CAPTURE_LIMIT,
};

#[derive(Clone, Copy)]
enum CaptureRetention {
    Head,
    Tail,
}

/// Reads a child pipe on its own thread. OpenSSH redirects the detached
/// ControlPersist master's standard streams, and ProxyCommand stderr, to
/// `/dev/null` unless debugging is enabled. An enabled user `LocalCommand` runs
/// before that redirect, though, and a background descendant can retain these
/// pipes. `finish` bounds the caller's wait and returns a partial capture, but
/// cannot close a pipe held by that descendant, so its reader thread remains
/// blocked until the inherited descriptor closes.
pub(crate) struct PipeCapture {
    captured: Arc<Mutex<VecDeque<u8>>>,
    done: mpsc::Receiver<io::Result<()>>,
    context: Option<(u32, &'static str)>,
}

impl PipeCapture {
    pub(crate) fn spawn(reader: impl Read + Send + 'static, limit: usize) -> Self {
        Self::spawn_with_retention(reader, limit, CaptureRetention::Head)
    }

    /// Retains trailing command output so a large login banner cannot hide its result.
    pub(crate) fn spawn_tail(reader: impl Read + Send + 'static, limit: usize) -> Self {
        Self::spawn_with_retention(reader, limit, CaptureRetention::Tail)
    }

    fn spawn_with_retention(
        reader: impl Read + Send + 'static,
        limit: usize,
        retention: CaptureRetention,
    ) -> Self {
        // Grows with the output: the stdout limit is 1 MiB and most commands
        // print a few lines, so reserving the limit up front would waste it.
        let captured = Arc::new(Mutex::new(VecDeque::new()));
        let worker_captured = Arc::clone(&captured);
        // limits-exempt: one-shot completion protocol; the reader sends exactly
        // one result and must never wait for its consumer to send it.
        let (done_tx, done) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = read_into(reader, &worker_captured, limit, retention);
            // The receiver is gone only when `finish` gave up on this reader after
            // its grace, or the capture was dropped unfinished; either way nobody
            // is left to want the result.
            drop(done_tx.send(result));
        });
        Self {
            captured,
            done,
            context: None,
        }
    }

    pub(crate) fn with_context(mut self, child_pid: u32, pipe: &'static str) -> Self {
        self.context = Some((child_pid, pipe));
        self
    }

    /// Waits up to `grace` for the pipe to reach end of stream and returns what
    /// was read. A background descendant that still holds the pipe is left to
    /// finish on its own, and the partial capture is returned after the grace.
    pub(crate) fn finish(self, grace: Duration) -> io::Result<Vec<u8>> {
        match self.done.recv_timeout(grace) {
            Ok(result) => result?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::debug!(
                    bridge_pid = std::process::id(),
                    child_pipe = ?self.context,
                    "ssh pipe is still open after the child exited; another process holds it"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("ssh pipe reader panicked"));
            }
        }
        let captured = self.captured.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(captured.iter().copied().collect())
    }
}

fn read_into(
    mut reader: impl Read,
    captured: &Mutex<VecDeque<u8>>,
    limit: usize,
    retention: CaptureRetention,
) -> io::Result<()> {
    let mut buffer = [0_u8; SSH_PIPE_READ_BUFFER_BYTES];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        {
            let mut captured = captured.lock().unwrap_or_else(PoisonError::into_inner);
            match retention {
                CaptureRetention::Head => {
                    let remaining = limit.saturating_sub(captured.len());
                    captured.extend(buffer[..read.min(remaining)].iter().copied());
                }
                CaptureRetention::Tail => {
                    let bytes = &buffer[..read];
                    if bytes.len() >= limit {
                        captured.clear();
                        captured.extend(bytes[bytes.len() - limit..].iter().copied());
                    } else {
                        let overflow = captured
                            .len()
                            .saturating_add(bytes.len())
                            .saturating_sub(limit);
                        captured.drain(..overflow);
                        captured.extend(bytes.iter().copied());
                    }
                }
            }
        }
    }
}

pub(crate) fn wait_with_output_timeout(
    child: std::process::Child,
    timeout: Duration,
) -> io::Result<Output> {
    let mut running = RunningCommand::new(child);
    let stdout = running
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH command stdout was not captured"))?;
    let stderr = running
        .child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH command stderr was not captured"))?;
    // Discovery and status responses are small. Drain both pipes to avoid a
    // child blocking, but retain only bounded output from the remote host.
    running.stdout = Some(
        PipeCapture::spawn_tail(stdout, SSH_STDOUT_CAPTURE_LIMIT)
            .with_context(running.child.id(), "command stdout"),
    );
    running.stderr = Some(
        PipeCapture::spawn(stderr, SSH_STDERR_CAPTURE_LIMIT)
            .with_context(running.child.id(), "command stderr"),
    );
    // clock-io-ok: this deadline measures the running child's wall time.
    let started = Instant::now();
    let status = loop {
        match running.child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(error),
        }
        if started.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH command timed out",
            ));
        }
        thread::sleep(SSH_CHILD_PROCESS_POLL_INTERVAL);
    };
    running.armed = false;
    let stdout = running
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH stdout capture missing"))?;
    let stderr = running
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH stderr capture missing"))?;
    let stdout = stdout.finish(PIPE_DRAIN_GRACE)?;
    let stderr = stderr.finish(PIPE_DRAIN_GRACE)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Reaps the command and drains capture threads on every incomplete return.
struct RunningCommand {
    child: std::process::Child,
    stdout: Option<PipeCapture>,
    stderr: Option<PipeCapture>,
    armed: bool,
}

impl RunningCommand {
    fn new(child: std::process::Child) -> Self {
        Self {
            child,
            stdout: None,
            stderr: None,
            armed: true,
        }
    }
}

impl Drop for RunningCommand {
    fn drop(&mut self) {
        if self.armed {
            kill_and_reap(&mut self.child, "SSH command");
            if let Some(stdout) = self.stdout.take() {
                drop(stdout.finish(PIPE_DRAIN_GRACE));
            }
            if let Some(stderr) = self.stderr.take() {
                drop(stderr.finish(PIPE_DRAIN_GRACE));
            }
        }
    }
}

/// Sends SIGKILL to an ssh child being abandoned. `kill` succeeds on a child
/// that exited but is not yet reaped, so an error means the signal did not
/// reach it and the process may outlive shepr's interest in it.
pub(crate) fn kill_child(child: &mut std::process::Child, what: &str) {
    if let Err(error) = child.kill() {
        shepr_platform::structured_log!(WARN, event = remote.child_kill, outcome = Error, pid = child.id(), %error, "could not kill {what}");
    }
}

/// Kills and reaps an ssh child being abandoned, logging (with its pid) a child
/// that could not be killed or reaped rather than leaving it unexplained.
pub(crate) fn kill_and_reap(child: &mut std::process::Child, what: &str) {
    kill_child(child, what);
    if let Err(error) = child.wait() {
        shepr_platform::structured_log!(WARN, event = remote.child_reap, outcome = Error, pid = child.id(), %error, "could not reap {what}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{self, Held, Step};
    use std::process::Stdio;

    fn fixture_process_group_is_gone(group: libc::pid_t) -> bool {
        // SAFETY: signal zero checks whether this test-owned group exists; it
        // does not signal or alter any process.
        let result = unsafe { libc::kill(-group, 0) };
        result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    struct FixtureProcessGroupGuard(libc::pid_t);

    impl Drop for FixtureProcessGroupGuard {
        fn drop(&mut self) {
            // SAFETY: the test fixture is started in a new process group below,
            // so its pid is the group id for it and its spawned fixture.
            let result = unsafe { libc::kill(-self.0, libc::SIGKILL) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return;
                }
                if !std::thread::panicking() {
                    panic!("could not kill fixture process group {}: {error}", self.0);
                }
                return;
            }

            let deadline = Instant::now() + Duration::from_secs(5);
            while !fixture_process_group_is_gone(self.0) {
                if Instant::now() >= deadline {
                    if !std::thread::panicking() {
                        panic!("fixture process group {} survived cleanup", self.0);
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn timeout_kills_the_child() {
        let mut command = fixture::command(&[Step::Sleep(Duration::from_secs(10))]);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let started = Instant::now();
        let error = wait_with_output_timeout(
            command.spawn().expect("test precondition"),
            Duration::from_millis(25),
        )
        .expect_err("test precondition");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Models a configured `LocalCommand` that leaves a background descendant
    /// holding stderr open while the ssh child itself exits.
    #[test]
    fn a_stderr_pipe_held_by_a_background_process_does_not_block_the_result() {
        let mut command = fixture::command(&[
            Step::Spawn {
                argv0: "local-command-descendant".into(),
                sleep: Duration::from_secs(5),
                held: Held::Stderr,
            },
            Step::Print("out".into()),
            Step::PrintErr("err".into()),
        ]);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        // This stand-in intentionally leaves a background fixture holding
        // stderr. Put it and that child in a test-owned group so drop can clean
        // up and wait for the simulated LocalCommand process tree, even on panic.
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command.spawn().expect("test precondition");
        let process_group = libc::pid_t::try_from(child.id()).expect("fixture pid fits");
        let fixture_group = FixtureProcessGroupGuard(process_group);
        let started = Instant::now();
        let output = wait_with_output_timeout(child, Duration::from_secs(10))
            .expect("the child itself exits immediately");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "waited on the background holder: {:?}",
            started.elapsed()
        );
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
        drop(fixture_group);
        assert!(
            fixture_process_group_is_gone(process_group),
            "the fixture's background child outlived the test"
        );
    }

    #[test]
    fn capture_is_bounded() {
        let input = vec![b'x'; 64 * 1024];
        let capture = PipeCapture::spawn(io::Cursor::new(input), 16 * 1024);
        let captured = capture
            .finish(Duration::from_secs(3))
            .expect("test precondition");
        assert_eq!(captured.len(), 16 * 1024);
    }

    #[test]
    fn tail_capture_keeps_the_newest_bytes_within_its_limit() {
        let capture = PipeCapture::spawn_tail(io::Cursor::new(b"hello world".to_vec()), 5);
        let captured = capture
            .finish(Duration::from_secs(3))
            .expect("test precondition");
        assert_eq!(captured, b"world");
    }
}
