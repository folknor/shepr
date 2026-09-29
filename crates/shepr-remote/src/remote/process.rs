use std::collections::VecDeque;
use std::io::{self, Read, Write as _};
use std::process::Output;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
pub(super) const SSH_STDOUT_CAPTURE_LIMIT: usize = 1024 * 1024;
pub(super) const SSH_STDERR_CAPTURE_LIMIT: usize = 16 * 1024;

/// How long a pipe reader may keep running after the ssh child has exited.
///
/// The child's own output is already in the pipe by then, so a healthy reader
/// drains it in far less than this. A reader that is still blocked means some
/// other process inherited the write end: an OpenSSH `ControlPersist` master
/// forked from this very command is the known case (older OpenSSH releases
/// daemonize the master without redirecting its stderr, so the pipe stays
/// open for up to the persist timeout). Joining such a reader would stall the
/// caller for that long, so it is abandoned instead and the bytes captured so
/// far are used.
pub(super) const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
enum CaptureRetention {
    Head,
    Tail,
}

/// Reads a child pipe on its own thread. [`PipeCapture::finish`] never waits on
/// the reader longer than the grace it is given, so a pipe held open by a
/// process other than the child cannot hang the caller.
pub(super) struct PipeCapture {
    captured: Arc<Mutex<VecDeque<u8>>>,
    done: mpsc::Receiver<io::Result<()>>,
}

/// Where a capture copies what it reads, besides its own buffer.
#[derive(Clone, Copy)]
pub(super) enum PipeEcho {
    None,
    /// Relay to this process's stderr as it arrives (interactive setup).
    Stderr,
}

impl PipeCapture {
    pub(super) fn spawn(reader: impl Read + Send + 'static, limit: usize, echo: PipeEcho) -> Self {
        Self::spawn_with_retention(reader, limit, echo, CaptureRetention::Head)
    }

    /// Retains trailing command output so a large login banner cannot hide its result.
    pub(super) fn spawn_tail(
        reader: impl Read + Send + 'static,
        limit: usize,
        echo: PipeEcho,
    ) -> Self {
        Self::spawn_with_retention(reader, limit, echo, CaptureRetention::Tail)
    }

    fn spawn_with_retention(
        reader: impl Read + Send + 'static,
        limit: usize,
        echo: PipeEcho,
        retention: CaptureRetention,
    ) -> Self {
        // Grows with the output: the stdout limit is 1 MiB and most commands
        // print a few lines, so reserving the limit up front would waste it.
        let captured = Arc::new(Mutex::new(VecDeque::new()));
        let worker_captured = Arc::clone(&captured);
        let (done_tx, done) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = read_into(reader, &worker_captured, limit, echo, retention);
            // The receiver is gone only when `finish` gave up on this reader after
            // its grace, or the capture was dropped unfinished; either way nobody
            // is left to want the result.
            drop(done_tx.send(result));
        });
        Self { captured, done }
    }

    /// Waits up to `grace` for the pipe to reach end of stream and returns what
    /// was read. When the reader is still blocked after `grace`, the thread is
    /// left to finish on its own and the partial capture is returned.
    pub(super) fn finish(self, grace: Duration) -> io::Result<Vec<u8>> {
        match self.done.recv_timeout(grace) {
            Ok(result) => result?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::debug!(
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
    echo: PipeEcho,
    retention: CaptureRetention,
) -> io::Result<()> {
    let mut buffer = [0_u8; 8 * 1024];
    let mut destination = io::stderr();
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
        // The echo is a live relay for the person at the terminal; the capture
        // above is what callers use. A failed write has nowhere better to be
        // reported than the stderr that just refused it, and std's stderr is
        // unbuffered, so there is nothing to flush.
        if matches!(echo, PipeEcho::Stderr) {
            drop(destination.write_all(&buffer[..read]));
        }
    }
}

pub(super) fn wait_with_output_timeout(
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
    running.stdout = Some(PipeCapture::spawn_tail(
        stdout,
        SSH_STDOUT_CAPTURE_LIMIT,
        PipeEcho::None,
    ));
    running.stderr = Some(PipeCapture::spawn(
        stderr,
        SSH_STDERR_CAPTURE_LIMIT,
        PipeEcho::None,
    ));
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
                "noninteractive SSH command timed out",
            ));
        }
        thread::sleep(POLL_INTERVAL);
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
pub(super) fn kill_child(child: &mut std::process::Child, what: &str) {
    if let Err(error) = child.kill() {
        tracing::warn!(pid = child.id(), %error, "could not kill {what}");
    }
}

/// Kills and reaps an ssh child being abandoned, logging (with its pid) a child
/// that could not be killed or reaped rather than leaving it unexplained.
pub(super) fn kill_and_reap(child: &mut std::process::Child, what: &str) {
    kill_child(child, what);
    if let Err(error) = child.wait() {
        tracing::warn!(pid = child.id(), %error, "could not reap {what}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{self, Held, Step};
    use std::process::Stdio;

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

    /// Models a `ControlPersist` master that daemonizes with the command's stderr
    /// still open: the child exits at once, a background process keeps the pipe.
    #[test]
    fn a_stderr_pipe_held_by_a_background_process_does_not_block_the_result() {
        let mut command = fixture::command(&[
            Step::Spawn {
                argv0: "control-master".into(),
                sleep: Duration::from_secs(5),
                held: Held::Stderr,
            },
            Step::Print("out".into()),
            Step::PrintErr("err".into()),
        ]);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let started = Instant::now();
        let output = wait_with_output_timeout(
            command.spawn().expect("test precondition"),
            Duration::from_secs(10),
        )
        .expect("the child itself exits immediately");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "waited on the background holder: {:?}",
            started.elapsed()
        );
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[test]
    fn capture_is_bounded() {
        let input = vec![b'x'; 64 * 1024];
        let capture = PipeCapture::spawn(io::Cursor::new(input), 16 * 1024, PipeEcho::None);
        let captured = capture
            .finish(Duration::from_secs(3))
            .expect("test precondition");
        assert_eq!(captured.len(), 16 * 1024);
    }

    #[test]
    fn tail_capture_keeps_the_newest_bytes_within_its_limit() {
        let capture =
            PipeCapture::spawn_tail(io::Cursor::new(b"hello world".to_vec()), 5, PipeEcho::None);
        let captured = capture
            .finish(Duration::from_secs(3))
            .expect("test precondition");
        assert_eq!(captured, b"world");
    }
}
