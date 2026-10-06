use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
pub use shepr_core::socket_path::SocketPath;

/// A plain alias on purpose. Trust is enforced where a connection is made
/// (`TrustedServerStream`, a peer-uid check before any protocol write) and where
/// one is accepted (the accept-side uid admission), not by this name. Accepted
/// and test streams are legitimately plain `UnixStream`s, so a newtype here
/// would only add wrappers across every transport.
pub type LocalStream = std::os::unix::net::UnixStream;

/// A connected server whose peer uid was checked before any protocol write.
#[derive(Debug)]
pub struct TrustedServerStream(LocalStream);

impl TrustedServerStream {
    /// Transfer the admitted connection into an existing stream transport.
    ///
    /// Trust is established once, at connect, by the peer-uid check, and the
    /// wrapper deliberately ends here: the returned stream is the same checked
    /// socket. Carrying the wrapper through the transport types would only
    /// guard against a hypothetical future connect that skips the check.
    pub fn into_local_stream(self) -> LocalStream {
        self.0
    }
}

impl std::ops::Deref for TrustedServerStream {
    type Target = LocalStream;
    fn deref(&self) -> &LocalStream {
        &self.0
    }
}
impl std::ops::DerefMut for TrustedServerStream {
    fn deref_mut(&mut self) -> &mut LocalStream {
        &mut self.0
    }
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

/// Whether a listener answers at `path`. `Absent | Stale` is `false`, `Live`
/// is `true`, and `Unreachable` is the error: an inaccessible path never
/// proves absence or permits a successor.
pub fn socket_is_live(path: &Path) -> io::Result<bool> {
    match probe(path) {
        Liveness::Absent | Liveness::Stale => Ok(false),
        Liveness::Live => Ok(true),
        Liveness::Unreachable(error) => Err(error),
    }
}

/// Whether an `accept` failure belongs to the one pending connection (it was
/// aborted, failed its protocol setup, was refused by a security module, or
/// the call was interrupted), so the listener retries at once. Every other
/// failure, descriptor and memory exhaustion included, is the listener's own
/// and calls for a backoff before the next attempt.
fn accept_failed_for_one_connection(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Interrupted
        || matches!(
            error.raw_os_error(),
            Some(libc::ECONNABORTED | libc::EPROTO | libc::EPERM)
        )
}

/// Admission differs by purpose: command sockets admit root as well as the
/// owner; a launch channel must belong to the user that forked the child.
#[derive(Clone, Copy)]
pub enum PeerAdmission {
    OwnerOrRoot,
    ExactOwner,
}

pub struct AdmittedPeer {
    pub fd: std::os::fd::OwnedFd,
    pub pid: u32,
}

pub enum Accepted {
    Peer(AdmittedPeer),
    RetryNow,
    Backoff(io::Error),
    Fatal(io::Error),
}

/// Accept and authenticate one connection. Unknown accept errors back off:
/// only an invalid listener proves that keeping the listener is futile.
pub fn accept_peer(listener: std::os::fd::RawFd, admission: PeerAdmission) -> Accepted {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: accept4 writes no address and returns a fresh close-on-exec fd.
    let fd = unsafe {
        libc::accept4(
            listener,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return classify_accept_failure(io::Error::last_os_error());
    }

    // SAFETY: accept4 returned a new descriptor owned only here.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let credentials = match peer_credentials(fd.as_raw_fd()) {
        Ok(credentials) => credentials,
        Err(error) => {
            warn_peer_rejection("credential lookup failed", Some(&error));
            return Accepted::RetryNow;
        }
    };
    let own_uid = super::effective_uid();
    let admitted = match admission {
        PeerAdmission::OwnerOrRoot => peer_uid_is_allowed_client(credentials.uid, own_uid),
        PeerAdmission::ExactOwner => peer_uid_is_same_effective_user(credentials.uid, own_uid),
    };
    // A command peer outside our PID namespace may report pid zero. Launch
    // peers are our own forked children and need a positive pid for routing.
    if !admitted || (matches!(admission, PeerAdmission::ExactOwner) && credentials.pid <= 0) {
        warn_peer_rejection("credentials were not admitted", None);
        return Accepted::RetryNow;
    }
    Accepted::Peer(AdmittedPeer {
        fd,
        pid: u32::try_from(credentials.pid).unwrap_or(0),
    })
}

use super::limits::PEER_REJECTION_WARNING_INTERVAL;

struct PeerRejectionWarningState {
    last_warning: Option<Instant>,
    suppressed: u64,
}

static PEER_REJECTION_WARNING_STATE: OnceLock<Mutex<PeerRejectionWarningState>> = OnceLock::new();

/// Rejected local peers are expected to be able to reach the socket. Keep one
/// noisy process from turning repeated failed admission into an unbounded log
/// stream, while periodically reporting the reason and number suppressed.
fn warn_peer_rejection(reason: &'static str, error: Option<&io::Error>) {
    let state = PEER_REJECTION_WARNING_STATE.get_or_init(|| {
        Mutex::new(PeerRejectionWarningState {
            last_warning: None,
            suppressed: 0,
        })
    });
    // clock-io-ok: the warning rate limit measures real time between accepts.
    let now = Instant::now();
    let suppressed = {
        let mut state = match state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let should_warn = match state.last_warning {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= PEER_REJECTION_WARNING_INTERVAL,
        };
        if should_warn {
            state.last_warning = Some(now);
            Some(std::mem::take(&mut state.suppressed))
        } else {
            state.suppressed = state.suppressed.saturating_add(1);
            None
        }
    };
    let Some(suppressed) = suppressed else {
        return;
    };
    crate::structured_log!(
        WARN, event = ipc.peer_admit, outcome = "refused",
        reason,
        error = ?error,
        suppressed,
        "accepted socket peer was not admitted; further rejection warnings are rate-limited"
    );
}

fn classify_accept_failure(error: io::Error) -> Accepted {
    if error.kind() == io::ErrorKind::WouldBlock || accept_failed_for_one_connection(&error) {
        Accepted::RetryNow
    } else if matches!(
        error.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK)
    ) {
        Accepted::Fatal(error)
    } else {
        Accepted::Backoff(error)
    }
}

/// A peer's first byte, observed without consuming it.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstByte {
    Byte(u8),
    /// The peer hung up before sending anything.
    Closed,
}

/// Waits until `deadline` for the peer's first byte and returns it without
/// consuming it, so whoever serves the stream reads it from byte zero. A
/// deadline at or before now polls once without waiting. A passed deadline
/// with nothing to read is `io::ErrorKind::TimedOut`.
pub fn peek_first_byte(stream: &LocalStream, deadline: Instant) -> io::Result<FirstByte> {
    // clock-io-ok: the public entry point supplies the real socket clock.
    peek_first_byte_with_clock(stream, deadline, &Instant::now)
}

fn peek_first_byte_with_clock(
    stream: &LocalStream,
    deadline: Instant,
    now: &dyn Fn() -> Instant,
) -> io::Result<FirstByte> {
    let fd = stream.as_raw_fd();
    loop {
        let timeout = super::child_io::remaining_until(deadline, now()).unwrap_or_default();
        match super::child_io::poll_fd_readable(fd, timeout) {
            Ok(true) => {}
            Ok(false) => return Err(io::ErrorKind::TimedOut.into()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        let mut byte = 0u8;
        // SAFETY: one writable byte lives on the stack and the borrowed stream
        // keeps the descriptor open throughout this non-consuming call.
        let count = unsafe {
            libc::recv(
                fd,
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        match count {
            0 => return Ok(FirstByte::Closed),
            1 => return Ok(FirstByte::Byte(byte)),
            _ => {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock if now() < deadline => continue,
                    io::ErrorKind::WouldBlock => return Err(io::ErrorKind::TimedOut.into()),
                    _ => return Err(error),
                }
            }
        }
    }
}

/// Another process already holds a server socket path: its startup lock, a
/// live listener at the path, or a path that appeared while binding. A
/// non-socket path already present during probing is reported as unreachable
/// with `AlreadyExists`, not as busy.
///
/// Busy refusals carry their path in [`BindError::Busy`].
#[derive(Debug)]
pub struct SocketBusy {
    path: PathBuf,
}

/// A private listener could not be bound.
#[derive(Debug)]
pub enum BindError {
    Busy(SocketBusy),
    Io(io::Error),
}

impl BindError {
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Busy(_) => io::ErrorKind::AddrInUse,
            Self::Io(error) => error.kind(),
        }
    }
}

impl From<io::Error> for BindError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Busy(error) => Some(error),
            Self::Io(error) => Some(error),
        }
    }
}

impl SocketBusy {
    fn error(path: &Path) -> BindError {
        BindError::Busy(Self {
            path: path.to_path_buf(),
        })
    }

    /// The socket path another process holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Display for SocketBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "socket busy at {}", self.path.display())
    }
}

impl std::error::Error for SocketBusy {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SocketFileIdentity {
    dev: u64,
    ino: u64,
}

/// Cleanup authority for exactly the inode this process bound. The path and
/// inode identity cannot be accidentally paired with another socket's.
#[derive(Clone, Debug)]
pub struct OwnedSocketFile {
    path: SocketPath,
    identity: SocketFileIdentity,
}
impl OwnedSocketFile {
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }
    pub fn is_still_ours(&self) -> bool {
        socket_file_identity(self.path()).is_ok_and(|identity| identity == self.identity)
    }
    pub fn remove_if_still_ours(&self) -> io::Result<()> {
        remove_socket_file_if_owned(self.path(), &self.identity)
    }
}

/// Listener and cleanup authority acquired together under the lifetime lock.
/// Splitting transfers the listener to its serving thread while the owner
/// keeps cleanup authority and the lock through its final save.
pub struct BoundSocket {
    listener: UnixListener,
    file: OwnedSocketFile,
    lock: SocketStartupLock,
}

/// The pieces of a [`BoundSocket`], handed to their separate owners.
pub struct BoundSocketParts {
    pub listener: UnixListener,
    pub file: OwnedSocketFile,
    pub lock: SocketStartupLock,
}

impl BoundSocket {
    pub fn into_parts(self) -> BoundSocketParts {
        BoundSocketParts {
            listener: self.listener,
            file: self.file,
            lock: self.lock,
        }
    }
    pub fn remove_if_still_ours(self) -> io::Result<()> {
        drop(self.listener);
        self.file.remove_if_still_ours()
    }
}

/// Binds a private listener while holding its profile-selected startup lock.
/// The caller supplies both paths so IPC does not choose a profile filename.
pub fn bind_owned_private_socket(
    path: &SocketPath,
    startup_lock_path: &Path,
) -> Result<BoundSocket, BindError> {
    bind_private_socket(path.as_path(), startup_lock_path)
}

/// What a socket startup lock attempt did, as logged in `ipc.socket_lock`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SocketLockOutcome {
    Acquired,
    Busy,
    Released,
}

impl SocketLockOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Acquired => "acquired",
            Self::Busy => "busy",
            Self::Released => "released",
        }
    }
}

/// An exclusive, nonblocking lock for a server socket's startup and lifetime.
///
/// [`bind_private_socket`] takes it before preparing the path; keep it until
/// the listener has stopped. The regular sidecar file stays beside the socket after the
/// guard drops so later processes always lock the same inode.
pub struct SocketStartupLock {
    _lock: FlockLock,
    socket_path: SocketPath,
}

impl Drop for SocketStartupLock {
    fn drop(&mut self) {
        crate::structured_log!(
            INFO, event = ipc.socket_lock, outcome = SocketLockOutcome::Released.as_str(),
            path = %self.socket_path.as_path().display(),
            "server socket startup lock released"
        );
    }
}

/// An exclusive advisory lock held for the lifetime of this guard.
///
/// The lock file is intentionally left in place after the guard drops. Removing
/// it could let racing processes lock different inodes at the same path.
pub struct FlockLock {
    _file: fs::File,
}

/// Returns a stable lock-file path for `key` inside `lock_dir`.
///
/// The SHA-256 digest keeps arbitrary filesystem paths within one filename
/// component while mapping the same target to the same persistent inode.
pub fn keyed_lock_path(lock_dir: &Path, key: &Path) -> PathBuf {
    let digest = Sha256::digest(key.as_os_str().as_bytes());
    let file_name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    lock_dir.join(format!("{file_name}.lock"))
}

/// What a lock request does when another holder has the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockWait {
    /// Wait until the holder releases the lock.
    UntilFree,
    /// Return `WouldBlock` at once.
    FailIfHeld,
}

/// Opens a private sidecar file and takes an exclusive `flock` on it.
///
/// `wait` selects whether another holder makes this call wait or return
/// `WouldBlock`. The file remains on disk after the returned guard is dropped
/// so callers racing on the same path continue to lock the same inode.
pub fn acquire_flock_lock(lock_path: &Path, wait: LockWait) -> io::Result<FlockLock> {
    if let Some(parent) = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        super::create_private_directory_all(parent)?;
    }

    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(super::limits::PRIVATE_FILE_MODE)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(lock_path)
        .map_err(|error| {
            super::private_file::PrivateFile::normalize_open_error(lock_path, error)
        })?;
    let metadata = file.metadata()?;
    super::private_file::PrivateFile::require_owned_regular(
        lock_path,
        &metadata,
        super::effective_uid(),
    )?;
    file.set_permissions(fs::Permissions::from_mode(super::limits::PRIVATE_FILE_MODE))?;
    flock_exclusive(&file, wait)?;
    Ok(FlockLock { _file: file })
}

/// Takes an exclusive `flock` on `file`, waiting for another holder or
/// returning `WouldBlock` as `wait` says.
pub(crate) fn flock_exclusive(file: &fs::File, wait: LockWait) -> io::Result<()> {
    let mut operation = libc::LOCK_EX;
    if wait == LockWait::FailIfHeld {
        operation |= libc::LOCK_NB;
    }
    loop {
        // SAFETY: flock(2) uses only the open descriptor owned by `file` and
        // does not read or write memory through the call.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

/// Acquire the lifetime lock associated with an absolute `socket_path`.
///
/// The profile path owner supplies the persistent sidecar path; taking it as
/// input keeps this IPC layer independent of the profile's file layout.
fn acquire_socket_startup_lock(
    socket_path: &Path,
    startup_lock_path: &Path,
) -> Result<SocketStartupLock, BindError> {
    let checked_socket_path = SocketPath::new(socket_path.to_path_buf())?;
    socket_parent(socket_path)?;
    let lock = match acquire_flock_lock(startup_lock_path, LockWait::FailIfHeld) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            crate::structured_log!(
                INFO, event = ipc.socket_lock, outcome = SocketLockOutcome::Busy.as_str(),
                path = %socket_path.display(),
                "server socket startup lock is already held"
            );
            return Err(SocketBusy::error(socket_path));
        }
        Err(error) => return Err(error.into()),
    };
    crate::structured_log!(
        INFO, event = ipc.socket_lock, outcome = SocketLockOutcome::Acquired.as_str(),
        path = %socket_path.display(),
        "server socket startup lock acquired"
    );
    Ok(SocketStartupLock {
        _lock: lock,
        socket_path: checked_socket_path,
    })
}

/// Acquires the startup lock, prepares an absolute `path`, and binds a private listener.
///
/// The lock stays alive in the returned tuple and must be held until the
/// listener has stopped and its socket file has been removed. Keeping these
/// steps together prevents a caller from reclaiming a stale socket before it
/// owns the lock, which could unlink a socket another server is about to use.
/// A busy path is returned as a [`SocketBusy`] error naming it; the caller
/// chooses any operator-facing wording.
fn bind_private_socket(path: &Path, startup_lock_path: &Path) -> Result<BoundSocket, BindError> {
    let startup_lock = acquire_socket_startup_lock(path, startup_lock_path)?;
    bind_private_socket_with_lock(startup_lock)
}

/// Binds the path a held startup lock reserves, keeping the lock. The path
/// comes from the guard, so a lock for another socket cannot be used.
fn bind_private_socket_with_lock(
    startup_lock: SocketStartupLock,
) -> Result<BoundSocket, BindError> {
    let path = startup_lock.socket_path.as_path();
    prepare_socket_path(path)?;
    let listener = bind_private_local_listener(path)?;
    let identity = socket_file_identity(path)?;
    Ok(BoundSocket {
        listener,
        file: OwnedSocketFile {
            path: startup_lock.socket_path.clone(),
            identity,
        },
        lock: startup_lock,
    })
}

/// Connects to a local socket, giving up with `TimedOut` after
/// `LOCAL_CONNECT_TIMEOUT` (see [`connect_local_stream_within`]).
pub fn connect_local_stream(path: &Path) -> io::Result<LocalStream> {
    connect_local_stream_within(path, super::limits::LOCAL_CONNECT_TIMEOUT)
}

/// Connects to the socket at `path`, waiting at most `timeout`.
///
/// A blocking `connect` to a Unix socket whose listen backlog is full waits
/// until the listener accepts, which a wedged server never does. The connect
/// here is nonblocking: a full backlog answers `EAGAIN`, which is retried until
/// the deadline and then reported as `TimedOut`. Every other error is the
/// connect's own (`NotFound`, `ConnectionRefused`, `PermissionDenied`), so
/// callers classify them as they would a blocking connect's. A timeout too
/// large to form an `Instant` deadline is `InvalidInput`.
pub fn connect_local_stream_within(path: &Path, timeout: Duration) -> io::Result<LocalStream> {
    use std::os::fd::{FromRawFd as _, OwnedFd};
    use std::os::unix::net::UnixStream;

    // clock-io-ok: the checked deadline bounds a real socket connect.
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "local socket connect timeout is too large",
        )
    })?;

    let socket_path = SocketPath::new(path.to_path_buf())?;
    let bytes = socket_path.as_path().as_os_str().as_bytes();
    // SAFETY: `sockaddr_un` is plain data for which all-zero bytes is a valid
    // (empty) value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX)
        .map_err(|_| io::Error::other("AF_UNIX does not fit sa_family_t"))?;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = libc::c_char::from_ne_bytes([*byte]);
    }
    let address_len = libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>())
        .map_err(|_| io::Error::other("sockaddr_un does not fit socklen_t"))?;

    // SAFETY: socket(2) takes no pointers; the returned descriptor is checked
    // and immediately owned.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };

    loop {
        // SAFETY: `address` is a valid `sockaddr_un` of `address_len` bytes and
        // `socket` stays open for the call.
        let result = unsafe {
            libc::connect(
                socket.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                address_len,
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => {
                // clock-io-ok: same real deadline as above.
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out connecting to {}: the server is not accepting connections",
                            path.display()
                        ),
                    ));
                }
                std::thread::sleep(super::limits::LOCAL_CONNECT_RETRY_INTERVAL);
            }
            _ => return Err(error),
        }
    }

    super::set_fd_nonblocking(socket.as_raw_fd(), false)?;
    Ok(UnixStream::from(socket))
}

/// Connects to a server socket and checks its owner before returning the
/// stream, so no request or attach byte is ever written to a socket served by
/// another effective uid.
///
/// It checks an exact effective-uid match with
/// [`peer_is_same_effective_user`]. The server's accept-side check separately
/// allows root. The peer of a stream that connected is a listener, so its
/// credentials are the ones the listening process had when it bound the
/// socket. A foreign or unverifiable owner is a `PermissionDenied` error that
/// names the socket; every connect error of [`connect_local_stream`] is passed
/// through unchanged.
pub fn connect_trusted_local_stream(path: &Path) -> io::Result<TrustedServerStream> {
    connect_trusted_local_stream_within(path, super::limits::LOCAL_CONNECT_TIMEOUT)
}

/// [`connect_trusted_local_stream`] with the connect bounded by `timeout`
/// (see [`connect_local_stream_within`]).
pub fn connect_trusted_local_stream_within(
    path: &Path,
    timeout: Duration,
) -> io::Result<TrustedServerStream> {
    let stream = connect_local_stream_within(path, timeout)?;
    match peer_is_same_effective_user(&stream) {
        Ok(true) => Ok(TrustedServerStream(stream)),
        Ok(false) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the server socket {} is served by another user; refusing to send it anything",
                path.display()
            ),
        )),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "could not verify who serves the socket {}: {error}",
                path.display()
            ),
        )),
    }
}

pub fn bind_local_listener(path: &Path) -> io::Result<UnixListener> {
    // The caller owns socket path cleanup; dropping the listener only closes its fd.
    UnixListener::bind(path)
}

/// Probe a local server socket and classify whether it is absent, stale, live,
/// or present but unreachable. Timeouts and access failures stay errors because
/// they do not prove that no server is listening. Anything at the path that is
/// not a socket is unreachable, never stale: `connect` to a regular file also
/// fails with `ECONNREFUSED`, and a stale path is one callers delete.
pub fn probe(path: &Path) -> Liveness {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Liveness::Absent,
        Err(error) => return Liveness::Unreachable(error),
        Ok(metadata) if !metadata.file_type().is_socket() => {
            return Liveness::Unreachable(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
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

/// Readies `path` for binding: creates its parent, removes a stale socket
/// where nothing listens, and refuses a live one. Only [`bind_private_socket`]
/// calls it, under the startup lock, so a stale socket is never reclaimed by a
/// caller that does not own the path.
fn prepare_socket_path(path: &Path) -> Result<(), BindError> {
    if let Some(parent) = path.parent() {
        super::create_private_directory_all(parent)?;
    }

    match probe(path) {
        Liveness::Absent => return Ok(()),
        Liveness::Live => return Err(SocketBusy::error(path)),
        Liveness::Stale => {}
        Liveness::Unreachable(error) => return Err(error.into()),
    }

    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error.into());
    }

    Ok(())
}

/// Mode applied by [`bind_private_local_listener`]: owner read/write only.
const PRIVATE_SOCKET_MODE: u32 = super::limits::PRIVATE_FILE_MODE;

/// Binds a listener at an absolute `path` so the socket is never reachable with anything
/// looser than owner-only permissions.
///
/// Binding at `path` and then chmodding leaves the socket connectable with
/// umask-derived permissions in between. Instead the socket is bound inside a
/// fresh private staging directory next to `path`, given owner-only socket
/// permissions, and then hard-linked into place. `link` fails if `path` already
/// exists, so a listener that raced us to the path is never replaced (a `bind` at the path
/// would have failed the same way); that is reported as [`SocketBusy`]. The
/// listener is bound to the inode, so connections through the new name reach
/// it, and a socket identity recorded from `path` afterwards is that inode.
///
/// If staging cannot be used (a staged path over the socket path length
/// limit, or a filesystem without hard links), this falls back to
/// bind-then-chmod at `path`, which still ends owner-only. Callers may tighten
/// or re-apply the mode afterwards. Access is also checked per connection by
/// [`accept_peer`] with [`PeerAdmission::OwnerOrRoot`]; the file mode is not
/// the only control.
pub fn bind_private_local_listener(path: &Path) -> Result<UnixListener, BindError> {
    let parent = socket_parent(path)?;
    match bind_via_private_staging(path, parent) {
        Ok(listener) => {
            crate::structured_log!(
                INFO, event = ipc.socket_bind, outcome = "ok",
                strategy = "staged",
                path = %path.display(),
                "private socket listener bound"
            );
            Ok(listener)
        }
        Err(StagedBindError::Busy) => Err(SocketBusy::error(path)),
        Err(StagedBindError::RandomSource(error)) => Err(error.into()),
        Err(StagedBindError::Unavailable(err)) => {
            crate::structured_log!(
                WARN, event = ipc.socket_bind, outcome = "staging_unavailable",
                path = %path.display(),
                error = %err,
                "private socket staging failed; binding in place"
            );
            let listener = bind_in_place_then_restrict(path)?;
            crate::structured_log!(
                INFO, event = ipc.socket_bind, outcome = "ok",
                strategy = "in_place",
                path = %path.display(),
                "private socket listener bound"
            );
            Ok(listener)
        }
    }
}

fn bind_in_place_then_restrict(path: &Path) -> Result<UnixListener, BindError> {
    let listener = bind_local_listener(path).map_err(|error| {
        if error.kind() == io::ErrorKind::AddrInUse {
            SocketBusy::error(path)
        } else {
            BindError::Io(error)
        }
    })?;
    if let Err(error) = restrict_socket_permissions(path, PRIVATE_SOCKET_MODE) {
        drop(listener);
        // The restrict error is what the caller acts on; a socket left behind
        // with the wrong mode is still worth an operator's attention.
        if let Err(remove_error) = fs::remove_file(path) {
            crate::structured_log!(
                WARN, event = ipc.socket_remove, outcome = "error",
                path = %path.display(),
                error = %remove_error,
                "failed to remove socket after restricting its mode failed"
            );
        }
        return Err(error.into());
    }
    Ok(listener)
}

enum StagedBindError {
    /// Something already exists at the target path.
    Busy,
    /// No private staging name can be created without kernel randomness.
    RandomSource(io::Error),
    /// Staging itself failed; binding in place may still work.
    Unavailable(io::Error),
}

fn socket_parent(path: &Path) -> io::Result<&Path> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path must be absolute",
        ));
    }
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path must name a file inside a directory",
            )
        })
}

fn bind_via_private_staging(path: &Path, parent: &Path) -> Result<UnixListener, StagedBindError> {
    use super::owned_runtime::{DirectoryKind, OwnedRuntimeEntry, RuntimeCreateError};
    let entry =
        OwnedRuntimeEntry::create_directory(parent, DirectoryKind::STAGING).map_err(|error| {
            match error {
                RuntimeCreateError::RandomSource(error) => StagedBindError::RandomSource(error),
                RuntimeCreateError::Io(error) => StagedBindError::Unavailable(error),
            }
        })?;
    let staged = DirectoryKind::STAGING.content_path(entry.path());
    let result = bind_staged_and_link(&staged, path);
    entry.release();
    result
}

fn bind_staged_and_link(staged: &Path, path: &Path) -> Result<UnixListener, StagedBindError> {
    let listener = bind_local_listener(staged).map_err(StagedBindError::Unavailable)?;
    restrict_socket_permissions(staged, PRIVATE_SOCKET_MODE)
        .map_err(StagedBindError::Unavailable)?;
    match fs::hard_link(staged, path) {
        Ok(()) => Ok(listener),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(StagedBindError::Busy),
        Err(err) => Err(StagedBindError::Unavailable(err)),
    }
}

/// Reports whether a local peer has exactly this process's effective uid.
/// Clients use this when deciding whether to trust a server at a socket path.
pub fn peer_is_same_effective_user(stream: &LocalStream) -> io::Result<bool> {
    Ok(peer_uid_is_same_effective_user(
        peer_uid(stream)?,
        super::effective_uid(),
    ))
}

fn peer_uid_is_allowed_client(peer_uid: libc::uid_t, own_uid: libc::uid_t) -> bool {
    peer_uid == own_uid || peer_uid == 0
}

fn peer_uid_is_same_effective_user(peer_uid: libc::uid_t, own_uid: libc::uid_t) -> bool {
    peer_uid == own_uid
}

fn peer_uid(stream: &LocalStream) -> io::Result<libc::uid_t> {
    use std::os::fd::AsRawFd;
    Ok(peer_credentials(stream.as_raw_fd())?.uid)
}

fn peer_credentials(fd: std::os::fd::RawFd) -> io::Result<libc::ucred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`size_of::<ucred>()` is a small compile-time constant, well within `socklen_t` (u32) range, so this cast never truncates"
    )]
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
    Ok(cred)
}

/// A local-socket adapter for the shared fd readiness deadline reader.
pub struct LocalStreamDeadlineReader<'a> {
    inner: super::child_io::DeadlineReader<LocalStreamReader<'a>>,
}

impl<'a> LocalStreamDeadlineReader<'a> {
    pub fn new(stream: &'a mut LocalStream, deadline: Instant) -> Self {
        // clock-io-ok: the public entry point supplies the real clock.
        Self::new_with_clock(stream, deadline, std::sync::Arc::new(Instant::now))
    }

    fn new_with_clock(
        stream: &'a mut LocalStream,
        deadline: Instant,
        now: std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self {
            inner: super::child_io::DeadlineReader::new_with_clock(
                LocalStreamReader { stream },
                deadline,
                now,
            ),
        }
    }
}

impl Read for LocalStreamDeadlineReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}

struct LocalStreamReader<'a> {
    stream: &'a mut LocalStream,
}

impl Read for LocalStreamReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buffer)
    }
}

impl AsRawFd for LocalStreamReader<'_> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.stream.as_raw_fd()
    }
}

/// One nonblocking read that preserves the number of bytes read.
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

/// The transport meaning of an error reported while using a local stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFailure {
    /// The stream was connected, but its peer has closed or disappeared.
    PeerGone,
    /// The stream path is absent or has no listener to answer a connection attempt.
    NoListener,
    /// The operation timed out or is not ready to proceed yet.
    TimedOut,
    /// The error does not establish a stream transport outcome.
    Other,
}

/// Gives local stream errors one transport meaning for all consumers.
/// This answers whether a listener or connected peer left, not whether an SSH
/// host is reachable or whether a launcher should retry its operation.
pub fn classify_stream_error(kind: io::ErrorKind) -> StreamFailure {
    match kind {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::NotConnected
        | io::ErrorKind::UnexpectedEof
        | io::ErrorKind::WriteZero => StreamFailure::PeerGone,
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound => StreamFailure::NoListener,
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => StreamFailure::TimedOut,
        _ => StreamFailure::Other,
    }
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
    use std::time::Duration;

    fn startup_lock_path_for_test(socket_path: &Path) -> PathBuf {
        socket_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("startup-lock")
    }

    fn reserve_test_socket(socket_path: &Path) -> Result<SocketStartupLock, BindError> {
        let lock_path = startup_lock_path_for_test(socket_path);
        acquire_socket_startup_lock(socket_path, &lock_path)
    }

    fn bind_test_socket(socket_path: &Path) -> Result<BoundSocket, BindError> {
        let lock_path = startup_lock_path_for_test(socket_path);
        bind_private_socket(socket_path, &lock_path)
    }
    #[test]
    fn accept_failures_keep_transient_and_unknown_errors_retryable() {
        for errno in [
            libc::EINTR,
            libc::ECONNABORTED,
            libc::EPROTO,
            libc::EPERM,
            libc::EAGAIN,
        ] {
            assert!(matches!(
                classify_accept_failure(io::Error::from_raw_os_error(errno)),
                Accepted::RetryNow
            ));
        }
        for errno in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::EIO,
        ] {
            assert!(matches!(
                classify_accept_failure(io::Error::from_raw_os_error(errno)),
                Accepted::Backoff(_)
            ));
        }
        for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(matches!(
                classify_accept_failure(io::Error::from_raw_os_error(errno)),
                Accepted::Fatal(_)
            ));
        }
    }

    #[test]
    fn bind_private_socket_is_owner_only_from_the_moment_it_is_reachable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = shepr_test_support::ScratchDir::new("hb");
        let path = dir.join("server.sock");

        let BoundSocketParts {
            listener,
            lock: startup_lock,
            ..
        } = bind_test_socket(&path).expect("bind").into_parts();
        let mode = fs::metadata(&path)
            .expect("socket exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, super::PRIVATE_SOCKET_MODE);
        // The staging directory is gone; only the socket and persistent lock remain.
        let entries = fs::read_dir(&dir)
            .expect("test precondition")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<std::collections::BTreeSet<_>>();
        let expected_entries = ["server.sock", "startup-lock"]
            .map(std::ffi::OsString::from)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(entries, expected_entries);
        // The linked name reaches the listener.
        assert!(connect_local_stream(&path).is_ok());
        assert!(listener.accept().is_ok());
        // A second server never replaces a socket that is already there.
        let err = bind_test_socket(&path).err().expect("startup lock is held");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);

        drop(listener);
        drop(startup_lock);
    }

    #[test]
    fn peek_first_byte_returns_the_byte_without_consuming_it() {
        use std::io::Write;
        let (mut peer, mut stream) = LocalStream::pair().expect("pair");
        peer.write_all(b"Shepr").expect("bytes");
        let now = Instant::now();
        assert_eq!(
            peek_first_byte_with_clock(&stream, now, &|| now).expect("peek"),
            FirstByte::Byte(b'S')
        );
        let mut bytes = [0; 5];
        stream.read_exact(&mut bytes).expect("unconsumed");
        assert_eq!(&bytes, b"Shepr");
    }

    #[test]
    fn peek_first_byte_reports_a_peer_that_closed_silently() {
        let (peer, stream) = LocalStream::pair().expect("pair");
        drop(peer);
        let now = Instant::now();
        assert_eq!(
            peek_first_byte_with_clock(&stream, now, &|| now).expect("peek"),
            FirstByte::Closed
        );
    }

    #[test]
    fn peek_first_byte_times_out_on_a_silent_peer() {
        let (_peer, stream) = LocalStream::pair().expect("pair");
        let now = Instant::now();
        assert_eq!(
            peek_first_byte_with_clock(&stream, now, &|| now)
                .expect_err("silent")
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn peek_first_byte_with_a_passed_deadline_polls_once() {
        use std::io::Write;
        let (mut peer, stream) = LocalStream::pair().expect("pair");
        let now = Instant::now();
        let deadline = now - Duration::from_secs(1);
        assert_eq!(
            peek_first_byte_with_clock(&stream, deadline, &|| now)
                .expect_err("silent")
                .kind(),
            io::ErrorKind::TimedOut
        );
        peer.write_all(b"{").expect("byte");
        assert_eq!(
            peek_first_byte_with_clock(&stream, deadline, &|| now).expect("ready"),
            FirstByte::Byte(b'{')
        );
    }

    #[test]
    fn accept_failures_are_classified_so_descriptor_pressure_never_stops_the_server() {
        for errno in [libc::ECONNABORTED, libc::EPROTO, libc::EPERM, libc::EINTR] {
            assert!(accept_failed_for_one_connection(
                &io::Error::from_raw_os_error(errno)
            ));
        }
        for errno in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::EBADF,
            libc::EINVAL,
            libc::ENOTSOCK,
        ] {
            assert!(!accept_failed_for_one_connection(
                &io::Error::from_raw_os_error(errno)
            ));
        }
    }

    #[test]
    fn socket_is_live_maps_absent_and_stale_to_false_and_unreachable_to_an_error() {
        let scratch = shepr_test_support::ScratchDir::new("socket-liveness");
        let socket = scratch.join("server.sock");
        assert!(!socket_is_live(&socket).expect("absent"));
        let listener = UnixListener::bind(&socket).expect("bind");
        assert!(socket_is_live(&socket).expect("live"));
        drop(listener);
        assert!(!socket_is_live(&socket).expect("stale"));
        let file = scratch.join("regular");
        fs::write(&file, b"file").expect("file");
        assert_eq!(
            socket_is_live(&file).expect_err("not a socket").kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn local_socket_connect_rejects_a_timeout_that_overflows_instant() {
        let error = connect_local_stream_within(Path::new("unused.sock"), Duration::MAX)
            .expect_err("an unrepresentable deadline is invalid input");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("timeout is too large"));
    }

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

    /// A regular file at a socket path refuses `connect` the way a stale
    /// socket does; it must read as unreachable so no binder deletes it.
    #[test]
    fn a_regular_file_at_the_socket_path_is_unreachable_and_survives_a_bind() {
        let dir = shepr_test_support::ScratchDir::new("probe-regular-file");
        let path = dir.join("server.sock");
        fs::write(&path, b"not a socket").expect("write regular file");

        assert!(matches!(probe(&path), Liveness::Unreachable(_)));
        let Err(error) = bind_test_socket(&path) else {
            panic!("a regular file at the socket path was bound over");
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&path).expect("regular file is kept"),
            b"not a socket"
        );
    }

    /// Binding takes the startup lock before touching the path: a stale socket
    /// is reclaimed, a live one it does not own is refused with `AddrInUse`,
    /// and a second binder is refused while the first holds the lock.
    #[test]
    fn bind_private_socket_reclaims_stale_refuses_live_and_holds_its_lock() {
        let dir = shepr_test_support::ScratchDir::new("bind-private-socket");

        let stale = dir.join("stale.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&stale).expect("bind stale");
        }
        let BoundSocketParts { listener, lock, .. } =
            bind_test_socket(&stale).expect("reclaim").into_parts();
        let second = bind_test_socket(&stale)
            .err()
            .expect("the first binder holds the startup lock");
        assert_busy_at(&second, &stale);
        drop(listener);
        drop(lock);

        let live = dir.join("live.sock");
        let _foreign = std::os::unix::net::UnixListener::bind(&live).expect("bind live");
        let refused = bind_test_socket(&live)
            .err()
            .expect("a live socket is never replaced");
        assert_busy_at(&refused, &live);
    }

    #[test]
    fn held_socket_reservation_binds_without_releasing_its_lock() {
        let scratch = shepr_test_support::ScratchDir::new("socket-reservation-handoff");
        let path = scratch.join("client.sock");
        let reservation = reserve_test_socket(&path).expect("reserve socket");
        assert!(!path.try_exists().expect("unpublished socket"));
        let before = fs::symlink_metadata(startup_lock_path_for_test(&path)).expect("lock inode");
        let BoundSocketParts {
            listener,
            file,
            lock,
        } = bind_private_socket_with_lock(reservation)
            .expect("bind reserved socket")
            .into_parts();
        let after =
            fs::symlink_metadata(startup_lock_path_for_test(&path)).expect("lock inode after bind");
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_busy_at(
            &reserve_test_socket(&path).err().expect("lock still held"),
            &path,
        );
        drop(listener);
        file.remove_if_still_ours().expect("cleanup socket");
        drop(lock);
        let _next = reserve_test_socket(&path).expect("lock released at teardown");
    }

    #[test]
    fn reserved_socket_bind_refuses_a_listener_that_ignores_the_lock() {
        let scratch = shepr_test_support::ScratchDir::new("socket-reservation-race");
        let path = scratch.join("client.sock");
        let reservation = reserve_test_socket(&path).expect("reserve socket");
        let _racer = std::os::unix::net::UnixListener::bind(&path).expect("uncooperative listener");
        let identity = socket_file_identity(&path).expect("racer identity");
        let error = bind_private_socket_with_lock(reservation)
            .err()
            .expect("live listener refused");
        assert_busy_at(&error, &path);
        assert_eq!(
            socket_file_identity(&path).expect("racer retained"),
            identity
        );
    }

    /// Every busy refusal is `AddrInUse` and names the path it refused.
    fn assert_busy_at(error: &BindError, path: &Path) {
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        let BindError::Busy(busy) = error else {
            panic!("expected busy refusal: {error}")
        };
        assert_eq!(busy.path(), path);
        assert_eq!(
            error.to_string(),
            format!("socket busy at {}", path.display())
        );
    }

    /// A socket path in a fresh scratch directory.
    fn test_socket_path(name: &str) -> std::path::PathBuf {
        shepr_test_support::ScratchDir::new(name).join("s.sock")
    }

    #[test]
    fn flock_lock_is_private_persistent_and_can_be_nonblocking() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = shepr_test_support::ScratchDir::new("flock-lock");
        let lock_path = dir.join("locks/resource.lock");
        let lock =
            acquire_flock_lock(&lock_path, LockWait::UntilFree).expect("acquire blocking lock");
        let metadata = fs::metadata(&lock_path).expect("lock file exists");
        assert!(metadata.is_file());
        assert_eq!(
            metadata.permissions().mode() & crate::limits::PERMISSION_BITS,
            crate::limits::PRIVATE_FILE_MODE
        );
        assert_eq!(
            metadata.uid(),
            fs::metadata(dir.path())
                .expect("scratch directory exists")
                .uid()
        );
        let first_inode = metadata.ino();

        let error = match acquire_flock_lock(&lock_path, LockWait::FailIfHeld) {
            Ok(_) => panic!("second nonblocking lock unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(lock);

        let _lock =
            acquire_flock_lock(&lock_path, LockWait::UntilFree).expect("reacquire blocking lock");
        assert_eq!(
            fs::metadata(&lock_path).expect("lock file persists").ino(),
            first_inode
        );
    }

    #[test]
    fn private_listener_socket_is_owner_only() {
        let path = test_socket_path("private");
        let listener = bind_private_local_listener(&path).expect("test precondition");
        let mode = fs::metadata(&path).expect("test precondition").mode() & 0o777;
        drop(listener);
        fs::remove_file(&path).expect("remove the bound socket");
        assert_eq!(mode, PRIVATE_SOCKET_MODE);
    }

    #[test]
    fn socket_binding_rejects_relative_paths() {
        for path in [Path::new("socket.sock"), Path::new("runtime/socket.sock")] {
            let error = match bind_private_local_listener(path) {
                Ok(_) => panic!("relative socket path unexpectedly bound"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

            let error = match reserve_test_socket(path) {
                Ok(_) => panic!("relative socket path unexpectedly locked"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

            let error = match bind_test_socket(path) {
                Ok(_) => panic!("relative socket path unexpectedly bound"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn private_listener_is_linked_into_place_and_never_replaces_a_path() {
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
        assert_busy_at(&err, &path);
        drop(listener);

        let plain = dir.join("plain");
        fs::write(&plain, b"keep").expect("test precondition");
        let err = bind_private_local_listener(&plain).expect_err("path is taken");
        assert_busy_at(&err, &plain);
        assert_eq!(fs::read(&plain).expect("file kept"), b"keep");

        fs::remove_dir_all(&dir).expect("remove the socket directory");
    }

    #[test]
    fn peer_credentials_admit_this_user() {
        let (client, server) = connected_pair("peercred");
        assert!(peer_is_same_effective_user(&server).expect("SO_PEERCRED"));
        assert!(peer_is_same_effective_user(&client).expect("SO_PEERCRED"));
    }

    #[test]
    fn trusted_server_requires_an_exact_uid_while_accept_side_keeps_root() {
        assert!(peer_uid_is_same_effective_user(1000, 1000));
        assert!(!peer_uid_is_same_effective_user(0, 1000));
        assert!(peer_uid_is_allowed_client(0, 1000));
    }

    #[test]
    fn trusted_connect_admits_a_socket_served_by_this_user() {
        let path = test_socket_path("trusted-connect");
        let _listener = bind_local_listener(&path).expect("test precondition");
        connect_trusted_local_stream(&path).expect("a same-user server is trusted");
    }

    #[test]
    fn trusted_connect_passes_connect_errors_through() {
        let path = test_socket_path("trusted-connect-absent");
        let Err(error) = connect_trusted_local_stream(&path) else {
            panic!("nothing listens at the path");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn nonblocking_listener_keeps_accepted_streams_blocking_and_leaves_path_on_drop() {
        let scratch = shepr_test_support::ScratchDir::new("listener-lifetime");
        let path = scratch.join("s.sock");
        let listener = bind_private_local_listener(&path).expect("bind private listener");
        listener
            .set_nonblocking(true)
            .expect("set accept nonblocking");
        let error = listener.accept().expect_err("empty backlog would block");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        let client = connect_trusted_local_stream(&path).expect("connect trusted client");
        let (server, _) = listener.accept().expect("accept client");
        assert!(peer_is_same_effective_user(&server).expect("read peer credentials"));
        // SAFETY: F_GETFL only reads flags from the live accepted descriptor.
        let flags = unsafe { libc::fcntl(server.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "read accepted stream flags");
        assert_eq!(flags & libc::O_NONBLOCK, 0);
        drop(server);
        drop(client);
        drop(listener);
        let metadata = fs::symlink_metadata(&path).expect("listener drop retains its socket path");
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, PRIVATE_SOCKET_MODE);
        assert!(matches!(probe(&path), Liveness::Stale));
    }

    fn connected_pair(name: &str) -> (LocalStream, LocalStream) {
        let path = test_socket_path(name);
        let listener = bind_local_listener(&path).expect("test precondition");
        let client = connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition").0;
        fs::remove_file(&path).expect("remove the bound socket");
        (client, server)
    }

    #[test]
    fn deadline_reader_cuts_off_a_peer_at_the_overall_deadline() {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let path = test_socket_path("deadline");
        let listener = bind_local_listener(&path).expect("test precondition");
        let mut client = connect_local_stream(&path).expect("test precondition");
        let mut server = listener.accept().expect("test precondition").0;
        fs::remove_file(&path).expect("remove the bound socket");
        client.write_all(b"x").expect("test precondition");
        let started = Instant::now();
        let deadline = started + Duration::from_millis(300);
        let clock_reads = std::sync::Arc::new(AtomicUsize::new(0));
        let clock_reads_for_reader = std::sync::Arc::clone(&clock_reads);
        let mut buf = [0u8; 2];
        let result = LocalStreamDeadlineReader::new_with_clock(
            &mut server,
            deadline,
            std::sync::Arc::new(move || {
                if clock_reads_for_reader.fetch_add(1, Ordering::Relaxed) == 0 {
                    started
                } else {
                    deadline
                }
            }),
        )
        .read_exact(&mut buf);

        let error = result.expect_err("a trickling peer must not complete the read");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(buf[0], b'x');
        assert_eq!(clock_reads.load(Ordering::Relaxed), 2);
    }
}
