use std::io::{self, Write as _};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::failure::{
    REMAPPED_REMOTE_255_EXIT_CODE, RemoteExit, SSH_OWN_FAILURE_EXIT_CODE, SshExit,
    SshFailureDiagnostic, local_setup_error,
};
use crate::host::{BRIDGE_FAILURE_MARKER, BridgeMode};
use crate::limits::{
    BRIDGE_CHILD_POLL, BRIDGE_CONNECTION_SHUTDOWN_GRACE, BRIDGE_IO_BUFFER_BYTES, BRIDGE_IO_POLL,
    BRIDGE_WRITE_CHUNK_BYTES, PIPE_DRAIN_GRACE, SSH_STDERR_CAPTURE_LIMIT,
};
use crate::machine::{RemoteExecutable, SshTarget};
use crate::process::{PipeCapture, kill_and_reap, kill_child};
use crate::shell_command::{AccountShellCommand, REMOTE_OUTPUT_READY_MARKER};
use crate::ssh::{
    ManagedSshOptions, apply_batch_ssh_options, apply_managed_ssh_options, ssh_command,
};

pub(crate) struct SshStdioBridge {
    should_stop: Arc<AtomicBool>,
    worker: std::sync::Mutex<Option<JoinHandle<io::Result<()>>>>,
}

impl SshStdioBridge {
    pub(crate) fn start(
        target: SshTarget,
        remote_shepr: &RemoteExecutable,
        mode: BridgeMode,
        ssh_options: Option<&ManagedSshOptions>,
    ) -> io::Result<(Self, shepr_platform::ipc::LocalStream)> {
        Self::start_command(target, remote_shepr.bridge_command(mode), ssh_options)
    }

    pub(crate) fn start_command(
        target: SshTarget,
        remote_command: AccountShellCommand,
        ssh_options: Option<&ManagedSshOptions>,
    ) -> io::Result<(Self, shepr_platform::ipc::LocalStream)> {
        let (client, stream) = shepr_platform::ipc::LocalStream::pair()?;
        let should_stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&should_stop);
        let ssh_options = ssh_options.cloned();
        let worker = thread::spawn(move || {
            bridge_connection(
                stream,
                &target,
                &remote_command,
                ssh_options.as_ref(),
                &thread_stop,
            )
            .inspect_err(|error| {
                tracing::warn!(%error, target = %target.as_str(), "remote SSH bridge failed");
            })
        });
        Ok((
            Self {
                should_stop,
                worker: std::sync::Mutex::new(Some(worker)),
            },
            client,
        ))
    }

    /// The connection's failure, joining its worker; `None` once taken. Call
    /// it only after the stream reached EOF: the worker has then ended the
    /// connection and only reaps ssh and drains its pipes, each bounded. Called
    /// earlier, it waits for the connection itself to end.
    pub(crate) fn reported_failure(&self) -> Option<io::Error> {
        self.finish().err()
    }

    fn finish(&self) -> io::Result<()> {
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match worker {
            Some(worker) => worker
                .join()
                .map_err(|_| io::Error::other("remote bridge worker panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for SshStdioBridge {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Release);
        if let Err(error) = self.finish() {
            tracing::debug!(%error, "remote bridge ended during teardown");
        }
    }
}

struct BridgeUploadStop {
    stopped: AtomicBool,
    wake: shepr_platform::StreamWake,
}

impl BridgeUploadStop {
    fn new() -> io::Result<Self> {
        Ok(Self {
            stopped: AtomicBool::new(false),
            wake: shepr_platform::StreamWake::new()?,
        })
    }

    fn cancel(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel)
            && let Err(error) = self.wake.cancel()
        {
            tracing::debug!(%error, "remote bridge read cancellation failed");
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

/// The upload half of a bridge connection: copies what the local client
/// writes to its socket into `writer` (the ssh child's stdin) on a thread of
/// its own, until the upload is cancelled, the bridge stops, or the client
/// closes its end. Cancelling never closes the socket, so the download half
/// keeps delivering what the remote end still sends.
struct BridgeUpload {
    stop: Arc<BridgeUploadStop>,
    failed: Arc<AtomicBool>,
    client_closed: Arc<AtomicBool>,
    worker: JoinHandle<io::Result<u64>>,
}

/// How an upload ended: the copy's result and whether the local client closed
/// its end.
struct BridgeUploadEnd {
    result: io::Result<u64>,
    client_closed: bool,
}

struct BridgeDownload {
    result: mpsc::Receiver<io::Result<u64>>,
    worker: JoinHandle<()>,
}

enum BridgeDownloadEnd {
    Complete(io::Result<u64>),
    DrainTimedOut,
}

impl BridgeDownload {
    fn spawn(copy: impl FnOnce() -> io::Result<u64> + Send + 'static) -> Self {
        let (result_tx, result) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = copy();
            drop(result_tx.send(result));
        });
        Self { result, worker }
    }

    /// Waits for the SSH stdout drain only for its grace period. A process
    /// forked by the SSH connection can inherit stdout and keep this worker
    /// blocked after the SSH child exits, so an unfinished worker is detached.
    fn finish(
        self,
        grace: std::time::Duration,
        connection_stop: &AtomicBool,
        stream: &shepr_platform::ipc::LocalStream,
    ) -> io::Result<BridgeDownloadEnd> {
        match self.result.recv_timeout(grace) {
            Ok(result) => {
                // The copy finished before sending its result; dropping the
                // handle avoids waiting for the thread's final return path.
                drop(self.worker);
                Ok(BridgeDownloadEnd::Complete(result))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                connection_stop.store(true, Ordering::Release);
                if let Err(error) = stream.shutdown(std::net::Shutdown::Both)
                    && !matches!(
                        shepr_platform::ipc::classify_stream_error(error.kind()),
                        shepr_platform::ipc::StreamFailure::PeerGone
                    )
                {
                    tracing::debug!(%error, "remote bridge download stream shutdown failed");
                }
                tracing::debug!(
                    ?grace,
                    "remote bridge stdout stayed open past its drain grace; detaching download worker"
                );
                drop(self.worker);
                Ok(BridgeDownloadEnd::DrainTimedOut)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.worker
                    .join()
                    .map_err(|_| io::Error::other("remote bridge download worker panicked"))?;
                Err(io::Error::other(
                    "remote bridge download worker ended without reporting",
                ))
            }
        }
    }
}

impl BridgeUpload {
    fn spawn(
        stream: shepr_platform::ipc::LocalStream,
        mut writer: impl io::Write + Send + 'static,
        bridge_stop: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let stop = Arc::new(BridgeUploadStop::new()?);
        let failed = Arc::new(AtomicBool::new(false));
        let client_closed = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            let failed = Arc::clone(&failed);
            let client_closed = Arc::clone(&client_closed);
            thread::spawn(move || {
                let result = copy_local_stream_to_writer(
                    stream,
                    &mut writer,
                    &stop,
                    &bridge_stop,
                    &client_closed,
                );
                failed.store(result.is_err(), Ordering::Release);
                result
            })
        };
        Ok(Self {
            stop,
            failed,
            client_closed,
            worker,
        })
    }

    fn stop_handle(&self) -> Arc<BridgeUploadStop> {
        Arc::clone(&self.stop)
    }

    /// The copy failed; set once it has ended.
    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    fn client_closed(&self) -> bool {
        self.client_closed.load(Ordering::Acquire)
    }

    /// Wait for the copy to end.
    fn join(self) -> io::Result<BridgeUploadEnd> {
        let result = self
            .worker
            .join()
            .map_err(|_| io::Error::other("remote bridge upload worker panicked"))?;
        Ok(BridgeUploadEnd {
            result,
            client_closed: self.client_closed.load(Ordering::Acquire),
        })
    }
}

fn bridge_connection(
    stream: shepr_platform::ipc::LocalStream,
    target: &SshTarget,
    remote_command: &AccountShellCommand,
    ssh_options: Option<&ManagedSshOptions>,
    bridge_stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut command = ssh_command();
    apply_managed_ssh_options(&mut command, ssh_options);
    apply_batch_ssh_options(&mut command);
    command.arg("-T");
    target.append_to(&mut command);
    command
        .arg(remote_command.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command
        .spawn()
        .map_err(|error| local_setup_error("could not start local ssh bridge", error))?;
    let mut child = BridgeChildStartupGuard::new(child);
    let child_stdin = child
        .child()?
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh bridge stdin missing"))?;
    let child_stdout =
        child.child()?.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ssh bridge stdout missing")
        })?;
    let child_stderr =
        child.child()?.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ssh bridge stderr missing")
        })?;
    let stderr_reader = PipeCapture::spawn(child_stderr, SSH_STDERR_CAPTURE_LIMIT);
    let stream_to_child = stream.try_clone()?;
    stream.set_nonblocking(true)?;
    let mut child_to_stream = stream;
    let download_shutdown = child_to_stream.try_clone()?;

    let connection_stop = Arc::new(AtomicBool::new(false));
    let download_done = Arc::new(AtomicBool::new(false));
    let upload = BridgeUpload::spawn(stream_to_child, child_stdin, Arc::clone(bridge_stop))?;
    let mut child = child.into_child()?;
    let upload_stop = upload.stop_handle();
    let download_stop = Arc::clone(&connection_stop);
    let download_bridge_stop = Arc::clone(bridge_stop);
    let download_done_worker = Arc::clone(&download_done);
    let download_upload_stop = Arc::clone(&upload_stop);
    let download = BridgeDownload::spawn(move || {
        let mut child_stdout = io::BufReader::new(child_stdout);
        let result = discard_remote_output_preamble(&mut child_stdout).and_then(|()| {
            copy_reader_to_local_stream(
                &mut child_stdout,
                &mut child_to_stream,
                &download_stop,
                &download_bridge_stop,
            )
        });
        download_done_worker.store(true, Ordering::Release);
        download_upload_stop.cancel();
        result
    });

    let mut stopped_at = None;
    let (status_result, child_exited) = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                upload_stop.cancel();
                break (Ok(status), true);
            }
            Ok(None) => {}
            Err(err) => {
                connection_stop.store(true, Ordering::Release);
                upload_stop.cancel();
                kill_and_reap(&mut child, "ssh bridge");
                break (Err(err), false);
            }
        }
        if bridge_stop.load(Ordering::Acquire) {
            connection_stop.store(true, Ordering::Release);
            upload_stop.cancel();
            kill_child(&mut child, "ssh bridge");
            break (child.wait(), false);
        }
        if upload.client_closed() || upload.failed() || download_done.load(Ordering::Acquire) {
            upload_stop.cancel();
            // clock-io-ok: grace begins when client or pipe IO first stops.
            let stopped_at = stopped_at.get_or_insert_with(Instant::now);
            if stopped_at.elapsed() >= BRIDGE_CONNECTION_SHUTDOWN_GRACE {
                connection_stop.store(true, Ordering::Release);
                kill_child(&mut child, "ssh bridge");
                break (child.wait(), false);
            }
        }
        thread::sleep(BRIDGE_CHILD_POLL);
    };
    upload_stop.cancel();
    if !child_exited {
        connection_stop.store(true, Ordering::Release);
    }
    let upload_end = upload.join();
    let download_result = download.finish(PIPE_DRAIN_GRACE, &connection_stop, &download_shutdown);
    let BridgeUploadEnd {
        result: upload_result,
        client_closed,
    } = upload_end?;
    let download_result = download_result?;
    // Bounded: a ControlPersist master forked by this ssh can hold its stderr open for
    // the whole persist timeout after the bridge itself has exited.
    let stderr = stderr_reader.finish(PIPE_DRAIN_GRACE)?;
    let status = status_result?;

    let stopping = bridge_stop.load(Ordering::Acquire);
    if child_exited && !status.success() && !stopping && !client_closed {
        return Err(ssh_bridge_exit_error(status, &stderr));
    }
    if !stopping && !client_closed {
        upload_result.map_err(|err| {
            let diagnostic =
                SshFailureDiagnostic::from_error(&err).with_context("remote bridge upload failed");
            io::Error::new(err.kind(), diagnostic)
        })?;
        if let BridgeDownloadEnd::Complete(download_result) = download_result {
            download_result.map_err(|err| {
                let diagnostic = SshFailureDiagnostic::from_error(&err)
                    .with_context("remote bridge download failed");
                io::Error::new(err.kind(), diagnostic)
            })?;
        }
    }

    if status.success() || stopping || client_closed {
        Ok(())
    } else {
        Err(ssh_bridge_exit_error(status, &stderr))
    }
}

/// Classify an SSH bridge exit, or the exit of another long-running remote
/// shepr command such as the wait for a server. The raw remote stderr is
/// handed to the diagnostic constructor, which stores it as terminal-safe
/// `RemoteText`.
pub(crate) fn ssh_bridge_exit_error(status: std::process::ExitStatus, stderr: &[u8]) -> io::Error {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if let Some(error) = classified_remote_bridge_failure(stderr) {
        return error;
    }
    let (failure, exit_status) = match SshExit::from_code(status.code()) {
        SshExit::SshFailed => (
            "remote SSH connection failed",
            format!("exit status {SSH_OWN_FAILURE_EXIT_CODE}"),
        ),
        SshExit::Remote(RemoteExit::Remapped255Or254) => {
            // SSH exposes one exit byte, so remapping remote 255 to 254 aliases
            // a native remote 254. Name the mapping without guessing which ran.
            (
                "remote command failed",
                format!(
                    "reported exit status {REMAPPED_REMOTE_255_EXIT_CODE} (remote status {SSH_OWN_FAILURE_EXIT_CODE} is remapped to {REMAPPED_REMOTE_255_EXIT_CODE}; a native {REMAPPED_REMOTE_255_EXIT_CODE} is indistinguishable)"
                ),
            )
        }
        exit => {
            let exit_status = match exit {
                SshExit::Remote(exit) => format!("exit status {}", exit.code()),
                _ => status.to_string(),
            };
            ("remote command failed", exit_status)
        }
    };
    let message = if stderr.is_empty() {
        format!("{failure} ({exit_status})")
    } else {
        format!("{failure} ({exit_status}): {stderr}")
    };
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        SshFailureDiagnostic::from_ssh_output(status.code(), &message),
    )
}

/// Converts the classification record a remote bridge leads its failure with
/// (see [`crate::host::classified_bridge_failure`]) to a typed endpoint
/// failure. Ordinary remote stderr remains diagnostic text and is never used
/// to infer an operator action: a remote of a build that writes no record, or
/// a class this build does not know, stays an unclassified retry.
fn classified_remote_bridge_failure(stderr: &str) -> Option<io::Error> {
    for (index, line) in stderr.lines().enumerate() {
        let line = line.trim();
        // The remote CLI may add a presentation prefix; the marker itself is
        // the failure record, so its position on the line is not significant.
        let Some(marker) = line.find(BRIDGE_FAILURE_MARKER) else {
            continue;
        };
        let token = &line[marker + BRIDGE_FAILURE_MARKER.len()..];
        let Some(class) = shepr_launch::RemoteFailureClass::from_token(token) else {
            continue;
        };
        let detail = stderr
            .lines()
            .skip(index + 1)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        let message = if detail.is_empty() {
            "the remote shepr bridge failed".to_owned()
        } else {
            format!("the remote shepr bridge failed:\n{detail}")
        };
        return Some(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            class.endpoint_failure(message),
        ));
    }
    None
}

pub(crate) fn discard_remote_output_preamble(reader: &mut impl io::BufRead) -> io::Result<()> {
    let marker = REMOTE_OUTPUT_READY_MARKER.as_bytes();
    let mut matched = 0;
    let mut matching = true;
    loop {
        let (consumed, ready) = {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                if matching && matched == marker.len() {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "remote command exited before producing its output marker",
                ));
            }

            let mut consumed = 0;
            let mut ready = false;
            for &byte in buffer {
                consumed += 1;
                if byte == b'\n' {
                    if matching && matched == marker.len() {
                        ready = true;
                        break;
                    }
                    matched = 0;
                    matching = true;
                } else if matching && matched < marker.len() && byte == marker[matched] {
                    matched += 1;
                } else if matching && (matched != marker.len() || byte != b'\r') {
                    matching = false;
                }
            }
            (consumed, ready)
        };
        reader.consume(consumed);
        if ready {
            return Ok(());
        }
    }
}

struct BridgeChildStartupGuard(Option<std::process::Child>);

impl BridgeChildStartupGuard {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    fn child(&mut self) -> io::Result<&mut std::process::Child> {
        self.0
            .as_mut()
            .ok_or_else(|| io::Error::other("ssh bridge child ownership was already transferred"))
    }

    fn into_child(mut self) -> io::Result<std::process::Child> {
        self.0
            .take()
            .ok_or_else(|| io::Error::other("ssh bridge child ownership was already transferred"))
    }
}

impl Drop for BridgeChildStartupGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            kill_and_reap(child, "ssh bridge");
        }
    }
}

fn copy_reader_to_local_stream<R: io::Read>(
    reader: &mut R,
    stream: &mut shepr_platform::ipc::LocalStream,
    connection_stop: &AtomicBool,
    bridge_stop: &AtomicBool,
) -> io::Result<u64> {
    let mut buffer = [0_u8; BRIDGE_IO_BUFFER_BYTES];
    let mut total = 0;

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        let mut written = 0;
        while written < read {
            if connection_stop.load(Ordering::Acquire) || bridge_stop.load(Ordering::Acquire) {
                return Ok(total);
            }
            let chunk_len = (read - written).min(BRIDGE_WRITE_CHUNK_BYTES);
            match stream.write(&buffer[written..written + chunk_len]) {
                Ok(0) => thread::sleep(BRIDGE_IO_POLL),
                Ok(count) => written += count,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    // StreamWake only waits for reads; writable readiness
                    // needs a shepr-platform primitive to keep fd polling out
                    // of this SSH policy layer.
                    thread::sleep(BRIDGE_IO_POLL);
                }
                Err(err) => return Err(err),
            }
        }
        stream.flush()?;
        total += read as u64;
    }
}

/// The upload half of the SSH bridge: everything the client types or pastes passes through
/// here on its way to the remote host. Like the download half above, it never logs the
/// bytes it copies; bridge diagnostics carry errors and ssh's own stderr only.
fn copy_local_stream_to_writer<W: io::Write>(
    stream: shepr_platform::ipc::LocalStream,
    writer: &mut W,
    connection_stop: &BridgeUploadStop,
    bridge_stop: &AtomicBool,
    client_closed: &AtomicBool,
) -> io::Result<u64> {
    copy_upload_stream_to_writer(stream, writer, connection_stop, bridge_stop, client_closed)
}

trait UploadReadStream {
    fn poll_read_count(
        &mut self,
        buffer: &mut [u8],
    ) -> io::Result<shepr_platform::ipc::LocalStreamReadCount>;

    fn wait_for_input(&self, wake: &shepr_platform::StreamWake) -> io::Result<()>;
}

impl UploadReadStream for shepr_platform::ipc::LocalStream {
    fn poll_read_count(
        &mut self,
        buffer: &mut [u8],
    ) -> io::Result<shepr_platform::ipc::LocalStreamReadCount> {
        shepr_platform::ipc::poll_local_stream_read_count(self, buffer)
    }

    fn wait_for_input(&self, wake: &shepr_platform::StreamWake) -> io::Result<()> {
        wake.wait(self)
    }
}

fn copy_upload_stream_to_writer<S: UploadReadStream, W: io::Write>(
    mut stream: S,
    writer: &mut W,
    connection_stop: &BridgeUploadStop,
    bridge_stop: &AtomicBool,
    client_closed: &AtomicBool,
) -> io::Result<u64> {
    let mut buffer = [0_u8; BRIDGE_IO_BUFFER_BYTES];
    let mut total = 0;

    while !connection_stop.is_stopped() && !bridge_stop.load(Ordering::Acquire) {
        match stream.poll_read_count(&mut buffer)? {
            shepr_platform::ipc::LocalStreamReadCount::Data(read) => {
                writer.write_all(&buffer[..read])?;
                writer.flush()?;
                total += read as u64;
            }
            shepr_platform::ipc::LocalStreamReadCount::Pending => {
                stream.wait_for_input(&connection_stop.wake)?;
            }
            shepr_platform::ipc::LocalStreamReadCount::Closed => {
                client_closed.store(true, Ordering::Release);
                break;
            }
        }
    }

    Ok(total)
}

#[cfg(test)]
mod tests;
