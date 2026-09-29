use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use interprocess::local_socket::traits::Stream as _;
use sha2::{Digest as _, Sha256};

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

/// An exclusive, nonblocking lock for a server socket's startup and lifetime.
///
/// [`bind_private_socket`] takes it before preparing the path; keep it until
/// the listener has stopped. The regular sidecar file stays beside the socket after the
/// guard drops so later processes always lock the same inode.
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
        fs::create_dir_all(parent)?;
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

    let mut operation = libc::LOCK_EX;
    if !blocking {
        operation |= libc::LOCK_NB;
    }
    loop {
        // SAFETY: flock(2) uses only the open descriptor owned by `file` and
        // does not read or write memory through the call.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result == 0 {
            return Ok(FlockLock { _file: file });
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
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("server startup lock is held for {}", socket_path.display()),
            ));
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
pub fn bind_private_socket(
    path: &Path,
    busy_message: impl Fn(&Path) -> String,
) -> io::Result<(LocalListener, SocketStartupLock, SocketFileIdentity)> {
    let startup_lock = acquire_socket_startup_lock(path)?;
    prepare_socket_path(path, &busy_message)?;
    let listener = bind_private_local_listener(path).map_err(|error| {
        if error.kind() == io::ErrorKind::AddrInUse {
            io::Error::new(io::ErrorKind::AddrInUse, busy_message(path))
        } else {
            error
        }
    })?;
    let identity = socket_file_identity(path)?;
    Ok((listener, startup_lock, identity))
}

fn socket_startup_lock_path(socket_path: &Path) -> PathBuf {
    let mut name = socket_path.as_os_str().to_os_string();
    name.push(".lock");
    name.into()
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

/// Readies `path` for binding: creates its parent, removes a stale socket
/// where nothing listens, and refuses a live one. Only [`bind_private_socket`]
/// calls it, under the startup lock, so a stale socket is never reclaimed by a
/// caller that does not own the path.
fn prepare_socket_path(path: &Path, busy_message: impl FnOnce(&Path) -> String) -> io::Result<()> {
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

/// Binds a listener at an absolute `path` so the socket is never reachable with anything
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
        Err(StagedBindError::Busy) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("socket busy at {}", path.display()),
        )),
        Err(StagedBindError::RandomSource(error)) => Err(error),
        Err(StagedBindError::Unavailable(err)) => {
            tracing::warn!(
                event = "ipc.socket_bind",
                subsystem = "ipc",
                outcome = "staging_unavailable",
                path = %path.display(),
                err = %err,
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
    let listener = bind_local_listener(path)?;
    if let Err(error) = restrict_socket_permissions(path, PRIVATE_SOCKET_MODE) {
        drop(listener);
        // The restrict error is what the caller acts on; a socket left behind
        // with the wrong mode is still worth an operator's attention.
        if let Err(remove_error) = fs::remove_file(path) {
            tracing::warn!(
                path = %path.display(),
                err = %remove_error,
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
        match fs::DirBuilder::new().mode(0o700).create(&staging_dir) {
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
                err = %error,
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
            err = %restore_error,
            "failed to restore socket staging owner marker"
        );
    }
    tracing::warn!(
        path = %staging_dir.display(),
        err = %error,
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
        || parent_metadata.permissions().mode() & 0o7777 != 0o700
    {
        return;
    }
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(path = %parent.display(), err = %error, "could not scan socket staging parent");
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
            || directory_metadata.permissions().mode() & 0o777 != 0o700
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

/// Readiness-only result for callers that only need to know whether a read
/// produced data. Use [`poll_local_stream_read_count`] when the byte count
/// matters.
pub fn poll_local_stream_read(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamRead> {
    match stream.read(buf) {
        Ok(0) => Ok(LocalStreamRead::Closed),
        Ok(_) => Ok(LocalStreamRead::Data),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(LocalStreamRead::Pending),
        Err(err) => Err(err),
    }
}

/// Like [`poll_local_stream_read`], but preserves the number of bytes read for
/// callers that need to consume a variable-sized buffer.
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

    /// Binding takes the startup lock before touching the path: a stale socket
    /// is reclaimed, a live one it does not own is refused with the caller's
    /// message, and a second binder is refused while the first holds the lock.
    #[test]
    fn bind_private_socket_reclaims_stale_refuses_live_and_holds_its_lock() {
        let busy = |path: &Path| format!("busy at {}", path.display());
        let dir = shepr_test_support::ScratchDir::new("bind-private-socket");

        let stale = dir.join("stale.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&stale).expect("bind stale");
        }
        let (listener, lock, _identity) = bind_private_socket(&stale, busy).expect("reclaim");
        let second = bind_private_socket(&stale, busy)
            .err()
            .expect("the first binder holds the startup lock");
        assert_eq!(second.kind(), io::ErrorKind::AddrInUse);
        drop(listener);
        drop(lock);

        let live = dir.join("live.sock");
        let _foreign = std::os::unix::net::UnixListener::bind(&live).expect("bind live");
        let refused = bind_private_socket(&live, busy)
            .err()
            .expect("a live socket is never replaced");
        assert_eq!(refused.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(refused.to_string(), busy(&live));
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

            let error = match bind_private_socket(path, |_| String::new()) {
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
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(listener);

        let plain = dir.join("plain");
        fs::write(&plain, b"keep").expect("test precondition");
        let err = bind_private_local_listener(&plain).expect_err("path is taken");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(fs::read(&plain).expect("file kept"), b"keep");

        fs::remove_dir_all(&dir).expect("remove the socket directory");
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
        fs::remove_file(&path).expect("remove the bound socket");
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
