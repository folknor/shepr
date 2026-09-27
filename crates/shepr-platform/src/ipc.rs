use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;

pub type LocalListener = interprocess::local_socket::Listener;
pub type LocalStream = interprocess::local_socket::Stream;

pub enum LocalStreamRead {
    Data,
    Pending,
    Closed,
}

pub enum LocalStreamReadCount {
    Data(usize),
    Pending,
    Closed,
}

#[derive(Debug)]
pub enum Liveness {
    Absent,
    Stale,
    Live,
    Unreachable(io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SocketFileIdentity {
    dev: u64,
    ino: u64,
}

pub fn connect_local_stream(path: &Path) -> io::Result<LocalStream> {
    use interprocess::local_socket::{GenericFilePath, prelude::*};

    let name = path.to_fs_name::<GenericFilePath>()?;
    LocalStream::connect(name)
}

pub fn bind_local_listener(path: &Path) -> io::Result<LocalListener> {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};

    let name = path.to_fs_name::<GenericFilePath>()?;
    ListenerOptions::new()
        .name(name)
        .reclaim_name(false)
        .create_sync()
}

/// Probe a local server socket and classify whether it is absent, stale, live,
/// or present but unreachable. Timeouts and access failures stay errors because
/// they do not prove that no server is listening.
pub fn probe(path: &Path) -> Liveness {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Liveness::Absent,
        Err(error) => return Liveness::Unreachable(error),
        Ok(_) => {}
    }

    match connect_local_stream(path) {
        Ok(_) => Liveness::Live,
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => Liveness::Stale,
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::symlink_metadata(path) {
            Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                Liveness::Absent
            }
            Err(metadata_error) => Liveness::Unreachable(metadata_error),
            Ok(_) => Liveness::Stale,
        },
        Err(error) => Liveness::Unreachable(error),
    }
}

pub fn prepare_socket_path(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    match probe(path) {
        Liveness::Absent => return Ok(()),
        Liveness::Live => {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, busy_message(path)));
        }
        Liveness::Stale => {}
        Liveness::Unreachable(error) => return Err(error),
    }

    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error);
    }

    Ok(())
}

/// Reports whether the peer has closed or shut down its write side.
///
/// Pure readiness check: it neither reads (unread bytes stay in the socket, so
/// a framed stream keeps its alignment and a trailing newline after a request
/// is not mistaken for a hang-up) nor touches the blocking mode. `POLLRDHUP`
/// fires even when unread data is still queued ahead of the EOF, so a client
/// that wrote something and then disconnected is still seen as closed.
pub fn local_stream_peer_closed(stream: &LocalStream) -> io::Result<bool> {
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

pub fn set_local_stream_polling(stream: &mut LocalStream, enabled: bool) -> io::Result<()> {
    stream.set_nonblocking(enabled)
}

/// Mode applied by [`bind_private_local_listener`]: owner read/write only.
const PRIVATE_SOCKET_MODE: u32 = 0o600;

/// How many staging directory names are tried before giving up on staging.
/// A collision needs another process to have created exactly that name, so
/// more than one retry only matters against someone guessing names in a
/// shared parent directory.
const STAGING_ATTEMPTS: u32 = 4;

/// Binds a listener at `path` so the socket is never reachable with anything
/// looser than owner-only permissions.
///
/// Binding at `path` and then chmodding leaves the socket connectable with
/// umask-derived permissions in between. Instead the socket is bound inside a
/// fresh 0700 staging directory next to `path`, restricted to 0600 there, and
/// then hard-linked into place. `link` fails if `path` already exists, so a
/// listener that raced us to the path is never replaced (a `bind` at the path
/// would have failed the same way); that is reported as `AddrInUse`. The
/// listener is bound to the inode, so connections through the new name reach
/// it, and a socket identity recorded from `path` afterwards is that inode.
///
/// If staging cannot be used (a staged path over the socket path length
/// limit, or a filesystem without hard links), this falls back to
/// bind-then-chmod at `path`, which still ends owner-only. Callers may tighten
/// or re-apply the mode afterwards. Access is also checked per connection by
/// [`peer_is_same_user`]; the file mode is not the only control.
pub fn bind_private_local_listener(path: &Path) -> io::Result<LocalListener> {
    match bind_via_private_staging(path) {
        Ok(listener) => Ok(listener),
        Err(StagedBindError::Busy) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("socket busy at {}", path.display()),
        )),
        Err(StagedBindError::Unavailable(err)) => {
            tracing::warn!(
                path = %path.display(),
                err = %err,
                "private socket staging failed; binding in place"
            );
            bind_in_place_then_restrict(path)
        }
    }
}

fn bind_in_place_then_restrict(path: &Path) -> io::Result<LocalListener> {
    let listener = bind_local_listener(path)?;
    if let Err(error) = restrict_socket_permissions(path, PRIVATE_SOCKET_MODE) {
        drop(listener);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(listener)
}

enum StagedBindError {
    /// Something already exists at the target path.
    Busy,
    /// Staging itself failed; binding in place may still work.
    Unavailable(io::Error),
}

fn bind_via_private_staging(path: &Path) -> Result<LocalListener, StagedBindError> {
    use std::os::unix::fs::DirBuilderExt as _;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut last_error = None;
    for attempt in 0..STAGING_ATTEMPTS {
        // Kept short: the staged path must fit the socket path length limit.
        let staging_dir = parent.join(format!(".shepr-{}-{nanos:x}-{attempt}", std::process::id()));
        // A name somebody else already created is never used: the directory
        // must be ours and fresh for the 0700 guarantee to hold.
        match fs::DirBuilder::new().mode(0o700).create(&staging_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(err);
                continue;
            }
            Err(err) => return Err(StagedBindError::Unavailable(err)),
        }
        let staged = staging_dir.join("s");
        let result = bind_staged_and_link(&staged, path);
        let _ = fs::remove_file(&staged);
        let _ = fs::remove_dir(&staging_dir);
        return result;
    }
    Err(StagedBindError::Unavailable(last_error.unwrap_or_else(
        || {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "no free staging directory name",
            )
        },
    )))
}

fn bind_staged_and_link(staged: &Path, path: &Path) -> Result<LocalListener, StagedBindError> {
    let listener = bind_local_listener(staged).map_err(StagedBindError::Unavailable)?;
    restrict_socket_permissions(staged, PRIVATE_SOCKET_MODE)
        .map_err(StagedBindError::Unavailable)?;
    match fs::hard_link(staged, path) {
        Ok(()) => Ok(listener),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(StagedBindError::Busy),
        Err(err) => Err(StagedBindError::Unavailable(err)),
    }
}

/// Reports whether the peer of an accepted local connection runs as the same
/// user as this process (its effective uid), or as root.
///
/// Every shepr socket is owner-only, so in normal operation this always
/// holds; it is a second check that does not depend on the socket file's
/// mode, which can be loosened after the fact or, on the in-place bind
/// fallback, briefly be umask-derived. Root is admitted because root can
/// connect through the 0600 mode anyway, and refusing it would only break
/// `sudo` use without protecting anything. The credentials are the ones the
/// peer had when it connected (`SO_PEERCRED`), so a later privilege drop by
/// the peer does not change the answer.
pub fn peer_is_same_user(stream: &LocalStream) -> io::Result<bool> {
    use std::os::fd::{AsFd as _, AsRawFd as _};

    let fd = match stream {
        LocalStream::UdSocket(inner) => inner.as_fd().as_raw_fd(),
    };
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    // `size_of::<ucred>()` is a small compile-time constant, well within
    // `socklen_t` (u32) range, so this cast never truncates.
    #[allow(clippy::cast_possible_truncation)]
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a valid, exclusively borrowed `ucred` and `len` holds
    // its exact size, which is what `SO_PEERCRED` writes; `fd` stays open for
    // the call because `stream` is borrowed.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned a short credential record",
        ));
    }
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let own_uid = unsafe { libc::geteuid() };
    Ok(cred.uid == own_uid || cred.uid == 0)
}

/// Reader that enforces one overall deadline across any number of reads.
///
/// `SO_RCVTIMEO` alone is a per-read idle timeout: it restarts on every
/// successful read, so a peer sending one byte just inside each timeout keeps
/// a blocking read loop alive indefinitely. This wrapper re-arms the receive
/// timeout with only the time left before each read and fails with
/// `TimedOut` once the deadline has passed. The stream must be in blocking
/// mode; the caller clears the receive timeout when it is done.
pub struct DeadlineReader<'a> {
    stream: &'a mut LocalStream,
    deadline: Instant,
}

impl<'a> DeadlineReader<'a> {
    pub fn new(stream: &'a mut LocalStream, deadline: Instant) -> Self {
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

pub fn poll_local_stream_read(
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

pub fn poll_local_stream_read_count(
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

pub fn is_connection_closed_error(err: &io::Error) -> bool {
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

pub fn socket_file_identity(path: &Path) -> io::Result<SocketFileIdentity> {
    let metadata = fs::metadata(path)?;
    Ok(SocketFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

pub fn remove_socket_file_if_owned(path: &Path, identity: &SocketFileIdentity) -> io::Result<()> {
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

pub fn restrict_socket_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_classifies_absent_stale_and_live_sockets() {
        let dir = shepr_test_support::ScratchDir::new("probe-liveness");
        let path = dir.join("server.sock");
        assert!(matches!(probe(&path), Liveness::Absent));

        {
            let _listener =
                std::os::unix::net::UnixListener::bind(&path).expect("bind stale socket");
        }
        assert!(matches!(probe(&path), Liveness::Stale));

        let live_path = dir.join("live.sock");
        let _listener =
            std::os::unix::net::UnixListener::bind(&live_path).expect("bind live socket");
        assert!(matches!(probe(&live_path), Liveness::Live));
    }

    /// A socket path in a scratch directory kept until the test process exits.
    fn test_socket_path(name: &str) -> std::path::PathBuf {
        shepr_test_support::ScratchDir::new(name)
            .keep_until_exit()
            .join("s.sock")
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

    #[test]
    fn private_listener_is_linked_into_place_and_never_replaces_a_path() {
        use interprocess::local_socket::traits::Listener as _;

        let dir = test_socket_path("staging-dir");
        fs::create_dir_all(&dir).expect("test precondition");
        let path = dir.join("api.sock");

        let listener = bind_private_local_listener(&path).expect("bind");
        assert_eq!(
            fs::metadata(&path).expect("socket exists").mode() & 0o777,
            PRIVATE_SOCKET_MODE
        );
        // The staging directory is gone; only the socket is left, and the
        // linked name reaches the listener.
        let entries = fs::read_dir(&dir)
            .expect("test precondition")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from("api.sock")]);
        let _client = connect_local_stream(&path).expect("connect through the linked name");
        assert!(listener.accept().is_ok());

        // A second bind never replaces what is already there.
        let err = bind_private_local_listener(&path).expect_err("path is taken");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(listener);

        let plain = dir.join("plain");
        fs::write(&plain, b"keep").expect("test precondition");
        let err = bind_private_local_listener(&plain).expect_err("path is taken");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(fs::read(&plain).expect("file kept"), b"keep");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_credentials_admit_this_user() {
        let (client, server) = connected_pair("peercred");
        assert!(peer_is_same_user(&server).expect("SO_PEERCRED"));
        assert!(peer_is_same_user(&client).expect("SO_PEERCRED"));
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
