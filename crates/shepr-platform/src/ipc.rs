use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

pub type LocalListener = std::os::unix::net::UnixListener;
pub type LocalStream = std::os::unix::net::UnixStream;

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

/// The common lifetime contract for the two server endpoints. Startup takes
/// the data lease, binds the API, reserves the client path, restores panes,
/// then binds the client listener. Shutdown retains endpoints through the
/// final save, retires the lease, removes the API, then removes the client.
/// Socket absence alone permits a launch attempt; the lease remains the
/// authority that prevents two owners, including before either socket binds.
pub struct ServerLifetime;

/// Resources reserved before restoring panes. Field order also retires the
/// lease before the API when an unused reservation is dropped.
pub struct ReservedServer<L, A> {
    lease: L,
    api: A,
    client: SocketStartupLock,
}

impl ServerLifetime {
    pub fn reserve<L, A, E>(
        acquire_lease: impl FnOnce() -> Result<L, E>,
        bind_api: impl FnOnce(&L) -> Result<A, E>,
        reserve_client: impl FnOnce() -> Result<SocketStartupLock, E>,
    ) -> Result<ReservedServer<L, A>, E> {
        Self::in_resource_order(
            acquire_lease,
            |lease| lease.and_then(|lease| bind_api(&lease).map(|api| (lease, api))),
            |resources| {
                resources.and_then(|(lease, api)| match reserve_client() {
                    Ok(client) => Ok(ReservedServer { lease, api, client }),
                    Err(error) => {
                        Self::release(
                            &mut (Some(lease), Some(api)),
                            |resources| drop(resources.0.take()),
                            |resources| drop(resources.1.take()),
                            |_| {},
                        );
                        Err(error)
                    }
                })
            },
        )
    }

    pub fn release<C>(
        context: &mut C,
        retire_lease: impl FnOnce(&mut C),
        remove_api: impl FnOnce(&mut C),
        remove_client: impl FnOnce(&mut C),
    ) {
        Self::in_resource_order(
            || {
                retire_lease(context);
                context
            },
            |context| {
                remove_api(context);
                context
            },
            remove_client,
        );
    }

    // One ordering primitive drives both acquisition and release. Passing the
    // preceding result to the next step prevents independently scheduled steps.
    fn in_resource_order<L, A, C>(
        lease: impl FnOnce() -> L,
        api: impl FnOnce(L) -> A,
        client: impl FnOnce(A) -> C,
    ) -> C {
        client(api(lease()))
    }

    /// An inaccessible endpoint never proves absence or permits a successor.
    pub fn endpoint_is_live(path: &Path) -> io::Result<bool> {
        Self::liveness(probe(path))
    }

    pub fn liveness(value: Liveness) -> io::Result<bool> {
        match value {
            Liveness::Absent | Liveness::Stale => Ok(false),
            Liveness::Live => Ok(true),
            Liveness::Unreachable(error) => Err(error),
        }
    }

    pub fn observe<T>(
        api_live: bool,
        client_live: bool,
        status: Option<(T, bool)>,
    ) -> ServerPresence<T> {
        match (api_live, client_live) {
            (false, false) => ServerPresence::Gone,
            (true, false) => ServerPresence::Starting,
            (false, true) => ServerPresence::Releasing,
            (true, true) => match status {
                Some((identity, true)) => ServerPresence::Stopping(identity),
                Some((identity, false)) => ServerPresence::Running(identity),
                None => ServerPresence::Unresponsive,
            },
        }
    }
}

impl<L, A> ReservedServer<L, A> {
    pub fn restore_and_bind<T, S, E>(
        self,
        restore: impl FnOnce(L) -> T,
        bind_client: impl FnOnce(T, A, SocketStartupLock) -> Result<S, E>,
    ) -> Result<S, E> {
        let app = restore(self.lease);
        bind_client(app, self.api, self.client)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPresence<T> {
    Gone,
    Starting,
    Running(T),
    Stopping(T),
    Releasing,
    Unresponsive,
}

/// Another process already holds a server socket path: its startup lock, a
/// live listener at the path, or a path that appeared while binding. A
/// non-socket path already present during probing is reported as unreachable
/// with `AlreadyExists`, not as busy.
///
/// Every refusal classified as busy is an [`io::ErrorKind::AddrInUse`] error
/// carrying this payload, so the path survives whichever caller sees it and
/// nobody gets a bare "address in use". Callers that word the refusal
/// themselves find it with [`SocketBusy::from_io`].
#[derive(Debug)]
pub struct SocketBusy {
    path: PathBuf,
}

impl SocketBusy {
    fn error(path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::AddrInUse,
            Self {
                path: path.to_path_buf(),
            },
        )
    }

    /// The busy refusal inside `error`, if it is one.
    pub fn from_io(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref::<Self>()
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

/// An exclusive, nonblocking lock for a server socket's startup and lifetime.
///
/// [`bind_private_socket`] takes it before preparing the path; keep it until
/// the listener has stopped. The regular sidecar file stays beside the socket after the
/// guard drops so later processes always lock the same inode. The exception
/// is [`bind_single_use_private_socket`], whose owner removes the sidecar.
pub struct SocketStartupLock {
    _lock: FlockLock,
    socket_path: PathBuf,
}

impl SocketStartupLock {
    /// The socket path this lock reserves.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for SocketStartupLock {
    fn drop(&mut self) {
        tracing::info!(
            event = "ipc.socket_lock",
            subsystem = "ipc",
            outcome = "released",
            path = %self.socket_path.display(),
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

/// Opens a private sidecar file and takes an exclusive `flock` on it.
///
/// `blocking` selects whether another holder makes this call wait or return
/// `WouldBlock`. The file remains on disk after the returned guard is dropped
/// so callers racing on the same path continue to lock the same inode.
pub fn acquire_flock_lock(lock_path: &Path, blocking: bool) -> io::Result<FlockLock> {
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
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(lock_path)?;
    let metadata = file.metadata()?;
    let expected_uid = super::effective_uid();
    if !metadata.is_file() || metadata.uid() != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "lock file {} must be a regular file owned by uid {expected_uid}; found a {} owned by uid {}",
                lock_path.display(),
                if metadata.is_file() {
                    "regular file"
                } else {
                    "non-regular file"
                },
                metadata.uid(),
            ),
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    flock_exclusive(&file, blocking)?;
    Ok(FlockLock { _file: file })
}

/// Takes an exclusive `flock` on `file`, waiting for another holder when
/// `blocking` and returning `WouldBlock` otherwise.
pub(crate) fn flock_exclusive(file: &fs::File, blocking: bool) -> io::Result<()> {
    let mut operation = libc::LOCK_EX;
    if !blocking {
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
/// The lock file has `.lock` appended to the socket path, so it shares the
/// socket's parent directory without counting against the socket path limit
/// (`shepr_core::socket_path`).
pub fn acquire_socket_startup_lock(socket_path: &Path) -> io::Result<SocketStartupLock> {
    socket_parent(socket_path)?;
    let lock_path = socket_startup_lock_path(socket_path);
    let lock = match acquire_flock_lock(&lock_path, false) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            tracing::info!(
                event = "ipc.socket_lock",
                subsystem = "ipc",
                outcome = "busy",
                path = %socket_path.display(),
                "server socket startup lock is already held"
            );
            return Err(SocketBusy::error(socket_path));
        }
        Err(error) => return Err(error),
    };
    tracing::info!(
        event = "ipc.socket_lock",
        subsystem = "ipc",
        outcome = "acquired",
        path = %socket_path.display(),
        "server socket startup lock acquired"
    );
    Ok(SocketStartupLock {
        _lock: lock,
        socket_path: socket_path.to_path_buf(),
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
pub fn bind_private_socket(
    path: &Path,
) -> io::Result<(LocalListener, SocketStartupLock, SocketFileIdentity)> {
    let startup_lock = acquire_socket_startup_lock(path)?;
    bind_private_socket_with_lock(startup_lock)
}

/// Binds while retaining a reservation acquired before expensive startup work.
/// The path comes from the guard, so a lock for another socket cannot be used.
pub fn bind_private_socket_with_lock(
    startup_lock: SocketStartupLock,
) -> io::Result<(LocalListener, SocketStartupLock, SocketFileIdentity)> {
    let path = startup_lock.socket_path.as_path();
    prepare_socket_path(path)?;
    let listener = bind_private_local_listener(path)?;
    let identity = socket_file_identity(path)?;
    Ok((listener, startup_lock, identity))
}

/// [`bind_private_socket`] for a path no other process will ever bind, such
/// as a randomly named SSH bridge socket.
///
/// The lock sidecar is created fresh, so one that already exists means the
/// path is taken and is refused as [`SocketBusy`] without touching it. Once
/// locked, the sidecar records this process's identity, which lets
/// [`sweep_abandoned_single_use_sockets`] reclaim the socket and sidecar of
/// an owner killed before its teardown ran. A bind that fails after the
/// sidecar exists removes it, and the socket if one was linked: no later
/// binder can race a single-use path for that inode, which is what makes the
/// removal safe here and unsafe for [`bind_private_socket`]. The caller
/// removes both when it is done, while still holding the returned lock.
pub fn bind_single_use_private_socket(
    path: &Path,
) -> io::Result<(LocalListener, SocketStartupLock, SocketFileIdentity)> {
    let startup_lock = acquire_single_use_socket_lock(path)?;
    let mut listener_bound = false;
    let bound = prepare_socket_path(path)
        .and_then(|()| bind_private_local_listener(path))
        .and_then(|listener| {
            listener_bound = true;
            let identity = socket_file_identity(path)?;
            Ok((listener, identity))
        });
    match bound {
        Ok((listener, identity)) => Ok((listener, startup_lock, identity)),
        Err(error) => {
            // A socket that could not be removed keeps its sidecar: the
            // dead-owner sweep needs both artifacts to validate and reclaim
            // this single-use path.
            if !listener_bound || super::owned_runtime::remove_file(path) {
                super::release_single_use_socket_lock(path);
            }
            drop(startup_lock);
            Err(error)
        }
    }
}

/// Creates, locks and owner-marks the sidecar of a single-use socket path.
/// A sidecar created here and then not locked is removed again.
fn acquire_single_use_socket_lock(socket_path: &Path) -> io::Result<SocketStartupLock> {
    let parent = socket_parent(socket_path)?;
    super::create_private_directory_all(parent)?;
    let entry = match super::owned_runtime::OwnedRuntimeEntry::create_socket(socket_path) {
        Ok(entry) => entry,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(SocketBusy::error(socket_path));
        }
        Err(error) => return Err(error),
    };
    let file = entry.into_hold();
    tracing::info!(
        event = "ipc.socket_lock",
        subsystem = "ipc",
        outcome = "acquired",
        path = %socket_path.display(),
        "single-use socket lock acquired"
    );
    Ok(SocketStartupLock {
        _lock: FlockLock { _file: file },
        socket_path: socket_path.to_path_buf(),
    })
}

/// Removes the sockets and lock sidecars that [`bind_single_use_private_socket`]
/// owners left in `dir` when they were killed before their teardown ran.
///
/// Only a sidecar that is a current-uid regular file recording a process
/// `/proc` proves has exited, and whose lock nobody holds, is reclaimed,
/// together with a current-uid socket at its socket path. An unmarked sidecar
/// (its owner's identity was unreadable, or not yet written) is retained
/// because its owner cannot be established. Sidecars of shared socket paths
/// are never written to, so they never qualify.
pub fn sweep_abandoned_single_use_sockets(dir: &Path) {
    super::owned_runtime::OwnedRuntimeEntry::sweep_socket_sidecars(dir);
}

/// The sidecar file [`acquire_socket_startup_lock`] locks for `socket_path`.
/// It normally outlives the lock (see [`SocketStartupLock`]); only an owner
/// whose socket path is single-use, such as a randomly named SSH bridge
/// socket bound with [`bind_single_use_private_socket`], may remove it, since
/// no later binder can race it for that path.
pub fn socket_startup_lock_path(socket_path: &Path) -> PathBuf {
    let mut name = socket_path.as_os_str().to_os_string();
    name.push(".lock");
    name.into()
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

    let bytes = path.as_os_str().as_bytes();
    // SAFETY: `sockaddr_un` is plain data for which all-zero bytes is a valid
    // (empty) value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("socket path {} cannot be connected to", path.display()),
        ));
    }
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

    // SAFETY: fcntl(2) with F_GETFL/F_SETFL on a descriptor this function owns.
    let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
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
pub fn connect_trusted_local_stream(path: &Path) -> io::Result<LocalStream> {
    connect_trusted_local_stream_within(path, super::limits::LOCAL_CONNECT_TIMEOUT)
}

/// [`connect_trusted_local_stream`] with the connect bounded by `timeout`
/// (see [`connect_local_stream_within`]).
pub fn connect_trusted_local_stream_within(
    path: &Path,
    timeout: Duration,
) -> io::Result<LocalStream> {
    let stream = connect_local_stream_within(path, timeout)?;
    match peer_is_same_effective_user(&stream) {
        Ok(true) => Ok(stream),
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

pub fn bind_local_listener(path: &Path) -> io::Result<LocalListener> {
    // The caller owns socket path cleanup; dropping the listener only closes its fd.
    LocalListener::bind(path)
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
fn prepare_socket_path(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        super::create_private_directory_all(parent)?;
    }

    match probe(path) {
        Liveness::Absent => return Ok(()),
        Liveness::Live => return Err(SocketBusy::error(path)),
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

/// Mode applied by [`bind_private_local_listener`]: owner read/write only.
// limits-exempt: this is the private socket's POSIX file mode, kept beside its application.
const PRIVATE_SOCKET_MODE: u32 = 0o600;

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
/// [`peer_is_same_user`]; the file mode is not the only control.
pub fn bind_private_local_listener(path: &Path) -> io::Result<LocalListener> {
    let parent = socket_parent(path)?;
    match bind_via_private_staging(path, parent) {
        Ok(listener) => {
            tracing::info!(
                event = "ipc.socket_bind",
                subsystem = "ipc",
                outcome = "ok",
                strategy = "staged",
                path = %path.display(),
                "private socket listener bound"
            );
            Ok(listener)
        }
        Err(StagedBindError::Busy) => Err(SocketBusy::error(path)),
        Err(StagedBindError::RandomSource(error)) => Err(error),
        Err(StagedBindError::Unavailable(err)) => {
            tracing::warn!(
                event = "ipc.socket_bind",
                subsystem = "ipc",
                outcome = "staging_unavailable",
                path = %path.display(),
                error = %err,
                "private socket staging failed; binding in place"
            );
            let listener = bind_in_place_then_restrict(path)?;
            tracing::info!(
                event = "ipc.socket_bind",
                subsystem = "ipc",
                outcome = "ok",
                strategy = "in_place",
                path = %path.display(),
                "private socket listener bound"
            );
            Ok(listener)
        }
    }
}

fn bind_in_place_then_restrict(path: &Path) -> io::Result<LocalListener> {
    let listener = bind_local_listener(path).map_err(|error| {
        if error.kind() == io::ErrorKind::AddrInUse {
            SocketBusy::error(path)
        } else {
            error
        }
    })?;
    if let Err(error) = restrict_socket_permissions(path, PRIVATE_SOCKET_MODE) {
        drop(listener);
        // The restrict error is what the caller acts on; a socket left behind
        // with the wrong mode is still worth an operator's attention.
        if let Err(remove_error) = fs::remove_file(path) {
            tracing::warn!(
                path = %path.display(),
                error = %remove_error,
                "failed to remove socket after restricting its mode failed"
            );
        }
        return Err(error);
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

fn bind_via_private_staging(path: &Path, parent: &Path) -> Result<LocalListener, StagedBindError> {
    use super::owned_runtime::{DirectoryKind, OwnedRuntimeEntry, RuntimeCreateError};
    let entry =
        OwnedRuntimeEntry::create_directory(parent, DirectoryKind::Staging).map_err(|error| {
            match error {
                RuntimeCreateError::RandomSource(error) => StagedBindError::RandomSource(error),
                RuntimeCreateError::Io(error) => StagedBindError::Unavailable(error),
            }
        })?;
    let staged = entry.path().join("s");
    let result = bind_staged_and_link(&staged, path);
    entry.release();
    result
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

/// Reports whether a local peer runs as the same user as this process (its
/// effective uid), or as root. This is for accept-side admission: root can
/// connect through the owner-only socket mode anyway, so this preserves
/// `sudo` clients. Clients trusting a server must use
/// [`peer_is_same_effective_user`] instead, because root must not be treated as
/// the owner of the server-side socket.
///
/// Every shepr socket is owner-only, so in normal operation this always
/// holds; it is a second check that does not depend on the socket file's
/// mode, which can be loosened after the fact or, on the in-place bind
/// fallback, briefly be umask-derived. The credentials are the ones the peer
/// had when it connected (`SO_PEERCRED`), so a later privilege drop by the
/// peer does not change the answer.
pub fn peer_is_same_user(stream: &LocalStream) -> io::Result<bool> {
    let peer_uid = peer_uid(stream)?;
    let own_uid = super::effective_uid();
    Ok(peer_uid_is_allowed_client(peer_uid, own_uid))
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
    use std::os::fd::AsRawFd as _;

    let fd = stream.as_raw_fd();
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
    Ok(cred.uid)
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
    use std::time::Duration;

    #[test]
    fn server_lifetime_observes_all_endpoint_transitions_and_stopping_identity() {
        assert_eq!(
            ServerLifetime::observe::<()>(false, false, None),
            ServerPresence::Gone
        );
        assert_eq!(
            ServerLifetime::observe::<()>(true, false, None),
            ServerPresence::Starting
        );
        assert_eq!(
            ServerLifetime::observe::<()>(false, true, None),
            ServerPresence::Releasing
        );
        assert_eq!(
            ServerLifetime::observe::<()>(true, true, None),
            ServerPresence::Unresponsive
        );
        assert_eq!(
            ServerLifetime::observe(true, true, Some(("boot", false))),
            ServerPresence::Running("boot")
        );
        assert_eq!(
            ServerLifetime::observe(true, true, Some(("boot", true))),
            ServerPresence::Stopping("boot")
        );
    }

    #[test]
    fn server_lifetime_orders_reservation_restore_bind_and_release() {
        use std::cell::RefCell;
        let scratch = shepr_test_support::ScratchDir::new("lifetime-order");
        let path = scratch.join("client.sock");
        let steps = RefCell::new(Vec::new());
        let reserved = ServerLifetime::reserve(
            || {
                steps.borrow_mut().push("lease");
                Ok::<_, io::Error>(())
            },
            |_| {
                steps.borrow_mut().push("api");
                Ok(())
            },
            || {
                steps.borrow_mut().push("reserve");
                acquire_socket_startup_lock(&path)
            },
        )
        .expect("reservation");
        let (_listener, _lock, identity) = reserved
            .restore_and_bind(
                |_| {
                    steps.borrow_mut().push("restore");
                    assert!(
                        !path
                            .try_exists()
                            .expect("client not published before restore")
                    );
                    assert!(acquire_socket_startup_lock(&path).is_err());
                },
                |_, _, lock| {
                    steps.borrow_mut().push("bind");
                    bind_private_socket_with_lock(lock)
                },
            )
            .expect("bind reserved client");
        assert_eq!(
            *steps.borrow(),
            ["lease", "api", "reserve", "restore", "bind"]
        );
        let mut releases = Vec::new();
        ServerLifetime::release(
            &mut releases,
            |steps| steps.push("lease"),
            |steps| steps.push("api"),
            |steps| steps.push("client"),
        );
        assert_eq!(releases, ["lease", "api", "client"]);
        remove_socket_file_if_owned(&path, &identity).expect("cleanup");
    }

    #[test]
    fn failed_client_reservation_retires_lease_before_api() {
        use std::cell::RefCell;
        struct Resource<'a>(&'a RefCell<Vec<&'static str>>, &'static str);
        impl Drop for Resource<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push(self.1);
            }
        }
        let dropped = RefCell::new(Vec::new());
        let result = ServerLifetime::reserve(
            || Ok(Resource(&dropped, "lease")),
            |_| Ok(Resource(&dropped, "api")),
            || Err(io::Error::other("reservation refused")),
        );
        assert!(result.is_err());
        assert_eq!(*dropped.borrow(), ["lease", "api"]);
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
        let Err(error) = bind_private_socket(&path) else {
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
        let (listener, lock, _identity) = bind_private_socket(&stale).expect("reclaim");
        let second = bind_private_socket(&stale)
            .err()
            .expect("the first binder holds the startup lock");
        assert_busy_at(&second, &stale);
        drop(listener);
        drop(lock);

        let live = dir.join("live.sock");
        let _foreign = std::os::unix::net::UnixListener::bind(&live).expect("bind live");
        let refused = bind_private_socket(&live)
            .err()
            .expect("a live socket is never replaced");
        assert_busy_at(&refused, &live);
    }

    /// A failed single-use bind removes the lock sidecar it created, while a
    /// failed shared bind keeps its sidecar so racing binders keep locking one
    /// inode. A sidecar that already existed is someone else's: the
    /// single-use bind refuses it as busy and leaves it untouched.
    #[test]
    fn failed_single_use_bind_removes_only_the_lock_it_created() {
        let exists = |path: &Path| path.try_exists().expect("stat");
        let dir = shepr_test_support::ScratchDir::new("bind-single-use-failure");

        let live = dir.join("live.sock");
        let _foreign = std::os::unix::net::UnixListener::bind(&live).expect("bind live");
        let refused = bind_single_use_private_socket(&live)
            .err()
            .expect("a live socket is never replaced");
        assert_busy_at(&refused, &live);
        assert!(
            !exists(&socket_startup_lock_path(&live)),
            "a failed single-use bind removes its lock"
        );
        assert!(exists(&live), "the foreign socket is left alone");

        let shared = bind_private_socket(&live)
            .err()
            .expect("a live socket is never replaced");
        assert_busy_at(&shared, &live);
        assert!(
            exists(&socket_startup_lock_path(&live)),
            "a failed shared bind keeps its lock"
        );

        let taken = dir.join("taken.sock");
        let taken_lock = socket_startup_lock_path(&taken);
        fs::write(&taken_lock, b"another binder").expect("test precondition");
        let refused = bind_single_use_private_socket(&taken)
            .err()
            .expect("an existing lock means the path is taken");
        assert_busy_at(&refused, &taken);
        assert_eq!(
            fs::read(&taken_lock).expect("the existing lock is kept"),
            b"another binder"
        );
        assert!(!exists(&taken));
    }

    #[test]
    fn held_socket_reservation_binds_without_releasing_its_lock() {
        let scratch = shepr_test_support::ScratchDir::new("socket-reservation-handoff");
        let path = scratch.join("client.sock");
        let reservation = acquire_socket_startup_lock(&path).expect("reserve socket");
        assert!(!path.try_exists().expect("unpublished socket"));
        let before = fs::symlink_metadata(socket_startup_lock_path(&path)).expect("lock inode");
        let (listener, lock, identity) =
            bind_private_socket_with_lock(reservation).expect("bind reserved socket");
        let after =
            fs::symlink_metadata(socket_startup_lock_path(&path)).expect("lock inode after bind");
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_busy_at(
            &acquire_socket_startup_lock(&path)
                .err()
                .expect("lock still held"),
            &path,
        );
        drop(listener);
        remove_socket_file_if_owned(&path, &identity).expect("cleanup socket");
        drop(lock);
        let _next = acquire_socket_startup_lock(&path).expect("lock released at teardown");
    }

    #[test]
    fn reserved_socket_bind_refuses_a_listener_that_ignores_the_lock() {
        let scratch = shepr_test_support::ScratchDir::new("socket-reservation-race");
        let path = scratch.join("client.sock");
        let reservation = acquire_socket_startup_lock(&path).expect("reserve socket");
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

    /// The runtime-directory sweep reclaims the socket and lock of a single-use
    /// bind whose owner is provably gone, and nothing else: not a live owner's,
    /// not an unmarked sidecar, not a dead-marked one whose lock is still
    /// held, and not a shared socket's sidecar. Allocating a bridge path runs
    /// the sweep.
    #[test]
    fn abandoned_single_use_sockets_of_dead_owners_are_swept() {
        let exists = |path: &Path| path.try_exists().expect("stat");
        let runtime = shepr_test_support::ScratchDir::new("single-use-sweep");
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))
            .expect("test precondition");
        let live_tag = super::super::process_identity::ProcessIdentity::current()
            .expect("current process identity")
            .tag(0);
        let (_, rest) = live_tag.split_once('-').expect("serialized identity");
        let dead_tag = format!("{:08x}-{rest}", u32::MAX);

        let stale_socket = |name: &str, marker: &[u8]| {
            let socket = runtime.join(name);
            drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind"));
            fs::write(socket_startup_lock_path(&socket), marker).expect("test precondition");
            fs::set_permissions(
                socket_startup_lock_path(&socket),
                fs::Permissions::from_mode(0o600),
            )
            .expect("private marker");
            socket
        };
        let dead = stale_socket("shepr-s-a.0000000000000001.sock", dead_tag.as_bytes());
        let dead_no_socket = runtime.join("shepr-s-b.0000000000000002.sock");
        fs::write(
            socket_startup_lock_path(&dead_no_socket),
            dead_tag.as_bytes(),
        )
        .expect("test precondition");
        fs::set_permissions(
            socket_startup_lock_path(&dead_no_socket),
            fs::Permissions::from_mode(0o600),
        )
        .expect("private marker");
        let live_without_lock = stale_socket("live-unlocked.sock", live_tag.as_bytes());
        let malformed = stale_socket("malformed.sock", b"invalid");
        let oversized = stale_socket(
            "oversized.sock",
            &vec![
                b'x';
                usize::try_from(super::super::limits::RUNTIME_OWNER_MAX_BYTES)
                    .expect("limit fits usize")
                    + 1
            ],
        );
        let unexpected = stale_socket("unexpected.sock", dead_tag.as_bytes());
        fs::remove_file(&unexpected).expect("remove fixture socket");
        fs::write(&unexpected, b"regular file").expect("unexpected socket content");
        let unmarked = stale_socket("shepr-s-c.0000000000000003.sock", b"");
        let held = stale_socket("shepr-s-d.0000000000000004.sock", dead_tag.as_bytes());
        let _held_lock =
            acquire_flock_lock(&socket_startup_lock_path(&held), false).expect("hold lock");
        let shared = runtime.join("server.sock");
        let (shared_listener, shared_lock, _) = bind_private_socket(&shared).expect("bind");
        drop(shared_listener);
        drop(shared_lock);
        let live = runtime.join("shepr-s-e.0000000000000005.sock");
        let (_live_listener, _live_lock, _) =
            bind_single_use_private_socket(&live).expect("bind single-use");
        assert_eq!(
            fs::read_to_string(socket_startup_lock_path(&live)).expect("read marker"),
            live_tag,
            "a single-use lock records its owner"
        );

        crate::remote_bridge_endpoint_path(runtime.path(), "shepr-s-f.sock", "shepr-s-f.sock")
            .expect("allocate a bridge path");

        for path in [&dead, &dead_no_socket] {
            assert!(!exists(path), "{} swept", path.display());
            assert!(
                !exists(&socket_startup_lock_path(path)),
                "{} lock swept",
                path.display()
            );
        }
        for path in [
            &unmarked,
            &held,
            &live,
            &live_without_lock,
            &malformed,
            &oversized,
            &unexpected,
        ] {
            assert!(exists(path), "{} retained", path.display());
            assert!(
                exists(&socket_startup_lock_path(path)),
                "{} lock retained",
                path.display()
            );
        }
        assert!(
            exists(&socket_startup_lock_path(&shared)),
            "a shared socket's lock is retained"
        );
    }

    /// Every busy refusal is `AddrInUse` and names the path it refused.
    fn assert_busy_at(error: &io::Error, path: &Path) {
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        let busy = SocketBusy::from_io(error).expect("a busy refusal carries its path");
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
        let lock = acquire_flock_lock(&lock_path, true).expect("acquire blocking lock");
        let metadata = fs::metadata(&lock_path).expect("lock file exists");
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            metadata.uid(),
            fs::metadata(dir.path())
                .expect("scratch directory exists")
                .uid()
        );
        let first_inode = metadata.ino();

        let error = match acquire_flock_lock(&lock_path, false) {
            Ok(_) => panic!("second nonblocking lock unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(lock);

        let _lock = acquire_flock_lock(&lock_path, true).expect("reacquire blocking lock");
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

            let error = match acquire_socket_startup_lock(path) {
                Ok(_) => panic!("relative socket path unexpectedly locked"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

            let error = match bind_private_socket(path) {
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
        assert!(peer_is_same_user(&server).expect("SO_PEERCRED"));
        assert!(peer_is_same_user(&client).expect("SO_PEERCRED"));
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
