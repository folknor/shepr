use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;

pub(crate) type LocalListener = interprocess::local_socket::Listener;
pub(crate) type LocalStream = interprocess::local_socket::Stream;

pub(crate) enum LocalStreamRead {
    Data,
    Pending,
    Closed,
}

pub(crate) enum LocalStreamReadCount {
    Data(usize),
    Pending,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    dev: u64,
    ino: u64,
}

pub(crate) fn connect_local_stream(path: &Path) -> io::Result<LocalStream> {
    use interprocess::local_socket::{GenericFilePath, prelude::*};

    let name = path.to_fs_name::<GenericFilePath>()?;
    LocalStream::connect(name)
}

pub(crate) fn bind_local_listener(path: &Path) -> io::Result<LocalListener> {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};

    let name = path.to_fs_name::<GenericFilePath>()?;
    ListenerOptions::new()
        .name(name)
        .reclaim_name(false)
        .create_sync()
}

pub(crate) fn prepare_socket_path(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    if !path.exists() {
        return Ok(());
    }

    match connect_local_stream(path) {
        Ok(_) => {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, busy_message(path)));
        }
        Err(err) if stale_socket_connect_error(err.kind()) => {}
        Err(err) => return Err(err),
    }

    if let Err(err) = fs::remove_file(path)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err);
    }

    Ok(())
}

fn stale_socket_connect_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound | io::ErrorKind::TimedOut
    )
}

/// Reports whether the peer has closed or shut down its write side.
///
/// Pure readiness check: it neither reads (unread bytes stay in the socket, so
/// a framed stream keeps its alignment and a trailing newline after a request
/// is not mistaken for a hang-up) nor touches the blocking mode. `POLLRDHUP`
/// fires even when unread data is still queued ahead of the EOF, so a client
/// that wrote something and then disconnected is still seen as closed.
pub(crate) fn local_stream_peer_closed(stream: &LocalStream) -> io::Result<bool> {
    use std::os::fd::{AsFd as _, AsRawFd as _};

    let fd = match stream {
        LocalStream::UdSocket(inner) => inner.as_fd().as_raw_fd(),
    };
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLRDHUP,
        revents: 0,
    };
    loop {
        // SAFETY: `pollfd` is a valid, exclusively borrowed array of length 1
        // for the duration of the call, and `fd` stays open because `stream`
        // is borrowed. A zero timeout makes the call non-blocking.
        let ready = unsafe { libc::poll(&raw mut pollfd, 1, 0) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let hangup = libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
        return Ok(ready > 0 && pollfd.revents & hangup != 0);
    }
}

pub(crate) fn set_local_stream_polling(stream: &mut LocalStream, enabled: bool) -> io::Result<()> {
    stream.set_nonblocking(enabled)
}

/// Mode applied by [`bind_private_local_listener`]: owner read/write only.
const PRIVATE_SOCKET_MODE: u32 = 0o600;

/// Binds a listener and restricts the socket file to its owner before
/// returning. Callers may still tighten or re-apply the mode afterwards.
///
/// The socket file exists with umask-derived permissions for the moment
/// between `bind` and `chmod`; closing that window needs a private parent
/// directory, which is the caller's to provide.
pub(crate) fn bind_private_local_listener(path: &Path) -> io::Result<LocalListener> {
    let listener = bind_local_listener(path)?;
    if let Err(error) = restrict_socket_permissions(path, PRIVATE_SOCKET_MODE) {
        drop(listener);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(listener)
}

/// Reader that enforces one overall deadline across any number of reads.
///
/// `SO_RCVTIMEO` alone is a per-read idle timeout: it restarts on every
/// successful read, so a peer sending one byte just inside each timeout keeps
/// a blocking read loop alive indefinitely. This wrapper re-arms the receive
/// timeout with only the time left before each read and fails with
/// `TimedOut` once the deadline has passed. The stream must be in blocking
/// mode; the caller clears the receive timeout when it is done.
pub(crate) struct DeadlineReader<'a> {
    stream: &'a mut LocalStream,
    deadline: Instant,
}

impl<'a> DeadlineReader<'a> {
    pub(crate) fn new(stream: &'a mut LocalStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(deadline_passed());
        }
        // A sub-microsecond timeout could round to zero, which the socket
        // layer treats as "no timeout"; never arm less than a millisecond.
        self.stream
            .set_recv_timeout(Some(remaining.max(Duration::from_millis(1))))?;
        match self.stream.read(buf) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(deadline_passed())
            }
            result => result,
        }
    }
}

fn deadline_passed() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "read deadline passed")
}

pub(crate) fn poll_local_stream_read(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamRead> {
    match poll_local_stream_read_count(stream, buf)? {
        LocalStreamReadCount::Data(read) => {
            let _ = read;
            Ok(LocalStreamRead::Data)
        }
        LocalStreamReadCount::Pending => Ok(LocalStreamRead::Pending),
        LocalStreamReadCount::Closed => Ok(LocalStreamRead::Closed),
    }
}

pub(crate) fn poll_local_stream_read_count(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamReadCount> {
    match stream.read(buf) {
        Ok(0) => Ok(LocalStreamReadCount::Closed),
        Ok(read) => Ok(LocalStreamReadCount::Data(read)),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(LocalStreamReadCount::Pending),
        Err(err) => Err(err),
    }
}

pub(crate) fn is_connection_closed_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WriteZero
    )
}

pub(crate) fn socket_file_identity(path: &Path) -> io::Result<SocketFileIdentity> {
    let metadata = fs::metadata(path)?;
    Ok(SocketFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

pub(crate) fn remove_socket_file_if_owned(
    path: &Path,
    identity: &SocketFileIdentity,
) -> io::Result<()> {
    let current = match socket_file_identity(path) {
        Ok(current) => current,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if current != *identity {
        return Ok(());
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub(crate) fn restrict_socket_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_socket_connect_errors_keep_unix_would_block_strict() {
        assert!(stale_socket_connect_error(io::ErrorKind::ConnectionRefused));
        assert!(stale_socket_connect_error(io::ErrorKind::NotFound));
        assert!(stale_socket_connect_error(io::ErrorKind::TimedOut));
        assert!(!stale_socket_connect_error(io::ErrorKind::WouldBlock));
    }

    fn test_socket_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "shepr-ipc-{name}-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("test precondition")
                .as_nanos()
        ))
    }

    #[test]
    fn private_listener_socket_is_owner_only() {
        let path = test_socket_path("private");
        let listener = bind_private_local_listener(&path).expect("test precondition");
        let mode = fs::metadata(&path).expect("test precondition").mode() & 0o777;
        drop(listener);
        let _ = fs::remove_file(&path);
        assert_eq!(mode, PRIVATE_SOCKET_MODE);
    }

    fn connected_pair(name: &str) -> (LocalStream, LocalStream) {
        use interprocess::local_socket::traits::Listener as _;

        let path = test_socket_path(name);
        let listener = bind_local_listener(&path).expect("test precondition");
        let client = connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        let _ = fs::remove_file(&path);
        (client, server)
    }

    #[test]
    fn peer_closed_probe_leaves_pending_data_unread() {
        use std::io::Write as _;

        let (mut client, mut server) = connected_pair("probe-data");
        assert!(!local_stream_peer_closed(&server).expect("probe"));

        client.write_all(b"\n{}").expect("test precondition");
        // Pending input is not a hang-up, and the probe must not eat it.
        assert!(!local_stream_peer_closed(&server).expect("probe"));
        assert!(!local_stream_peer_closed(&server).expect("probe"));

        // Every byte the probe looked past is still there to read. The
        // timeout only keeps a regression from hanging the test run.
        server
            .set_recv_timeout(Some(Duration::from_secs(2)))
            .expect("test precondition");
        let mut buf = [0u8; 3];
        server.read_exact(&mut buf).expect("data still readable");
        assert_eq!(&buf, b"\n{}");
    }

    #[test]
    fn peer_closed_probe_sees_hangup_behind_unread_data() {
        use std::io::Write as _;

        let (mut client, server) = connected_pair("probe-close");
        client.write_all(b"trailing\n").expect("test precondition");
        drop(client);
        assert!(local_stream_peer_closed(&server).expect("probe"));
    }

    #[test]
    fn deadline_reader_cuts_off_a_trickling_peer() {
        use interprocess::local_socket::traits::Listener as _;
        use std::io::Write as _;

        let path = test_socket_path("deadline");
        let listener = bind_local_listener(&path).expect("test precondition");
        let mut client = connect_local_stream(&path).expect("test precondition");
        let mut server = listener.accept().expect("test precondition");
        let _ = fs::remove_file(&path);

        // One byte every 50 ms: each read finishes well inside any per-read
        // timeout, so only an overall deadline can end the loop.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let trickle_stop = std::sync::Arc::clone(&stop);
        let trickler = std::thread::spawn(move || {
            while !trickle_stop.load(std::sync::atomic::Ordering::Acquire) {
                if client.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let started = Instant::now();
        let mut buf = [0u8; 1024];
        let result = DeadlineReader::new(&mut server, started + Duration::from_millis(300))
            .read_exact(&mut buf);
        let elapsed = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Release);
        drop(server);
        trickler.join().expect("test precondition");

        let error = result.expect_err("a trickling peer must not complete the read");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_secs(2),
            "deadline overran: {elapsed:?}"
        );
    }
}
