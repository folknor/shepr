//! The session's files on disk: path layout, the regular-file policy, atomic
//! publication, and reading and writing the layout file.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use tracing::warn;

use super::lock::DataDirLease;
use super::schema::{SessionSnapshot, parse_session_file};
use crate::limits::{MAX_SESSION_FILE_BYTES, MAX_SESSION_PATH_SYMLINK_HOPS};

pub(super) const SESSION_FILE_NAME: &str = "session.json";
pub(super) const SNAPSHOT_DIRECTORY_NAME: &str = "session-snapshots";
pub(super) const BACKUP_DIRECTORY_NAME: &str = "session-backups";

/// The session layout file in `data_dir`: the file restore reads and every
/// save replaces.
#[must_use]
pub fn session_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SESSION_FILE_NAME)
}

pub(super) fn snapshot_directory(path: &Path) -> PathBuf {
    path.with_file_name(SNAPSHOT_DIRECTORY_NAME)
}

pub(super) fn backup_directory(path: &Path) -> PathBuf {
    path.with_file_name(BACKUP_DIRECTORY_NAME)
}

/// A session path that resolves to something other than a regular file: a
/// directory, a FIFO, a socket or a device.
#[derive(Debug, Clone, Copy)]
enum NotRegularKind {
    Directory,
    Fifo,
    Socket,
    CharacterDevice,
    BlockDevice,
    Other,
}

impl NotRegularKind {
    fn description(self) -> &'static str {
        match self {
            Self::Directory => "a directory",
            Self::Fifo => "a FIFO",
            Self::Socket => "a socket",
            Self::CharacterDevice => "a character device",
            Self::BlockDevice => "a block device",
            Self::Other => "something else",
        }
    }
}

#[derive(Debug)]
struct NotRegularFile {
    target: PathBuf,
    requested: Option<PathBuf>,
    kind: NotRegularKind,
}

impl std::fmt::Display for NotRegularFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is {}, not a regular file",
            self.target.display(),
            self.kind.description()
        )?;
        if let Some(requested) = &self.requested {
            write!(f, " (resolved from {})", requested.display())?;
        }
        f.write_str("; remove it or make it a regular file")
    }
}

impl std::error::Error for NotRegularFile {}

#[derive(Debug)]
struct SessionPathResolveError {
    path: PathBuf,
    source: std::io::Error,
}

impl std::fmt::Display for SessionPathResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "failed to resolve session path {}: {}",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for SessionPathResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
struct SessionFileTooLarge {
    limit_bytes: usize,
}

impl std::fmt::Display for SessionFileTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session file exceeds {} bytes", self.limit_bytes)
    }
}

impl std::error::Error for SessionFileTooLarge {}

fn session_file_size_limit(error: &std::io::Error) -> Option<usize> {
    error
        .get_ref()?
        .downcast_ref::<SessionFileTooLarge>()
        .map(|too_large| too_large.limit_bytes)
}

enum SessionPathState {
    Absent,
    Regular(std::fs::Metadata),
    NotRegular(std::fs::FileType),
}

/// The inspected target and state of one session path.
///
/// All persistence operations use this resolver so startup checks, reads,
/// replacement checks, and metadata stamps classify the same target through
/// the same bounded symlink walk. Opening still goes through the platform's
/// nonblocking regular-file open, which checks the object actually opened.
pub(super) struct SessionPath {
    requested: PathBuf,
    target: PathBuf,
    state: SessionPathState,
}

impl SessionPath {
    pub(super) fn resolve(path: &Path) -> std::io::Result<Self> {
        Self::resolve_inner(path).map_err(|source| {
            let kind = source.kind();
            std::io::Error::new(
                kind,
                SessionPathResolveError {
                    path: path.to_path_buf(),
                    source,
                },
            )
        })
    }

    fn resolve_inner(path: &Path) -> std::io::Result<Self> {
        let mut target = path.to_path_buf();
        for _ in 0..MAX_SESSION_PATH_SYMLINK_HOPS {
            let metadata = match std::fs::symlink_metadata(&target) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Self {
                        requested: path.to_path_buf(),
                        target,
                        state: SessionPathState::Absent,
                    });
                }
                // Only absence means that the target is missing. An
                // unsearchable parent or other stat failure leaves it unknown.
                Err(err) => return Err(err),
            };
            if !metadata.file_type().is_symlink() {
                return Ok(Self::from_metadata(path, target, metadata));
            }
            let link = std::fs::read_link(&target)?;
            target = if link.is_absolute() {
                link
            } else {
                target.parent().unwrap_or_else(|| Path::new(".")).join(link)
            };
        }
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "session path still resolves through a symlink after the hop limit",
            )),
            Ok(metadata) => Ok(Self::from_metadata(path, target, metadata)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                requested: path.to_path_buf(),
                target,
                state: SessionPathState::Absent,
            }),
            Err(err) => Err(err),
        }
    }

    fn from_metadata(requested: &Path, target: PathBuf, metadata: std::fs::Metadata) -> Self {
        let state = if metadata.file_type().is_file() {
            SessionPathState::Regular(metadata)
        } else {
            SessionPathState::NotRegular(metadata.file_type())
        };
        Self {
            requested: requested.to_path_buf(),
            target,
            state,
        }
    }

    pub(super) fn target(&self) -> &Path {
        &self.target
    }

    fn not_regular(&self, file_type: std::fs::FileType) -> std::io::Error {
        use std::os::unix::fs::FileTypeExt;
        let kind = if file_type.is_dir() {
            NotRegularKind::Directory
        } else if file_type.is_fifo() {
            NotRegularKind::Fifo
        } else if file_type.is_socket() {
            NotRegularKind::Socket
        } else if file_type.is_char_device() {
            NotRegularKind::CharacterDevice
        } else if file_type.is_block_device() {
            NotRegularKind::BlockDevice
        } else {
            NotRegularKind::Other
        };
        let requested = (self.requested != self.target).then(|| self.requested.clone());
        std::io::Error::other(NotRegularFile {
            target: self.target.clone(),
            requested,
            kind,
        })
    }

    pub(super) fn ensure_replaceable(&self) -> std::io::Result<()> {
        match &self.state {
            SessionPathState::NotRegular(file_type) => Err(self.not_regular(*file_type)),
            SessionPathState::Absent | SessionPathState::Regular(_) => Ok(()),
        }
    }

    pub(super) fn regular_metadata(&self) -> std::io::Result<Option<&std::fs::Metadata>> {
        match &self.state {
            SessionPathState::Absent => Ok(None),
            SessionPathState::Regular(metadata) => Ok(Some(metadata)),
            SessionPathState::NotRegular(file_type) => Err(self.not_regular(*file_type)),
        }
    }

    fn open_regular(&self) -> std::io::Result<std::fs::File> {
        self.ensure_replaceable()?;
        shepr_platform::open_regular_file(&self.target)?
            .map_err(|file_type| self.not_regular(file_type))
    }
}

/// Opens `path` for reading only if it resolves to a regular file (see
/// `shepr_platform::open_regular_file`): a FIFO in its place never blocks a
/// read, and anything other than a regular file is a [`NotRegularFile`] error.
pub(super) fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    SessionPath::resolve(path)?.open_regular()
}

/// The metadata stamp of the regular file `path` resolves to, `None` when it
/// is absent.
pub(super) fn regular_file_stamp(
    path: &Path,
) -> std::io::Result<Option<shepr_platform::FileStamp>> {
    let resolved = SessionPath::resolve(path)?;
    let Some(metadata) = resolved.regular_metadata()? else {
        return Ok(None);
    };
    Ok(Some(shepr_platform::FileStamp::from_metadata(metadata)))
}

pub(super) fn read_session_file(path: &Path) -> std::io::Result<String> {
    let file = open_regular(path)?;
    let mut content = Vec::new();
    file.take((MAX_SESSION_FILE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut content)?;
    if content.len() > MAX_SESSION_FILE_BYTES {
        return Err(session_file_too_large(MAX_SESSION_FILE_BYTES));
    }
    String::from_utf8(content)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn session_file_too_large(limit_bytes: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        SessionFileTooLarge { limit_bytes },
    )
}

/// The directory holding `path`; a bare file name lives in `.`.
pub(super) fn containing_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Directories `create_dir_all` will add, from the requested leaf toward its
/// nearest existing ancestor. Each new directory's parent needs a sync for
/// that directory entry to be durable. A stat error other than `NotFound` is
/// returned: read as absence it would add an existing directory to the chain,
/// or stop short of a missing one, and the durability walk would be wrong.
fn missing_directory_chain(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut current = directory;
    while !current.as_os_str().is_empty() && !current.try_exists()? {
        missing.push(current.to_path_buf());
        let Some(parent) = current.parent() else {
            break;
        };
        current = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
    }
    Ok(missing)
}

/// A save whose new content is in place at the target path.
pub(super) use shepr_platform::publish_file::Published;

/// What [`publish_private_file`] does with a target that already exists.
pub(super) use shepr_platform::publish_file::PublishTarget;

/// Publishes `source` at `target` through a private (0600) temporary named by
/// the platform's publication path: write, fsync the file, publish, fsync the
/// directory. A crash leaves either the previous file or the complete new
/// one, never a truncated one. The staging name is unpredictable and
/// exclusively created, so a leftover from an interrupted publish is never
/// reused or removed here.
///
/// With `PublishTarget::CreateOnly` an existing `target` is atomically refused with `AlreadyExists`,
/// and a published target is withdrawn again when the directory sync fails,
/// so that mode only ever returns `Published::Durable` or an error.
/// With `PublishTarget::ReplaceExisting` the target is overwritten, and a completed rename is
/// kept even when the directory sync then reports an error; that comes back
/// as `Published::NotDurable`.
///
/// Both the live session file and the recovery copies go through here, so
/// they share one durability and permission policy. A saved layout names
/// working directories, labels and agent session references, so nothing here
/// may be group- or world-readable.
pub(super) fn publish_private_file(
    source: &mut impl std::io::Read,
    target: &Path,
    existing: PublishTarget,
) -> std::io::Result<Published> {
    use shepr_platform::publish_file::{Durability, PreparedFile, PublishOptions};
    let replace = existing == PublishTarget::ReplaceExisting;
    PreparedFile::prepare(
        target,
        source,
        &PublishOptions {
            preserve_metadata_from: None,
            refuse_symlink_target: false,
            durability: if replace {
                Durability::Directory
            } else {
                Durability::DirectoryOrWithdraw
            },
            existing,
            mode: 0o600,
        },
    )?
    .commit()
}

/// Best-effort removal of a file a failed publish left behind. The publish
/// error is what the caller acts on, so a failed removal is logged rather than
/// returned; an operator should be able to see the stray private file.
pub(super) fn remove_after_failed_publish(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(
            event = "persist.cleanup", subsystem = "persist", outcome = "remove_error",
            path = %path.display(), error = %err,
            "failed to remove a file left by a failed session publish"
        ),
    }
}

/// A sink that keeps output only while it is within `cap`, then counts the
/// rest without retaining it. Oversized JSON costs no more buffer memory than
/// a value that fits, and its full size is still known.
struct CappedBuf {
    bytes: Vec<u8>,
    cap: usize,
    /// Bytes written, kept or not.
    len: usize,
}

impl CappedBuf {
    fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            cap,
            len: 0,
        }
    }
}

impl Write for CappedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.len = self.len.saturating_add(buf.len());
        if self.len <= self.cap {
            self.bytes.extend_from_slice(buf);
        } else {
            // Over the cap: nothing kept is of use any more.
            self.bytes = Vec::new();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn save_to_path(path: &Path, snapshot: &SessionSnapshot) -> std::io::Result<Published> {
    save_to_path_with_size_limit(path, snapshot, MAX_SESSION_FILE_BYTES)
}

fn save_to_path_with_size_limit(
    path: &Path,
    snapshot: &SessionSnapshot,
    size_limit_bytes: usize,
) -> std::io::Result<Published> {
    let mut json = CappedBuf::new(size_limit_bytes);
    serde_json::to_writer_pretty(&mut json, snapshot)?;
    if json.len > size_limit_bytes {
        return Err(session_file_too_large(size_limit_bytes));
    }
    let resolved = SessionPath::resolve(path)?;
    resolved.ensure_replaceable()?;
    let target = resolved.target();
    let directory = containing_directory(target);
    let missing_directories = missing_directory_chain(directory)?;
    // The session root may already have been created by DataDirLease before
    // this save runs; that earlier creator must apply the same private mode.
    shepr_platform::create_private_directory_all(directory)?;
    let mut source = json.bytes.as_slice();
    let published = publish_private_file(&mut source, target, PublishTarget::ReplaceExisting)?;
    if matches!(published, Published::Durable) {
        // Publishing synced the leaf directory. Sync each parent that records
        // a newly created directory, stopping at the existing ancestor.
        for created_directory in missing_directories {
            if let Err(err) =
                shepr_platform::sync_directory(containing_directory(&created_directory))
            {
                return Ok(Published::NotDurable(err));
            }
        }
    }
    Ok(published)
}

/// The unlink state, including whether its containing directory was synced.
#[derive(Debug)]
pub(super) enum ClearOutcome {
    Durable,
    NotDurable(std::io::Error),
}

/// Removes what a save to `path` would have written. Saves write through
/// symlinks (stow users keep the session file in a dotfiles tree), so a clear
/// removes the file the link points at and leaves the link in place, dangling
/// until the next save writes through it again. Removing the link instead
/// would strand the stale target with the old session and turn the next save
/// into a plain file where the link was.
pub(super) fn clear_path(path: &Path) -> std::io::Result<ClearOutcome> {
    clear_path_with_directory_sync(path, shepr_platform::sync_directory)
}

pub(super) fn clear_path_with_directory_sync(
    path: &Path,
    mut sync_directory: impl FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<ClearOutcome> {
    let resolved = SessionPath::resolve(path)?;
    resolved.ensure_replaceable()?;
    let target = resolved.target();
    match std::fs::remove_file(target) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    match sync_directory(containing_directory(target)) {
        Ok(()) => Ok(ClearOutcome::Durable),
        // If the containing directory itself is gone, the target cannot be
        // present there; retain the established no-op result for that case.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ClearOutcome::Durable),
        Err(error) => Ok(ClearOutcome::NotDurable(error)),
    }
}

/// What reading the saved session found.
pub enum SessionLoad {
    /// No session file: a fresh start.
    Missing,
    Loaded(SessionSnapshot),
    /// A session file exists but could not be read or parsed; the reason.
    /// Nothing of it is restored, and the first save backs it up before
    /// replacing it.
    Unusable(shepr_protocol::SessionRestoreFailure),
}

impl SessionLoad {
    #[must_use]
    pub fn into_snapshot(self) -> Option<SessionSnapshot> {
        match self {
            Self::Loaded(snapshot) => Some(snapshot),
            Self::Missing | Self::Unusable(_) => None,
        }
    }
}

/// Refuses a session path that holds something other than a regular file (a
/// directory, a FIFO, a socket, a device). No save could ever replace it, so a
/// server that started anyway would run panes whose layout can never be
/// saved; the server refuses to start instead, before anything is restored.
/// Symlinks are followed, as saves follow them.
pub fn check_session_target(lease: &DataDirLease) -> std::io::Result<()> {
    let path = session_path(lease.directory());
    // Use the shared resolver so startup and later saves apply the same
    // symlink hop limit, including dangling links.
    let resolved = SessionPath::resolve(&path)?;
    resolved.ensure_replaceable()
}

/// The directory a session file is backed up to before a save replaces one
/// that restore could not fully use.
#[must_use]
pub(super) fn session_backup_directory(data_dir: &Path) -> PathBuf {
    backup_directory(&session_path(data_dir))
}

/// Reads the saved layout while the caller owns the data directory.
pub fn load(lease: &DataDirLease) -> SessionLoad {
    let path = session_path(lease.directory());
    let content = match read_session_file(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                event = "persist.restore", subsystem = "persist", outcome = "missing",
                path = %path.display(), "session file is missing"
            );
            return SessionLoad::Missing;
        }
        Err(err) => {
            let detail = if let Some(limit_bytes) = session_file_size_limit(&err) {
                format!("it exceeds the {limit_bytes}-byte session file limit")
            } else {
                format!("it could not be read: {err}")
            };
            let failure = shepr_protocol::SessionRestoreFailure {
                path: shepr_protocol::RemotePath::from(path.as_path()),
                detail,
            };
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), error = %err, "failed to read session file"
            );
            return SessionLoad::Unusable(failure);
        }
    };
    match parse_session_file(&content) {
        Ok(snapshot) => SessionLoad::Loaded(snapshot),
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), error = %err, "failed to parse session file, ignoring"
            );
            SessionLoad::Unusable(shepr_protocol::SessionRestoreFailure {
                path: shepr_protocol::RemotePath::from(path.as_path()),
                detail: format!("it could not be parsed: {err}"),
            })
        }
    }
}

#[cfg(test)]
fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    Ok(SessionPath::resolve(path)?.target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::schema::SNAPSHOT_VERSION;

    /// Whether `error` says a session path is not a regular file.
    fn is_not_regular(error: &std::io::Error) -> bool {
        error
            .get_ref()
            .is_some_and(<dyn std::error::Error + Send + Sync>::is::<NotRegularFile>)
    }

    fn too_large_limit(error: &std::io::Error) -> Option<usize> {
        super::session_file_size_limit(error)
    }

    #[test]
    fn a_session_path_still_a_symlink_after_the_hop_limit_is_refused() {
        let scratch = shepr_test_support::ScratchDir::new("session-symlink-loop");
        let first = scratch.path().join("first.json");
        let second = scratch.path().join("second.json");
        std::os::unix::fs::symlink(&second, &first).expect("test precondition");
        std::os::unix::fs::symlink(&first, &second).expect("test precondition");

        let error = resolve_write_target(&first).expect_err("a symlink loop is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains(&first.display().to_string()));
    }

    #[test]
    fn an_oversized_session_file_is_refused() {
        let scratch = shepr_test_support::ScratchDir::new("session-oversized");
        let path = scratch.path().join("session.json");
        // Sparse, so the test writes no real data.
        std::fs::File::create(&path)
            .and_then(|file| file.set_len(MAX_SESSION_FILE_BYTES as u64 + 1))
            .expect("test precondition");

        let error = read_session_file(&path).expect_err("an oversized file is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(too_large_limit(&error), Some(MAX_SESSION_FILE_BYTES));
    }

    #[test]
    fn saving_an_oversized_session_returns_the_same_typed_limit_error() {
        let path = temp_session_path("oversized-save");

        let error = save_to_path_with_size_limit(&path, &empty_snapshot(), 1)
            .expect_err("an oversized save is refused");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(too_large_limit(&error), Some(1));
    }

    /// A session file whose data directory does not exist yet, so saves
    /// exercise creating it.
    fn temp_session_path(name: &str) -> PathBuf {
        crate::test_support::ScratchDir::new(name)
            .join("data")
            .join("session.json")
    }

    fn empty_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![],
            active: None,
        }
    }

    #[test]
    fn reacquiring_after_release_loads_the_existing_session() {
        let scratch = crate::test_support::ScratchDir::new("released-session-lease");
        let lease = DataDirLease::acquire(&scratch).expect("lease");
        save_to_path(&session_path(lease.directory()), &empty_snapshot()).expect("save");
        assert!(matches!(load(&lease), SessionLoad::Loaded(_)));
        lease.release();
        let lease = DataDirLease::acquire(&scratch).expect("lease after release");
        assert!(matches!(load(&lease), SessionLoad::Loaded(_)));
    }

    #[test]
    fn a_session_path_that_is_not_a_regular_file_is_reported_without_blocking() {
        let scratch = crate::test_support::ScratchDir::new("session-not-regular");
        let lease = DataDirLease::acquire(&scratch).expect("lease");
        let session = session_path(lease.directory());
        check_session_target(&lease).expect("an absent session is fine");
        std::fs::create_dir(&session).expect("test precondition");
        let error = check_session_target(&lease).expect_err("a directory is refused");
        assert!(is_not_regular(&error), "{error}");
        assert!(error.to_string().contains("a directory"), "{error}");
        // A save neither replaces it nor a clear removes it.
        let save_error = save_to_path(&session, &empty_snapshot()).expect_err("refused");
        let clear_error = clear_path(&session).expect_err("refused");
        assert!(is_not_regular(&save_error));
        assert!(is_not_regular(&clear_error));
        assert_eq!(error.to_string(), save_error.to_string());
        assert_eq!(error.to_string(), clear_error.to_string());
        assert!(std::fs::metadata(&session).expect("test stat").is_dir());
        std::fs::remove_dir(&session).expect("test precondition");

        let fifo = std::ffi::CString::new(session.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: a valid NUL-terminated path; mkfifo writes no memory of ours.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        // Reading a FIFO would block forever; these return at once.
        assert!(is_not_regular(
            &check_session_target(&lease).expect_err("a fifo is refused")
        ));
        assert!(matches!(load(&lease), SessionLoad::Unusable(_)));
        assert!(is_not_regular(
            &read_session_file(&session).expect_err("not read")
        ));
    }

    #[test]
    fn a_session_file_that_does_not_parse_is_unusable_not_missing() {
        // Both restore nothing, but only a missing file is a fresh start; an
        // unusable one is a whole saved session the user has to be told about.
        let scratch = crate::test_support::ScratchDir::new("unusable-session");
        let lease = DataDirLease::acquire(&scratch).expect("lease");
        assert!(matches!(load(&lease), SessionLoad::Missing));
        std::fs::write(session_path(lease.directory()), b"{ not a session").expect("write");
        let SessionLoad::Unusable(failure) = load(&lease) else {
            panic!("a damaged session file is unusable");
        };
        assert_eq!(
            failure.path.as_path(),
            session_path(lease.directory()).as_path()
        );
        assert!(failure.detail.contains("it could not be parsed"));
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot()).expect("test precondition");

        assert!(matches!(clear_path(&path), Ok(ClearOutcome::Durable)));

        assert!(!path.try_exists().expect("test stat"));
    }

    #[test]
    fn clear_path_ignores_missing_session_file() {
        let path = temp_session_path("clear-missing");

        assert!(matches!(clear_path(&path), Ok(ClearOutcome::Durable)));

        assert!(!path.try_exists().expect("test stat"));
    }

    #[test]
    fn save_to_path_preserves_existing_symlink() {
        let target = temp_session_path("symlink-target");
        let link = target.with_file_name("link.json");
        save_to_path(&target, &empty_snapshot()).expect("test precondition");
        std::os::unix::fs::symlink(&target, &link).expect("test precondition");

        let mut snap = empty_snapshot();
        snap.active = Some(7);
        save_to_path(&link, &snap).expect("test precondition");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        let parsed =
            parse_session_file(&std::fs::read_to_string(&target).expect("test precondition"))
                .expect("test precondition");
        assert_eq!(parsed.active, Some(7));
    }

    #[test]
    fn save_to_path_writes_through_dangling_symlink() {
        let target = temp_session_path("dangling-target");
        let link = target.with_file_name("link.json");
        std::fs::create_dir_all(target.parent().expect("test precondition"))
            .expect("test precondition");
        std::os::unix::fs::symlink(&target, &link).expect("test precondition");

        save_to_path(&link, &empty_snapshot()).expect("test precondition");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        assert!(target.try_exists().expect("test stat"));
    }

    /// The names in `directory`, sorted.
    fn entry_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .expect("test precondition")
            .map(|entry| {
                entry
                    .expect("test precondition")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_saved_session_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let data_dir = crate::test_support::ScratchDir::new("private-mode").join("data");
        let session = session_path(&data_dir);
        save_to_path(&session, &empty_snapshot()).expect("create private session directory");
        // Publishing renames a fresh private file over the target, so an
        // existing file with a broader mode is replaced, not reused.
        std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o644))
            .expect("test precondition");

        save_to_path(&session, &empty_snapshot()).expect("test precondition");

        let mode = std::fs::metadata(&session)
            .expect("test precondition")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            entry_names(&data_dir),
            ["session.json"],
            "no staging file is left behind"
        );
        let directory_mode = std::fs::metadata(&data_dir)
            .expect("data directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
    }

    #[test]
    fn an_unrelated_leftover_beside_the_session_is_not_touched_by_saves() {
        let path = temp_session_path("unrelated-leftover");
        let directory = path.parent().expect("test precondition");
        std::fs::create_dir_all(directory).expect("test precondition");
        let leftover = path.with_extension("json.tmp");
        std::fs::write(&leftover, b"{\"trunc").expect("test precondition");

        let mut snap = empty_snapshot();
        snap.active = Some(3);
        save_to_path(&path, &snap).expect("test precondition");

        let parsed =
            parse_session_file(&std::fs::read_to_string(&path).expect("test precondition"))
                .expect("test precondition");
        assert_eq!(parsed.active, Some(3));
        assert_eq!(
            std::fs::read(&leftover).expect("test precondition"),
            b"{\"trunc"
        );
        assert_eq!(entry_names(directory), ["session.json", "session.json.tmp"]);
    }

    #[test]
    fn clear_path_removes_symlink_target_and_keeps_the_link() {
        let session = temp_session_path("clear-symlink");
        let dir = session.parent().expect("test precondition");
        std::fs::create_dir_all(dir).expect("test precondition");
        let target = dir.join("real.json");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).expect("test precondition");
        save_to_path(&link, &empty_snapshot()).expect("test precondition");
        assert!(target.try_exists().expect("test stat"));

        assert!(matches!(clear_path(&link), Ok(ClearOutcome::Durable)));

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        assert!(!target.try_exists().expect("test stat"));
        // Clearing again with the link dangling is a no-op.
        assert!(matches!(clear_path(&link), Ok(ClearOutcome::Durable)));
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn resolve_write_target_returns_a_stat_error_other_than_not_found() {
        use std::os::unix::fs::PermissionsExt;

        let session = temp_session_path("unsearchable");
        let dir = session.parent().expect("test precondition");
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).expect("test precondition");
        let path = locked.join("session.json");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("test precondition");
        let inspectable = !std::fs::symlink_metadata(&path)
            .is_err_and(|err| err.kind() == std::io::ErrorKind::PermissionDenied);

        let resolved = resolve_write_target(&path);
        let cleared = clear_path(&path);

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
            .expect("test cleanup");
        // A privileged runner can search the directory anyway; there is no
        // stat error to observe then.
        if inspectable {
            return;
        }
        let resolve_error = resolved.expect_err("an unreadable path is not absent");
        let clear_error = cleared.expect_err("a clear must not guess");
        assert_eq!(resolve_error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(clear_error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            resolve_error
                .to_string()
                .contains(&path.display().to_string())
        );
        assert!(
            clear_error
                .to_string()
                .contains(&path.display().to_string())
        );
    }

    #[test]
    fn non_regular_symlink_errors_report_the_target_and_link_consistently() {
        let scratch = crate::test_support::ScratchDir::new("session-non-regular-link");
        let lease = DataDirLease::acquire(&scratch).expect("lease");
        let link = session_path(lease.directory());
        let target = scratch.path().join("session-directory");
        std::fs::create_dir(&target).expect("test precondition");
        std::os::unix::fs::symlink(&target, &link).expect("test precondition");

        let startup_error = check_session_target(&lease).expect_err("directory target is refused");
        let read_error = read_session_file(&link).expect_err("directory target is not read");
        let save_error =
            save_to_path(&link, &empty_snapshot()).expect_err("directory not replaced");
        let clear_error = clear_path(&link).expect_err("directory not removed");
        let message = startup_error.to_string();

        assert!(message.contains(&target.display().to_string()), "{message}");
        assert!(message.contains(&link.display().to_string()), "{message}");
        assert_eq!(message, read_error.to_string());
        assert_eq!(message, save_error.to_string());
        assert_eq!(message, clear_error.to_string());
    }

    #[test]
    fn save_to_path_resolves_relative_symlink() {
        let session = temp_session_path("relative-symlink");
        let dir = session.parent().expect("test precondition");
        std::fs::create_dir_all(dir).expect("test precondition");
        let target = dir.join("real.json");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).expect("test precondition");

        save_to_path(&link, &empty_snapshot()).expect("test precondition");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        assert!(target.try_exists().expect("test stat"));
    }
}
