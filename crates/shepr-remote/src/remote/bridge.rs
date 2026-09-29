use super::*;

use super::process::{PipeCapture, PipeEcho, kill_and_reap, kill_child};
use interprocess::TryClone as _;
use interprocess::local_socket::ListenerNonblockingMode;
use interprocess::local_socket::traits::Listener as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::limits::{
    BRIDGE_ACCEPT_POLL, BRIDGE_CONNECTION_SHUTDOWN_GRACE, BRIDGE_FAILURE_CHANNEL_CAPACITY,
    BRIDGE_FAILURE_REPORT_POLL_INTERVAL, BRIDGE_FAILURE_REPORT_TIMEOUT, BRIDGE_IO_BUFFER_BYTES,
    BRIDGE_IO_POLL, BRIDGE_PATH_COMPONENT_MAX_CHARS, BRIDGE_TARGET_PREFIX_CHARS,
    BRIDGE_WRITE_CHUNK_BYTES, PIPE_DRAIN_GRACE, SSH_STDERR_CAPTURE_LIMIT,
};

/// Another live bridge already holds a local bridge socket path. Carried
/// inside an [`io::ErrorKind::AddrInUse`] error, so SSH failure classification
/// keeps treating it as a link failure while the message names the path.
#[derive(Debug)]
pub(crate) struct BridgeSocketBusy {
    path: PathBuf,
}

impl BridgeSocketBusy {
    fn error(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::AddrInUse,
            Self {
                path: path.to_path_buf(),
            },
        )
    }
}

impl std::fmt::Display for BridgeSocketBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "local bridge socket {} is held by another bridge",
            self.path.display()
        )
    }
}

impl std::error::Error for BridgeSocketBusy {}

pub(crate) struct SshStdioBridge {
    local_socket: PathBuf,
    socket_identity: shepr_platform::ipc::SocketFileIdentity,
    _socket_startup_lock: shepr_platform::ipc::SocketStartupLock,
    should_stop: Arc<AtomicBool>,
    // The accept thread clears a previous report before each accepted stream.
    // A generation slot would also need the caller to pass the stream's
    // generation into reported_failure; the current SavedSshStream and API
    // bridge handles carry no such identity.
    failure_rx: Arc<std::sync::Mutex<mpsc::Receiver<io::Error>>>,
    thread: Option<JoinHandle<()>>,
    // Dropped after `Drop::drop` has removed the socket; see `TeardownRegistry`.
    _teardown: TeardownRegistration,
}

impl SshStdioBridge {
    pub(crate) fn start(
        target: SshTarget,
        remote_shepr: &RemoteExecutable,
        local_socket: PathBuf,
        session_name: &str,
        ssh_options: Option<&ManagedSshOptions>,
        noninteractive: bool,
    ) -> io::Result<Self> {
        let target_id = target.as_str().to_owned();
        let executable_path = remote_shepr.as_str().to_owned();
        let session = session_name.to_owned();
        let bridge = Self::start_command(
            target,
            remote_shepr.bridge_command(session_name),
            local_socket,
            ssh_options,
            noninteractive,
        )?;
        tracing::info!(
            target = %target_id,
            session = %session,
            executable = %executable_path,
            socket = %bridge.local_socket.display(),
            "remote SSH stdio bridge listening"
        );
        Ok(bridge)
    }

    pub(crate) fn start_command(
        target: SshTarget,
        remote_command: String,
        local_socket: PathBuf,
        ssh_options: Option<&ManagedSshOptions>,
        noninteractive: bool,
    ) -> io::Result<Self> {
        let (listener, socket_startup_lock, socket_identity) =
            shepr_platform::ipc::bind_private_socket(&local_socket).map_err(|error| {
                if error.kind() == io::ErrorKind::AddrInUse {
                    BridgeSocketBusy::error(&local_socket)
                } else {
                    error
                }
            })?;
        let teardown = SSH_TEARDOWN.register(TeardownResource::Socket {
            path: local_socket.clone(),
            identity: socket_identity.clone(),
        });
        let mut socket_cleanup = BridgeSocketStartupCleanup::new(&local_socket, &socket_identity);
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;

        let should_stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&should_stop);
        let thread_ssh_options = ssh_options.cloned();
        // A failure belongs to the last accepted stream. Before accepting a later
        // stream, discard any unclaimed report so a slow earlier SSH exit cannot
        // be shown as the later request's failure. Keep send nonblocking so the SSH
        // worker can finish even if its caller is already unwinding.
        let (failure_tx, failure_rx) = mpsc::sync_channel(BRIDGE_FAILURE_CHANNEL_CAPACITY);
        let failure_rx = Arc::new(std::sync::Mutex::new(failure_rx));
        let thread_failure_rx = Arc::clone(&failure_rx);
        let thread_socket = local_socket.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok(stream) => {
                        match shepr_platform::ipc::peer_is_same_user(&stream) {
                            Ok(true) => {}
                            Ok(false) => {
                                tracing::warn!(
                                    target = %target.as_str(),
                                    socket = %thread_socket.display(),
                                    "rejected remote bridge socket peer with different credentials"
                                );
                                continue;
                            }
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    target = %target.as_str(),
                                    socket = %thread_socket.display(),
                                    "could not check remote bridge socket peer"
                                );
                                continue;
                            }
                        }
                        discard_unclaimed_bridge_failure(&thread_failure_rx);
                        let stream = match prepare_remote_bridge_stream(stream) {
                            Ok(stream) => stream,
                            Err(err) => {
                                // This local setup failure drops the accepted request; the
                                // bridge remains available for later connections.
                                tracing::error!(
                                    error = %err,
                                    target = %target.as_str(),
                                    socket = %thread_socket.display(),
                                    "remote bridge failed to prepare client socket"
                                );
                                continue;
                            }
                        };
                        // Each local API request has its own stream and SSH stdio
                        // process. Keep `bridge_connection` inline in this accept loop:
                        // a second stream waits until this one returns because one SSH
                        // process can carry only one local stream.
                        if let Err(err) = bridge_connection(
                            stream,
                            &target,
                            &remote_command,
                            thread_ssh_options.as_ref(),
                            noninteractive,
                            &thread_stop,
                        ) {
                            // Use tracing in both modes. The owner reads the error back
                            // through `reported_failure` and presents it; noninteractive
                            // is context on the event, not a choice of output channel.
                            tracing::warn!(
                                error = %err,
                                noninteractive,
                                target = %target.as_str(),
                                socket = %thread_socket.display(),
                                "remote SSH bridge failed"
                            );
                            // The original error, so its typed SSH failure survives.
                            // Already logged above. This thread shares the receiver,
                            // so it never disconnects, and the slot was emptied before
                            // this stream was served with only this thread filling
                            // it: the send has no way to fail.
                            drop(failure_tx.try_send(err));
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(BRIDGE_ACCEPT_POLL);
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            noninteractive,
                            target = %target.as_str(),
                            socket = %thread_socket.display(),
                            "remote SSH bridge listener failed"
                        );
                        // Already logged above. The send fails only when the last
                        // stream's failure is still unclaimed; the owner then reads
                        // that one, which is the failure its request actually saw.
                        drop(failure_tx.try_send(io::Error::new(
                            err.kind(),
                            format!("remote bridge listener failed: {err}"),
                        )));
                        break;
                    }
                }
            }
        });

        let bridge = Self {
            local_socket,
            socket_identity,
            _socket_startup_lock: socket_startup_lock,
            should_stop,
            failure_rx,
            thread: Some(thread),
            _teardown: teardown,
        };
        socket_cleanup.disarm();
        Ok(bridge)
    }

    pub(crate) fn reported_failure(&self) -> Option<io::Error> {
        // A local client can observe EOF before this worker has reaped ssh and
        // sent its exit diagnostic. Polling keeps the receiver mutex available
        // to the accept thread while it discards an unclaimed earlier failure.
        // clock-io-ok: allow the SSH worker time to report after stream EOF.
        let deadline = Instant::now() + BRIDGE_FAILURE_REPORT_TIMEOUT;
        loop {
            let received = self
                .failure_rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_recv();
            match received {
                Ok(error) => return Some(error),
                Err(mpsc::TryRecvError::Disconnected) => return None,
                Err(mpsc::TryRecvError::Empty) => {}
            }

            // clock-io-ok: the worker runs concurrently while the caller polls.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            thread::sleep(remaining.min(BRIDGE_FAILURE_REPORT_POLL_INTERVAL));
        }
    }
}

fn discard_unclaimed_bridge_failure(failure_rx: &std::sync::Mutex<mpsc::Receiver<io::Error>>) {
    let failure_rx = failure_rx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while failure_rx.try_recv().is_ok() {}
}

pub(super) fn prepare_remote_bridge_stream(
    mut stream: shepr_platform::ipc::LocalStream,
) -> io::Result<shepr_platform::ipc::LocalStream> {
    shepr_platform::ipc::set_local_stream_polling(&mut stream, false)?;
    Ok(stream)
}

impl Drop for SshStdioBridge {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Release);
        remove_bridge_socket(&self.local_socket, &self.socket_identity);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            // The panic itself went to the panic hook; this ties it to the bridge.
            tracing::error!(
                socket = %self.local_socket.display(),
                "remote bridge accept thread panicked"
            );
        }
    }
}

/// Removes a bridge's own socket on a failed start or on drop. The caller has
/// nothing better to do with a failure, but a socket file left behind in the
/// runtime directory is worth a line naming it.
fn remove_bridge_socket(path: &Path, identity: &shepr_platform::ipc::SocketFileIdentity) {
    if let Err(error) = shepr_platform::ipc::remove_socket_file_if_owned(path, identity) {
        tracing::warn!(%error, socket = %path.display(), "could not remove remote bridge socket");
    }
}

/// Owns the newly bound socket until the bridge itself is ready to own cleanup.
struct BridgeSocketStartupCleanup {
    path: PathBuf,
    identity: shepr_platform::ipc::SocketFileIdentity,
    armed: bool,
}

impl BridgeSocketStartupCleanup {
    fn new(path: &Path, identity: &shepr_platform::ipc::SocketFileIdentity) -> Self {
        Self {
            path: path.to_owned(),
            identity: identity.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BridgeSocketStartupCleanup {
    fn drop(&mut self) {
        if self.armed {
            remove_bridge_socket(&self.path, &self.identity);
        }
    }
}

pub(super) struct BridgeUploadStop {
    stopped: AtomicBool,
    pub(super) wake: shepr_platform::RemoteBridgeWake,
}

impl BridgeUploadStop {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            stopped: AtomicBool::new(false),
            wake: shepr_platform::RemoteBridgeWake::new()?,
        })
    }

    pub(super) fn cancel(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel)
            && let Err(error) = self.wake.cancel()
        {
            tracing::debug!(%error, "remote bridge read cancellation failed");
        }
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

/// The upload half of a bridge connection: copies what the local client
/// writes to its socket into `writer` (the ssh child's stdin) on a thread of
/// its own, until the upload is cancelled, the bridge stops, or the client
/// closes its end. Cancelling never closes the socket, so the download half
/// keeps delivering what the remote end still sends.
pub struct BridgeUpload {
    stop: Arc<BridgeUploadStop>,
    failed: Arc<AtomicBool>,
    client_closed: Arc<AtomicBool>,
    worker: JoinHandle<io::Result<u64>>,
}

/// How an upload ended: the copy's result and whether the local client closed
/// its end.
pub struct BridgeUploadEnd {
    pub result: io::Result<u64>,
    pub client_closed: bool,
}

impl BridgeUpload {
    pub fn spawn(
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

    /// Stop copying. Bytes already read are still written.
    pub fn cancel(&self) {
        self.stop.cancel();
    }

    pub(super) fn stop_handle(&self) -> Arc<BridgeUploadStop> {
        Arc::clone(&self.stop)
    }

    /// The copy failed; set once it has ended.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn client_closed(&self) -> bool {
        self.client_closed.load(Ordering::Acquire)
    }

    pub fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }

    /// Wait for the copy to end.
    pub fn join(self) -> io::Result<BridgeUploadEnd> {
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

pub(super) fn bridge_connection(
    mut stream: shepr_platform::ipc::LocalStream,
    target: &SshTarget,
    remote_command: &str,
    ssh_options: Option<&ManagedSshOptions>,
    noninteractive: bool,
    bridge_stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut command = ssh_command();
    apply_managed_ssh_options(&mut command, ssh_options);
    if noninteractive {
        apply_noninteractive_ssh_options(&mut command);
    }
    command
        .arg("-T")
        .arg(target.as_str())
        .arg(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if noninteractive {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });

    let child = command
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("failed to start ssh bridge: {err}")))?;
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
    let stderr_reader = if noninteractive {
        let child_stderr = child.child()?.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ssh bridge stderr missing")
        })?;
        Some(PipeCapture::spawn(
            child_stderr,
            SSH_STDERR_CAPTURE_LIMIT,
            PipeEcho::None,
        ))
    } else {
        None
    };
    let stream_to_child = stream.try_clone()?;
    shepr_platform::ipc::set_local_stream_polling(&mut stream, true)?;
    let mut child_to_stream = stream;

    let connection_stop = Arc::new(AtomicBool::new(false));
    let download_done = Arc::new(AtomicBool::new(false));
    let upload = BridgeUpload::spawn(stream_to_child, child_stdin, Arc::clone(bridge_stop))?;
    let mut child = child.into_child()?;
    let upload_stop = upload.stop_handle();
    let download_stop = Arc::clone(&connection_stop);
    let download_bridge_stop = Arc::clone(bridge_stop);
    let download_done_worker = Arc::clone(&download_done);
    let download_upload_stop = Arc::clone(&upload_stop);
    let download = thread::spawn(move || {
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
        thread::sleep(BRIDGE_ACCEPT_POLL);
    };
    upload_stop.cancel();
    if !child_exited {
        connection_stop.store(true, Ordering::Release);
    }
    let BridgeUploadEnd {
        result: upload_result,
        client_closed,
    } = upload.join()?;
    let download_result = download
        .join()
        .map_err(|_| io::Error::other("remote bridge download worker panicked"))?;
    // Bounded: a ControlPersist master forked by this ssh can hold its stderr open for
    // the whole persist timeout after the bridge itself has exited.
    let stderr = match stderr_reader {
        Some(reader) => reader.finish(PIPE_DRAIN_GRACE)?,
        None => Vec::new(),
    };
    let status = status_result?;

    let stopping = bridge_stop.load(Ordering::Acquire);
    if child_exited && !status.success() && !stopping && !client_closed {
        return Err(ssh_bridge_exit_error(status, &stderr));
    }
    if !stopping && !client_closed {
        upload_result.map_err(|err| {
            let diagnostic = super::SshFailureDiagnostic::from_error(&err)
                .with_context("remote bridge upload failed");
            io::Error::new(err.kind(), diagnostic)
        })?;
        download_result.map_err(|err| {
            let diagnostic = super::SshFailureDiagnostic::from_error(&err)
                .with_context("remote bridge download failed");
            io::Error::new(err.kind(), diagnostic)
        })?;
    }

    if status.success() || stopping || client_closed {
        Ok(())
    } else {
        Err(ssh_bridge_exit_error(status, &stderr))
    }
}

/// Classify an SSH bridge exit. The remote stderr goes into the message
/// unredacted, for the reason given at `ssh::command_failed`.
pub(super) fn ssh_bridge_exit_error(status: std::process::ExitStatus, stderr: &[u8]) -> io::Error {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = super::server_lifecycle::printable_remote_text(stderr.trim());
    let (failure, exit_status) = match status.code() {
        Some(SSH_OWN_FAILURE_EXIT_CODE) => (
            "remote SSH connection failed",
            format!("exit status {SSH_OWN_FAILURE_EXIT_CODE}"),
        ),
        Some(REMAPPED_REMOTE_255_EXIT_CODE) => {
            // SSH exposes one exit byte, so remapping remote 255 to 254 aliases
            // a native remote 254. Name the mapping without guessing which ran.
            (
                "remote command failed",
                format!(
                    "reported exit status {REMAPPED_REMOTE_255_EXIT_CODE} (remote status {SSH_OWN_FAILURE_EXIT_CODE} is remapped to {REMAPPED_REMOTE_255_EXIT_CODE}; a native {REMAPPED_REMOTE_255_EXIT_CODE} is indistinguishable)"
                ),
            )
        }
        code => {
            let exit_status =
                code.map_or_else(|| status.to_string(), |code| format!("exit status {code}"));
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
        super::SshFailureDiagnostic::from_ssh_output(status.code(), message),
    )
}

/// A connection attempt that ran out of its time budget. `TimedOut`, so it counts as a link
/// failure (no rediscovery) and a transient one (a retry, not attention).
pub(crate) fn attempt_deadline_passed() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "SSH connection attempt ran out of time",
    )
}

/// OpenSSH exits with 255 when ssh itself fails (resolve, connect, host key,
/// authentication, a dropped link); any other code came from the remote command.
// limits-exempt: an exit status of the OpenSSH and remote-shell contract.
pub(crate) const SSH_OWN_FAILURE_EXIT_CODE: i32 = 255;
// limits-exempt: remote exit 255 is remapped to 254 to keep it apart from SSH failures.
pub(crate) const REMAPPED_REMOTE_255_EXIT_CODE: i32 = 254;

/// Whether `error` says the SSH link, not the remote side, failed: the remote end
/// was never reached or was lost, so nothing is known about the remote install.
pub(crate) fn is_ssh_link_failure(error: &io::Error) -> bool {
    super::SshFailureDiagnostic::from_error(error).is_link_failure()
}

pub(super) fn discard_remote_output_preamble(reader: &mut impl io::BufRead) -> io::Result<()> {
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

pub(super) fn copy_reader_to_local_stream<R: io::Read>(
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
pub(super) fn copy_local_stream_to_writer<W: io::Write>(
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

    fn wait_for_input(&self, wake: &shepr_platform::RemoteBridgeWake) -> io::Result<()>;
}

impl UploadReadStream for shepr_platform::ipc::LocalStream {
    fn poll_read_count(
        &mut self,
        buffer: &mut [u8],
    ) -> io::Result<shepr_platform::ipc::LocalStreamReadCount> {
        shepr_platform::ipc::poll_local_stream_read_count(self, buffer)
    }

    fn wait_for_input(&self, wake: &shepr_platform::RemoteBridgeWake) -> io::Result<()> {
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

/// Runs the foreground client for `shepr --remote`. It runs in `launch_dir`,
/// the directory the user ran `shepr` from: it is a foreground child that ends
/// with this process, so it pins nothing the user's shell does not already.
pub(super) fn run_client_process(
    local_socket: &Path,
    reattach_command: &str,
    keybindings: RemoteKeybindings,
    launch_dir: &Path,
) -> io::Result<()> {
    let exe = shepr_platform::launch_executable()?;
    let status = shepr_platform::child_command(exe, launch_dir)
        .arg("client")
        .env(shepr_core::env::EnvVar::SheprClientSocketPath, local_socket)
        .env(
            shepr_core::env::EnvVar::SheprReattachCommand,
            reattach_command,
        )
        .env(
            shepr_core::env::EnvVar::SheprRemoteKeybindings,
            keybindings.to_env_value(),
        )
        .env_remove(shepr_core::env::EnvVar::SheprSocketPath)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!("remote client exited with {status}"),
        ))
    }
}

/// The `--remote` bridge's local socket. The name carries the SSH target
/// (`user@host`) so the owner can tell sockets apart; that is not a leak,
/// because `remote_bridge_endpoint_path` only accepts a runtime directory
/// owned by the user with mode 0700, so no one else can list it. Saved
/// machines name theirs by profile id because the profile, not the target,
/// is their identity.
pub(super) fn local_forward_socket_path(
    runtime_dir: &Path,
    target: &str,
    session_name: &str,
) -> io::Result<PathBuf> {
    let target_clean = sanitize_path_component(target);
    let session_clean = sanitize_path_component(session_name);
    let readable_name = format!("shepr-remote-{target_clean}-{session_clean}.sock");
    let target_prefix: String = target_clean
        .chars()
        .take(BRIDGE_TARGET_PREFIX_CHARS)
        .collect();
    let hash = short_socket_hash(target, session_name);
    let short_name = format!("shepr-r-{target_prefix}-{hash}.sock");
    shepr_platform::remote_bridge_endpoint_path(runtime_dir, &readable_name, &short_name)
}

pub(super) fn short_socket_hash(target: &str, session: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    // Keep a fixed separator between the target and session fields in this hash format.
    0u8.hash(&mut hasher);
    session.hash(&mut hasher);
    // Keep all 64 hash bits in the socket name as fixed width hexadecimal.
    format!("{:016x}", hasher.finish())
}

pub(super) fn sanitize_path_component(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();

    sanitized
        .trim_matches('-')
        .chars()
        .take(BRIDGE_PATH_COMPONENT_MAX_CHARS)
        .collect()
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod bridge_tests;
