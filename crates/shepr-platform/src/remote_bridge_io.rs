use super::*;
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    time::Duration,
};

/// How a stdio relay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "an idle-expired bridge must end its process"]
pub enum RemoteBridgeOutcome {
    /// The server side closed and everything it sent reached stdout.
    Closed,
    /// The idle watchdog fired: no byte moved in either direction for the idle
    /// timeout. `idle_for` is absent only if the watchdog could not read its
    /// clock. The socket has been shut down, but a relay thread may still be
    /// blocked writing to a full stdout or reading stdin, and nothing can
    /// interrupt either. The caller must end the process promptly (exit status
    /// 1) and must not join, wait for, or write to stdout behind those threads.
    IdleExpired { idle_for: Option<Duration> },
}

enum RelayEvent {
    Download(std::io::Result<()>),
    Expired(Option<Duration>),
}

/// Relay the SSH bridge's stdio to the local server socket. Every remote
/// keystroke and paste passes through here as raw bytes: input content must
/// stay out of logs and error messages here and in `remote_bridge` (log byte
/// counts or error kinds, never the buffers).
///
/// With `idle_timeout`, a watchdog ends the relay after
/// `remote_bridge::IDLE_TIMEOUT` without traffic and this returns
/// [`RemoteBridgeOutcome::IdleExpired`]; see that variant for what the caller
/// owes. Without it the relay only ends when the server side closes.
pub fn forward_remote_bridge_stdio(
    stream: crate::ipc::LocalStream,
    idle_timeout: bool,
) -> std::io::Result<RemoteBridgeOutcome> {
    forward_remote_bridge_stdio_with_timeout(
        stream,
        idle_timeout.then_some(remote_bridge::IDLE_TIMEOUT),
    )
}

pub(super) fn forward_remote_bridge_stdio_with_timeout(
    stream: crate::ipc::LocalStream,
    idle_timeout: Option<Duration>,
) -> std::io::Result<RemoteBridgeOutcome> {
    use remote_bridge::{Activity, TrackedIo};
    use std::os::fd::AsFd as _;

    let (events, relay) = std::sync::mpsc::channel();
    let activity = match idle_timeout {
        Some(timeout) => {
            let expired = events.clone();
            Some(Activity::start(timeout, move |idle_for| {
                // The receiver is gone only once the relay already returned
                // on a finished download, so there is nothing left to end.
                expired.send(RelayEvent::Expired(idle_for)).ok();
            })?)
        }
        None => None,
    };
    // The bridge's stdout is the SSH channel to the client, relayed as bytes.
    // A duplicate of fd 1 rather than the std handle: a download blocked on a
    // full stdout pipe must not hold std's stdout lock after an expiry returns.
    // stdout-handoff-ok: fd 1 is taken over whole, not written as text.
    let stdout = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    let mut stdout = TrackedIo::new(stdout, activity.clone());
    let mut socket_to_stdout = TrackedIo::new(stream.try_clone()?, activity.clone());
    let control = stream.try_clone()?;
    let mut stdin_to_socket = stream;
    let _upload = std::thread::spawn(move || {
        let mut stdin = TrackedIo::new(std::io::stdin(), activity.clone());
        // The download side reports how the relay ended. A socket the server
        // closed (or the idle watchdog shut down) is an ordinary end for the
        // upload; anything else means remote input stopped reaching the
        // server, which is worth an operator's attention. Only the error kind
        // is logged, never input.
        if let Err(err) = copy_flush(
            &mut stdin,
            &mut TrackedIo::new(&mut stdin_to_socket, activity),
        ) && !is_closed_socket(&err)
        {
            tracing::warn!(error_kind = ?err.kind(), error = %err, "SSH bridge upload failed");
        }
        if let Err(err) = stdin_to_socket.shutdown(std::net::Shutdown::Write)
            && !is_closed_socket(&err)
        {
            tracing::warn!(error = %err, "SSH bridge failed to half-close the server socket");
        }
    });
    let _download = std::thread::spawn(move || {
        let result = copy_flush(&mut socket_to_stdout, &mut stdout);
        // The receiver is gone only after an idle expiry already returned,
        // and the process is ending then; the download result is moot.
        events.send(RelayEvent::Download(result)).ok();
    });
    match relay.recv() {
        Ok(RelayEvent::Download(result)) => result.map(|()| RemoteBridgeOutcome::Closed),
        Ok(RelayEvent::Expired(idle_for)) => {
            // Unblocks both socket copies; stdin and stdout cannot be
            // interrupted, which is why the caller must end the process.
            // The caller ends the process next, which closes the socket
            // regardless; a failure only delays the server noticing.
            if let Err(err) = control.shutdown(std::net::Shutdown::Both)
                && !is_closed_socket(&err)
            {
                tracing::warn!(error = %err, "SSH bridge failed to shut down the idle server socket");
            }
            Ok(RemoteBridgeOutcome::IdleExpired { idle_for })
        }
        Err(std::sync::mpsc::RecvError) => Err(std::io::Error::other(
            "the bridge relay thread ended without reporting",
        )),
    }
}

/// Whether `err` only says the server side of the socket is already gone.
fn is_closed_socket(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
    )
}

fn copy_flush<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> std::io::Result<()> {
    let mut buffer = [0_u8; super::limits::REMOTE_BRIDGE_COPY_BUFFER_BYTES];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        writer.write_all(&buffer[..read])?;
        writer.flush()?;
    }
}

pub struct RemoteBridgeWake {
    reader: std::os::unix::net::UnixStream,
    writer: std::os::unix::net::UnixStream,
}

impl RemoteBridgeWake {
    pub fn new() -> std::io::Result<Self> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        Ok(Self { reader, writer })
    }

    pub fn cancel(&self) -> std::io::Result<()> {
        // EOF stays readable, including when cancellation precedes the wait.
        self.writer.shutdown(std::net::Shutdown::Write)
    }

    pub fn wait(&self, stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
        use std::os::fd::AsFd as _;
        let mut descriptors = [
            libc::pollfd {
                fd: stream.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: both descriptors remain borrowed and the array has two entries.
            if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) } >= 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// Answers a bridge client directly through the bridge's byte-stream stdout.
pub fn answer_remote_bridge(bytes: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsFd as _;
    // stdout-handoff-ok: fd 1 is taken over whole, not written as text.
    let mut stdout = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    stdout.write_all(bytes)?;
    stdout.flush()
}
