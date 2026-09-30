use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;
use sha2::{Digest as _, Sha256};

pub type LocalListener = interprocess::local_socket::Listener;
pub type LocalStream = interprocess::local_socket::Stream;

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

/// Another process already holds a server socket path: its startup lock, a
/// live listener at the path, or a file that raced the bind into place.
///
/// Every busy refusal from this module is an [`io::ErrorKind::AddrInUse`]
/// error carrying this payload, so the path survives whichever caller sees it
/// and nobody gets a bare "address in use". Callers that word the refusal
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
fn flock_exclusive(file: &fs::File, blocking: bool) -> io::Result<()> {
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
            if listener_bound {
                remove_single_use_file(path, "socket");
            }
            remove_single_use_file(&socket_startup_lock_path(path), "socket lock");
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
    let lock_path = socket_startup_lock_path(socket_path);
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            tracing::info!(
                event = "ipc.socket_lock",
                subsystem = "ipc",
                outcome = "busy",
                path = %socket_path.display(),
                "single-use socket lock already exists"
            );
            return Err(SocketBusy::error(socket_path));
        }
        Err(error) => return Err(error),
    };
    if let Err(error) = flock_exclusive(&file, false) {
        drop(file);
        remove_single_use_file(&lock_path, "socket lock");
        return Err(error);
    }
    // Written only once locked, so a sweep that reads a complete identity
    // also finds the lock of a live owner held. Without a readable identity
    // the sidecar stays unmarked, and the sweep never removes an unmarked one.
    match super::process_identity::ProcessIdentity::current() {
        Ok(owner) => {
            if let Err(error) = (&file).write_all(owner.tag(0).as_bytes()) {
                drop(file);
                remove_single_use_file(&lock_path, "socket lock");
                return Err(error);
            }
        }
        Err(error) => {
            tracing::debug!(%error, "could not record single-use socket owner identity; a leaked socket will be retained");
        }
    }
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

/// Removes a file of a single-use socket that this process owns. Absence is
/// success; any other failure leaves a file in the runtime directory, which is
/// worth a line naming it.
fn remove_single_use_file(path: &Path, what: &str) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not remove single-use {what}");
        }
    }
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
    let uid = super::effective_uid();
    let Ok(dir_metadata) = fs::symlink_metadata(dir) else {
        return;
    };
    if !dir_metadata.file_type().is_dir()
        || dir_metadata.uid() != uid
        || dir_metadata.permissions().mode() & 0o7777 != super::limits::PRIVATE_DIRECTORY_MODE
    {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(path = %dir.display(), error = %error, "could not scan single-use socket directory");
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(socket_name) = name.to_str().and_then(|name| name.strip_suffix(".lock")) else {
            continue;
        };
        if socket_name.is_empty() {
            continue;
        }
        reclaim_abandoned_single_use_socket(&entry.path(), &dir.join(socket_name), uid);
    }
}

fn reclaim_abandoned_single_use_socket(lock_path: &Path, socket_path: &Path, uid: u32) {
    // O_NONBLOCK keeps a FIFO that happens to end in `.lock` from stalling
    // the open; anything but a regular file is refused just below.
    let Ok(mut file) = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)
    else {
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.len() > super::limits::SINGLE_USE_SOCKET_OWNER_MAX_BYTES
    {
        return;
    }
    let mut marker = String::new();
    if file.read_to_string(&mut marker).is_err() {
        return;
    }
    let Some((owner, 0)) = super::process_identity::ProcessIdentity::parse_tag(&marker) else {
        return;
    };
    // A held lock means a live owner, whatever `/proc` says. Holding it
    // through the removals keeps a concurrent sweep off the same files.
    if !owner.is_provably_gone() || flock_exclusive(&file, false).is_err() {
        return;
    }
    // The name must still be the inode just inspected.
    let Ok(current) = fs::symlink_metadata(lock_path) else {
        return;
    };
    if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return;
    }
    match fs::symlink_metadata(socket_path) {
        Ok(socket) if socket.file_type().is_socket() && socket.uid() == uid => {
            remove_single_use_file(socket_path, "socket");
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        // Anything else at the socket path, or a failed stat, leaves both
        // files alone.
        Ok(_) | Err(_) => return,
    }
    remove_single_use_file(lock_path, "socket lock");
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
    Ok(LocalStream::from(
        interprocess::os::unix::uds_local_socket::Stream::from(UnixStream::from(socket)),
    ))
}

/// Connects to a server socket and checks its owner before returning the
/// stream, so no request or attach byte is ever written to a socket served by
/// another user.
///
/// This is the client-side counterpart of the accept-side [`peer_is_same_user`]
/// check. The peer of a stream that connected is a listener, so its
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
    match peer_is_same_user(&stream) {
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
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};

    let name = path.to_fs_name::<GenericFilePath>()?;
    ListenerOptions::new()
        .name(name)
        .reclaim_name(false)
        .create_sync()
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

pub fn set_local_stream_polling(stream: &mut LocalStream, enabled: bool) -> io::Result<()> {
    stream.set_nonblocking(enabled)
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
    use std::os::unix::fs::DirBuilderExt as _;

    // Without a readable identity the directory is staged unmarked, and the
    // sweep never removes an unmarked directory.
    let owner = super::process_identity::ProcessIdentity::current()
        .inspect_err(|error| {
            tracing::debug!(%error, "could not record socket staging owner identity; a leaked staging directory will be retained");
        })
        .ok();
    sweep_stale_socket_staging_dirs(parent);
    let mut last_error = None;
    for _ in 0..super::limits::RANDOM_NAME_ATTEMPTS {
        // A compact random name keeps staging usable for socket paths near
        // the socket path limit.
        let staging_token =
            super::random::unpredictable_token().map_err(StagedBindError::RandomSource)?;
        let staging_name = format!(".s{staging_token:016x}");
        let staging_dir = parent.join(staging_name);
        // A name somebody else already created is never used: the directory
        // must be ours and fresh for the 0700 guarantee to hold.
        match fs::DirBuilder::new()
            .mode(super::limits::PRIVATE_DIRECTORY_MODE)
            .create(&staging_dir)
        {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(err);
                continue;
            }
            Err(err) => return Err(StagedBindError::Unavailable(err)),
        }
        if let Some(owner) = owner
            && let Err(error) = write_staging_owner_marker(&staging_dir, owner)
        {
            remove_staging_directory(&staging_dir, Some(owner));
            return Err(StagedBindError::Unavailable(error));
        }
        let staged = staging_dir.join("s");
        let result = bind_staged_and_link(&staged, path);
        // The staged name is absent when binding it failed, so NotFound is the
        // expected outcome there. Anything else leaks a private directory in
        // the runtime directory, which an operator needs to see.
        remove_staging_entry(&staged);
        remove_staging_directory(&staging_dir, owner);
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

const STAGING_OWNER_MARKER: &str = ".owner";

fn write_staging_owner_marker(
    staging_dir: &Path,
    owner: super::process_identity::ProcessIdentity,
) -> io::Result<()> {
    let mut marker = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(staging_dir.join(STAGING_OWNER_MARKER))?;
    marker.write_all(owner.tag(0).as_bytes())?;
    Ok(())
}

fn remove_staging_entry(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to remove socket staging entry"
            );
            false
        }
    }
}

/// Removes a staging directory and its marker. When the directory itself
/// cannot be removed, the marker is written back so a later sweep can still
/// prove the directory abandoned.
fn remove_staging_directory(
    staging_dir: &Path,
    owner: Option<super::process_identity::ProcessIdentity>,
) {
    let marker_may_be_missing = remove_staging_entry(&staging_dir.join(STAGING_OWNER_MARKER));
    let error = match fs::remove_dir(staging_dir) {
        Ok(()) => return,
        // A concurrent sweep removed it first.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => error,
    };
    if marker_may_be_missing
        && let Some(owner) = owner
        && let Err(restore_error) = write_staging_owner_marker(staging_dir, owner)
    {
        tracing::warn!(
            path = %staging_dir.display(),
            error = %restore_error,
            "failed to restore socket staging owner marker"
        );
    }
    tracing::warn!(
        path = %staging_dir.display(),
        error = %error,
        "failed to remove socket staging directory"
    );
}

/// Remove only private staging directories whose marker records a process
/// that `/proc` proves has exited. An unmarked directory (staged while the
/// owner identity was unreadable, or whose marker is not yet written) is
/// retained because its owner cannot be established.
fn sweep_stale_socket_staging_dirs(parent: &Path) {
    let uid = super::effective_uid();
    let Ok(parent_metadata) = fs::symlink_metadata(parent) else {
        return;
    };
    if !parent_metadata.file_type().is_dir()
        || parent_metadata.uid() != uid
        || parent_metadata.permissions().mode() & 0o7777 != super::limits::PRIVATE_DIRECTORY_MODE
    {
        return;
    }
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(path = %parent.display(), error = %error, "could not scan socket staging parent");
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(token) = name.to_str().and_then(|name| name.strip_prefix(".s")) else {
            continue;
        };
        if token.len() != 16 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let staging_dir = entry.path();
        let Ok(directory_metadata) = fs::symlink_metadata(&staging_dir) else {
            continue;
        };
        if !directory_metadata.file_type().is_dir()
            || directory_metadata.uid() != uid
            || directory_metadata.permissions().mode() & 0o777
                != super::limits::PRIVATE_DIRECTORY_MODE
        {
            continue;
        }
        let marker_path = staging_dir.join(STAGING_OWNER_MARKER);
        let Ok(marker_metadata) = fs::symlink_metadata(&marker_path) else {
            continue;
        };
        if !marker_metadata.file_type().is_file()
            || marker_metadata.uid() != uid
            || marker_metadata.permissions().mode() & 0o777 != 0o600
        {
            continue;
        }
        let Ok(marker) = fs::read_to_string(&marker_path) else {
            continue;
        };
        let Some((owner, 0)) = super::process_identity::ProcessIdentity::parse_tag(&marker) else {
            continue;
        };
        if !owner.is_provably_gone() || !staging_contents_are_owned(&staging_dir, uid) {
            continue;
        }
        remove_staging_entry(&staging_dir.join("s"));
        remove_staging_directory(&staging_dir, Some(owner));
    }
}

fn staging_contents_are_owned(staging_dir: &Path, uid: u32) -> bool {
    let Ok(entries) = fs::read_dir(staging_dir) else {
        return false;
    };
    let mut marker_seen = false;
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let name = entry.file_name();
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            return false;
        };
        if metadata.uid() != uid {
            return false;
        }
        match name.to_str() {
            Some(STAGING_OWNER_MARKER) if metadata.file_type().is_file() => marker_seen = true,
            Some("s") if metadata.file_type().is_socket() => {}
            _ => return false,
        }
    }
    marker_seen
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
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let own_uid = unsafe { libc::geteuid() };
    Ok(cred.uid == own_uid || cred.uid == 0)
}

/// A local-socket adapter for the shared fd readiness deadline reader.
pub struct LocalStreamDeadlineReader<'a> {
    inner: super::child_io::DeadlineReader<LocalStreamReader<'a>>,
}

/// Preserve the public path used by API and client crates.
pub type DeadlineReader<'a> = LocalStreamDeadlineReader<'a>;

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
        use std::os::fd::AsFd as _;

        match &*self.stream {
            LocalStream::UdSocket(inner) => inner.as_fd().as_raw_fd(),
        }
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
            socket
        };
        let dead = stale_socket("shepr-s-a.0000000000000001.sock", dead_tag.as_bytes());
        let dead_no_socket = runtime.join("shepr-s-b.0000000000000002.sock");
        fs::write(
            socket_startup_lock_path(&dead_no_socket),
            dead_tag.as_bytes(),
        )
        .expect("test precondition");
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
        for path in [&unmarked, &held, &live] {
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

    fn connected_pair(name: &str) -> (LocalStream, LocalStream) {
        use interprocess::local_socket::traits::Listener as _;

        let path = test_socket_path(name);
        let listener = bind_local_listener(&path).expect("test precondition");
        let client = connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        fs::remove_file(&path).expect("remove the bound socket");
        (client, server)
    }

    #[test]
    fn deadline_reader_cuts_off_a_peer_at_the_overall_deadline() {
        use interprocess::local_socket::traits::Listener as _;
        use std::io::Write as _;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let path = test_socket_path("deadline");
        let listener = bind_local_listener(&path).expect("test precondition");
        let mut client = connect_local_stream(&path).expect("test precondition");
        let mut server = listener.accept().expect("test precondition");
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
