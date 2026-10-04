//! The session's files on disk: path layout, the regular-file policy, atomic
//! publication, and reading and writing the layout and history files.

use std::io::Read;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::history::{
    CappedBuf, HistoryDigest, MAX_SESSION_HISTORY_FILE_BYTES, SessionHistory, ensure_history_size,
    history_digest, serialize_history,
};
use super::lock::DataDirLease;
use super::schema::{
    SessionFile, SessionHistorySnapshot, SessionSnapshot, parse_history_snapshot,
    parse_session_file,
};

/// The session layout file's size bound, for saves and for the reads restore
/// and snapshot recovery make, so a damaged file cannot allocate without limit.
const MAX_SESSION_FILE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum symlink hops when finding a writable session path; bounds cycles
/// while allowing an ordinary chain of user-managed links.
const MAX_SESSION_PATH_SYMLINK_HOPS: usize = 16;

pub(super) const SESSION_FILE_NAME: &str = "session.json";
const SESSION_HISTORY_FILE_NAME: &str = "session-history.json";
pub(super) const SNAPSHOT_DIRECTORY_NAME: &str = "session-snapshots";
pub(super) const BACKUP_DIRECTORY_NAME: &str = "session-backups";

pub(super) fn session_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SESSION_FILE_NAME)
}

pub(super) fn session_history_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SESSION_HISTORY_FILE_NAME)
}

pub(super) fn snapshot_directory(path: &Path) -> PathBuf {
    path.with_file_name(SNAPSHOT_DIRECTORY_NAME)
}

pub(super) fn backup_directory(path: &Path) -> PathBuf {
    path.with_file_name(BACKUP_DIRECTORY_NAME)
}

/// A session or history path that resolves to something other than a regular
/// file: a directory, a FIFO, a socket or a device.
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
    path: PathBuf,
    kind: NotRegularKind,
}

impl std::fmt::Display for NotRegularFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is {}, not a regular file; remove it or make it a regular file",
            self.path.display(),
            self.kind.description()
        )
    }
}

impl std::error::Error for NotRegularFile {}

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

pub(super) fn not_regular(path: &Path, file_type: std::fs::FileType) -> std::io::Error {
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
    std::io::Error::other(NotRegularFile {
        path: path.to_path_buf(),
        kind,
    })
}

/// Whether `error` says a session or history path is not a regular file.
pub(super) fn is_not_regular(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<NotRegularFile>)
}

fn session_file_kind(error: &std::io::Error) -> Option<shepr_protocol::SessionFileKind> {
    let file = error.get_ref()?.downcast_ref::<NotRegularFile>()?;
    Some(match file.kind {
        NotRegularKind::Directory => shepr_protocol::SessionFileKind::Directory,
        NotRegularKind::Fifo => shepr_protocol::SessionFileKind::Fifo,
        NotRegularKind::Socket => shepr_protocol::SessionFileKind::Socket,
        NotRegularKind::CharacterDevice => shepr_protocol::SessionFileKind::CharacterDevice,
        NotRegularKind::BlockDevice => shepr_protocol::SessionFileKind::BlockDevice,
        NotRegularKind::Other => shepr_protocol::SessionFileKind::Other,
    })
}

fn session_io_error_kind(kind: std::io::ErrorKind) -> shepr_protocol::SessionIoErrorKind {
    use shepr_protocol::SessionIoErrorKind;

    match kind {
        std::io::ErrorKind::NotFound => SessionIoErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied => SessionIoErrorKind::PermissionDenied,
        std::io::ErrorKind::AlreadyExists => SessionIoErrorKind::AlreadyExists,
        std::io::ErrorKind::ConnectionRefused => SessionIoErrorKind::ConnectionRefused,
        std::io::ErrorKind::ConnectionReset => SessionIoErrorKind::ConnectionReset,
        std::io::ErrorKind::ConnectionAborted => SessionIoErrorKind::ConnectionAborted,
        std::io::ErrorKind::NotConnected => SessionIoErrorKind::NotConnected,
        std::io::ErrorKind::AddrInUse => SessionIoErrorKind::AddrInUse,
        std::io::ErrorKind::AddrNotAvailable => SessionIoErrorKind::AddrNotAvailable,
        std::io::ErrorKind::BrokenPipe => SessionIoErrorKind::BrokenPipe,
        std::io::ErrorKind::WouldBlock => SessionIoErrorKind::WouldBlock,
        std::io::ErrorKind::InvalidInput => SessionIoErrorKind::InvalidInput,
        std::io::ErrorKind::InvalidData => SessionIoErrorKind::InvalidData,
        std::io::ErrorKind::ResourceBusy => SessionIoErrorKind::ResourceBusy,
        std::io::ErrorKind::TimedOut => SessionIoErrorKind::TimedOut,
        std::io::ErrorKind::Interrupted => SessionIoErrorKind::Interrupted,
        std::io::ErrorKind::Unsupported => SessionIoErrorKind::Unsupported,
        std::io::ErrorKind::UnexpectedEof => SessionIoErrorKind::UnexpectedEof,
        std::io::ErrorKind::OutOfMemory => SessionIoErrorKind::OutOfMemory,
        std::io::ErrorKind::WriteZero => SessionIoErrorKind::WriteZero,
        _ => SessionIoErrorKind::Other,
    }
}

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

/// The inspected target and state of one session or history path.
///
/// All persistence operations use this resolver so startup checks, reads,
/// replacement checks, and metadata stamps classify the same target through
/// the same bounded symlink walk. Opening still goes through the platform's
/// nonblocking regular-file open, which checks the object actually opened.
pub(super) struct SessionPath {
    target: PathBuf,
    state: SessionPathState,
}

impl SessionPath {
    pub(super) fn resolve(path: &Path) -> std::io::Result<Self> {
        let mut target = path.to_path_buf();
        for _ in 0..MAX_SESSION_PATH_SYMLINK_HOPS {
            let metadata = match std::fs::symlink_metadata(&target) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Self {
                        target,
                        state: SessionPathState::Absent,
                    });
                }
                // Only absence means that the target is missing. An
                // unsearchable parent or other stat failure leaves it unknown.
                Err(err) => return Err(err),
            };
            if !metadata.file_type().is_symlink() {
                return Ok(Self::from_metadata(target, metadata));
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
                "session or history path still resolves through a symlink after the hop limit",
            )),
            Ok(metadata) => Ok(Self::from_metadata(target, metadata)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                target,
                state: SessionPathState::Absent,
            }),
            Err(err) => Err(err),
        }
    }

    fn from_metadata(target: PathBuf, metadata: std::fs::Metadata) -> Self {
        let state = if metadata.file_type().is_file() {
            SessionPathState::Regular(metadata)
        } else {
            SessionPathState::NotRegular(metadata.file_type())
        };
        Self { target, state }
    }

    pub(super) fn target(&self) -> &Path {
        &self.target
    }

    pub(super) fn ensure_replaceable(&self, path: &Path) -> std::io::Result<()> {
        match &self.state {
            SessionPathState::NotRegular(file_type) => Err(not_regular(path, *file_type)),
            SessionPathState::Absent | SessionPathState::Regular(_) => Ok(()),
        }
    }

    pub(super) fn regular_metadata(
        &self,
        path: &Path,
    ) -> std::io::Result<Option<&std::fs::Metadata>> {
        match &self.state {
            SessionPathState::Absent => Ok(None),
            SessionPathState::Regular(metadata) => Ok(Some(metadata)),
            SessionPathState::NotRegular(file_type) => Err(not_regular(path, *file_type)),
        }
    }

    fn open_regular(&self, path: &Path) -> std::io::Result<std::fs::File> {
        self.ensure_replaceable(path)?;
        shepr_platform::open_regular_file(&self.target)?
            .map_err(|file_type| not_regular(path, file_type))
    }
}

/// Opens `path` for reading only if it resolves to a regular file (see
/// `shepr_platform::open_regular_file`): a FIFO in its place never blocks a
/// read, and anything other than a regular file is a [`NotRegularFile`] error.
pub(super) fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    SessionPath::resolve(path)?.open_regular(path)
}

/// The metadata stamp of the regular file `path` resolves to, `None` when it
/// is absent.
pub(super) fn regular_file_stamp(
    path: &Path,
) -> std::io::Result<Option<shepr_platform::FileStamp>> {
    let resolved = SessionPath::resolve(path)?;
    let Some(metadata) = resolved.regular_metadata(path)? else {
        return Ok(None);
    };
    Ok(Some(shepr_platform::FileStamp::from_metadata(metadata)))
}

fn read_history_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = open_regular(path)?;
    let mut content = Vec::new();
    file.take((MAX_SESSION_HISTORY_FILE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut content)?;
    ensure_history_size(content.len())?;
    Ok(content)
}

pub(super) fn read_session_file(path: &Path) -> std::io::Result<String> {
    let file = open_regular(path)?;
    let mut content = Vec::new();
    file.take((MAX_SESSION_FILE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut content)?;
    if content.len() > MAX_SESSION_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            SessionFileTooLarge {
                limit_bytes: MAX_SESSION_FILE_BYTES,
            },
        ));
    }
    String::from_utf8(content)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
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
/// Both the live session files and the recovery copies go through here, so
/// they share one durability and permission policy. Session history can hold
/// full pane scrollback up to its file-size limit and can include tokens, so
/// nothing here may be group- or world-readable.
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

pub(super) fn save_to_path(
    path: &Path,
    snapshot: &SessionSnapshot,
    history_digest: Option<&HistoryDigest>,
) -> std::io::Result<Published> {
    save_json_to_path(
        path,
        &SessionFile {
            snapshot,
            history_digest: history_digest.copied(),
        },
    )
}

fn save_json_to_path<T: serde::Serialize>(path: &Path, snapshot: &T) -> std::io::Result<Published> {
    let mut json = CappedBuf::new(MAX_SESSION_FILE_BYTES);
    serde_json::to_writer_pretty(&mut json, snapshot)?;
    if json.len > MAX_SESSION_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("session file exceeds {MAX_SESSION_FILE_BYTES} bytes"),
        ));
    }
    save_serialized_to_path(path, &json.bytes)
}

fn save_serialized_to_path(path: &Path, json: &[u8]) -> std::io::Result<Published> {
    let resolved = SessionPath::resolve(path)?;
    resolved.ensure_replaceable(resolved.target())?;
    let target = resolved.target();
    let directory = containing_directory(target);
    let missing_directories = missing_directory_chain(directory)?;
    // The session root may already have been created by DataDirLease before
    // this save runs; that earlier creator must apply the same private mode.
    shepr_platform::create_private_directory_all(directory)?;
    let mut source = json;
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

/// Optional history has no follow-up work that depends on it, so a
/// published-but-unsynced write is reported like any other failure.
pub(super) fn save_history_to_path(
    path: &Path,
    history: Option<&SessionHistory>,
) -> std::io::Result<()> {
    match history {
        Some(history) => save_history_json_to_path(path, &serialize_history(history)?.json),
        None => clear_path(path),
    }
}

/// Writes history that `serialize_history` already produced, so a caller that
/// needs the bytes too (to tell whether anything changed) serializes once.
pub(super) fn save_history_json_to_path(path: &Path, json: &[u8]) -> std::io::Result<()> {
    ensure_history_size(json.len())?;
    match save_serialized_to_path(path, json)? {
        Published::Durable => Ok(()),
        Published::NotDurable(err) => Err(err),
    }
}

/// Removes what a save to `path` would have written. Saves write through
/// symlinks (stow users keep the session file in a dotfiles tree), so a clear
/// removes the file the link points at and leaves the link in place, dangling
/// until the next save writes through it again. Removing the link instead
/// would strand the stale target with the old session and turn the next save
/// into a plain file where the link was.
pub(super) fn clear_path(path: &Path) -> std::io::Result<()> {
    let resolved = SessionPath::resolve(path)?;
    resolved.ensure_replaceable(resolved.target())?;
    let target = resolved.target();
    match std::fs::remove_file(target) {
        Ok(()) => shepr_platform::sync_directory(containing_directory(target)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// What reading the saved session found.
pub enum SessionLoad {
    /// No session file: a fresh start.
    Missing,
    Loaded {
        snapshot: SessionSnapshot,
        /// The digest of the history file this layout pairs with; `None`
        /// when it was saved without history.
        history_digest: Option<HistoryDigest>,
    },
    /// A session file exists but could not be read or parsed; the reason.
    /// Nothing of it is restored, and the first save backs it up before
    /// replacing it.
    Unusable(shepr_protocol::SessionRestoreFailure),
}

impl SessionLoad {
    #[must_use]
    pub fn into_snapshot(self) -> Option<SessionSnapshot> {
        match self {
            Self::Loaded { snapshot, .. } => Some(snapshot),
            Self::Missing | Self::Unusable(_) => None,
        }
    }
}

/// Refuses a session path that holds something other than a regular file (a
/// directory, a FIFO, a socket, a device). No save could ever replace it, so a
/// server that started anyway would run panes whose layout can never be
/// saved; the server refuses to start instead, before anything is restored.
/// Symlinks are followed, as saves follow them. A history path in the same
/// state only costs history and is left to the saves.
pub fn check_session_target(lease: &DataDirLease) -> std::io::Result<()> {
    let path = session_path(lease.directory());
    // Use the shared resolver so startup and later saves apply the same
    // symlink hop limit, including dangling links.
    let resolved = SessionPath::resolve(&path)?;
    resolved.ensure_replaceable(resolved.target())
}

/// The directory a session file is backed up to before a save replaces one
/// that restore could not fully use.
#[must_use]
pub fn session_backup_directory(data_dir: &Path) -> PathBuf {
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
            let failure = if let Some(kind) = session_file_kind(&err) {
                shepr_protocol::SessionRestoreFailure::NotRegularFile {
                    kind,
                    detail: err.to_string(),
                }
            } else if let Some(limit_bytes) = session_file_size_limit(&err) {
                shepr_protocol::SessionRestoreFailure::TooLarge { limit_bytes }
            } else {
                shepr_protocol::SessionRestoreFailure::Unreadable {
                    kind: session_io_error_kind(err.kind()),
                    detail: err.to_string(),
                }
            };
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), error = %err, "failed to read session file"
            );
            return SessionLoad::Unusable(failure);
        }
    };
    match parse_session_file(&content) {
        Ok(file) => SessionLoad::Loaded {
            snapshot: file.snapshot,
            history_digest: file.history_digest,
        },
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), error = %err, "failed to parse session file, ignoring"
            );
            SessionLoad::Unusable(session_parse_failure(&err))
        }
    }
}

/// Reads the history file only when the layout names one (`expected_digest`)
/// and only if its bytes hash to exactly that digest: then it is the history
/// that layout's own save serialized, every pane under the key it was saved
/// with. The bytes are read once, and the ones hashed are the ones parsed. A
/// matching digest is no exemption from parsing and version checks. It binds
/// the buffer, not the file: a rewrite in place during the read yields bytes
/// that simply fail to match.
pub fn load_history(
    lease: &DataDirLease,
    expected_digest: Option<&HistoryDigest>,
) -> Option<SessionHistorySnapshot> {
    let expected_digest = expected_digest?;
    let path = session_history_path(lease.directory());
    let content = match read_history_file(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "history_missing",
                path = %path.display(), "the saved layout names a history file that is missing"
            );
            return None;
        }
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), error = %err, "failed to read session history file"
            );
            return None;
        }
    };
    if history_digest(&content) != *expected_digest {
        warn!(
            event = "persist.restore", subsystem = "persist", outcome = "history_mismatch",
            path = %path.display(),
            "ignoring a session history file that is not the one the saved layout names"
        );
        return None;
    }
    let content = match String::from_utf8(content) {
        Ok(content) => content,
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), error = %err, "session history file is not UTF-8, ignoring"
            );
            return None;
        }
    };
    match parse_history_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), error = %err, "failed to parse session history file, ignoring"
            );
            None
        }
    }
}

fn session_parse_failure(error: &serde_json::Error) -> shepr_protocol::SessionRestoreFailure {
    use shepr_protocol::SessionParseCategory;

    let category = match error.classify() {
        serde_json::error::Category::Io => SessionParseCategory::Io,
        serde_json::error::Category::Syntax => SessionParseCategory::Syntax,
        serde_json::error::Category::Data => SessionParseCategory::Data,
        serde_json::error::Category::Eof => SessionParseCategory::Eof,
    };
    shepr_protocol::SessionRestoreFailure::Unparseable {
        line: error.line(),
        column: error.column(),
        category,
        detail: error.to_string(),
    }
}

#[cfg(test)]
fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    Ok(SessionPath::resolve(path)?.target)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::persist::history::HistoryText;
    use crate::persist::schema::SNAPSHOT_VERSION;
    use shepr_protocol::PanePublicNumber;

    #[test]
    fn a_session_path_still_a_symlink_after_the_hop_limit_is_refused() {
        let scratch = shepr_test_support::ScratchDir::new("session-symlink-loop");
        let first = scratch.path().join("first.json");
        let second = scratch.path().join("second.json");
        std::os::unix::fs::symlink(&second, &first).expect("test precondition");
        std::os::unix::fs::symlink(&first, &second).expect("test precondition");

        let error = resolve_write_target(&first).expect_err("a symlink loop is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
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
    }

    /// A session file whose data directory does not exist yet, so saves
    /// exercise creating it.
    fn temp_session_path(name: &str) -> PathBuf {
        crate::test_support::ScratchDir::new(name)
            .join("data")
            .join("session.json")
    }

    fn temp_session_paths(name: &str) -> (PathBuf, PathBuf) {
        let session = temp_session_path(name);
        let history = session_history_path(containing_directory(&session));
        (session, history)
    }

    fn empty_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![],
            active: None,
        }
    }

    fn number(value: usize) -> PanePublicNumber {
        PanePublicNumber::new(value).expect("nonzero literal")
    }

    /// A history with one workspace holding one pane's text.
    fn history_with(text: &str) -> SessionHistory {
        SessionHistory {
            version: SNAPSHOT_VERSION,
            workspaces: vec![vec![(number(1), HistoryText::single(Arc::from(text)))]],
        }
    }

    #[test]
    fn reacquiring_after_release_loads_the_existing_session() {
        let scratch = crate::test_support::ScratchDir::new("released-session-lease");
        let lease = DataDirLease::acquire(&scratch).expect("lease");
        save_to_path(&session_path(lease.directory()), &empty_snapshot(), None).expect("save");
        assert!(matches!(load(&lease), SessionLoad::Loaded { .. }));
        lease.release();
        let lease = DataDirLease::acquire(&scratch).expect("lease after release");
        assert!(matches!(load(&lease), SessionLoad::Loaded { .. }));
        let digest = history_digest(b"any");
        assert!(load_history(&lease, Some(&digest)).is_none());
    }

    #[test]
    fn history_is_restored_only_for_the_digest_its_layout_names() {
        let scratch = crate::test_support::ScratchDir::new("history-digest-pairing");
        let lease = DataDirLease::acquire(&scratch).expect("lease");
        let history = history_with("saved scrollback\r\n");
        let json = serialize_history(&history).expect("serialize").json;
        save_history_json_to_path(&session_history_path(lease.directory()), &json)
            .expect("write history");
        let digest = history_digest(&json);
        save_to_path(
            &session_path(lease.directory()),
            &empty_snapshot(),
            Some(&digest),
        )
        .expect("save layout");

        let SessionLoad::Loaded {
            history_digest: named,
            ..
        } = load(&lease)
        else {
            panic!("the layout loads");
        };
        assert_eq!(named, Some(digest));
        let restored = load_history(&lease, named.as_ref()).expect("paired history");
        assert_eq!(
            restored.workspaces[0].panes[&number(1)].ansi,
            "saved scrollback\r\n"
        );

        // Any other history in the file, a layout naming none, or a digest
        // for other bytes restores nothing.
        assert!(load_history(&lease, None).is_none());
        let other_digest = history_digest(b"other");
        assert!(load_history(&lease, Some(&other_digest)).is_none());
        let other = serialize_history(&history_with("swapped\r\n"))
            .expect("serialize")
            .json;
        save_history_json_to_path(&session_history_path(lease.directory()), &other)
            .expect("write history");
        assert!(load_history(&lease, Some(&digest)).is_none());
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
        assert!(is_not_regular(
            &save_to_path(&session, &empty_snapshot(), None).expect_err("refused")
        ));
        assert!(is_not_regular(&clear_path(&session).expect_err("refused")));
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
        assert!(matches!(
            failure,
            shepr_protocol::SessionRestoreFailure::Unparseable { .. }
        ));
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_path(&session_path, &empty_snapshot(), None).expect("test precondition");
        save_history_to_path(&history_path, Some(&history_with("split-secret")))
            .expect("test precondition");

        let session = std::fs::read_to_string(&session_path).expect("test precondition");
        let history = std::fs::read_to_string(&history_path).expect("test precondition");
        assert!(!session.contains("split-secret"));
        assert!(
            parse_session_file(&session)
                .expect("test precondition")
                .history_digest
                .is_none()
        );
        assert!(history.contains("split-secret"));
    }

    #[test]
    fn save_to_paths_removes_stale_history_when_history_is_disabled() {
        let (session_path, history_path) = temp_session_paths("clear-history");
        save_to_path(&session_path, &empty_snapshot(), None).expect("test precondition");
        save_history_to_path(&history_path, Some(&history_with("stale-secret")))
            .expect("test precondition");

        save_history_to_path(&history_path, None).expect("test precondition");

        assert!(session_path.try_exists().expect("test stat"));
        assert!(!history_path.try_exists().expect("test stat"));
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot(), None).expect("test precondition");

        clear_path(&path).expect("test precondition");

        assert!(!path.try_exists().expect("test stat"));
    }

    #[test]
    fn clear_path_ignores_missing_session_file() {
        let path = temp_session_path("clear-missing");

        clear_path(&path).expect("test precondition");

        assert!(!path.try_exists().expect("test stat"));
    }

    #[test]
    fn save_to_path_preserves_existing_symlink() {
        let target = temp_session_path("symlink-target");
        let link = target.with_file_name("link.json");
        save_to_path(&target, &empty_snapshot(), None).expect("test precondition");
        std::os::unix::fs::symlink(&target, &link).expect("test precondition");

        let mut snap = empty_snapshot();
        snap.active = Some(7);
        save_to_path(&link, &snap, None).expect("test precondition");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        let parsed =
            parse_session_file(&std::fs::read_to_string(&target).expect("test precondition"))
                .expect("test precondition");
        assert_eq!(parsed.snapshot.active, Some(7));
    }

    #[test]
    fn save_to_path_writes_through_dangling_symlink() {
        let target = temp_session_path("dangling-target");
        let link = target.with_file_name("link.json");
        std::fs::create_dir_all(target.parent().expect("test precondition"))
            .expect("test precondition");
        std::os::unix::fs::symlink(&target, &link).expect("test precondition");

        save_to_path(&link, &empty_snapshot(), None).expect("test precondition");

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
    fn saved_session_and_history_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let data_dir = crate::test_support::ScratchDir::new("private-mode").join("data");
        let session = session_path(&data_dir);
        let history = session_history_path(&data_dir);
        save_to_path(&session, &empty_snapshot(), None).expect("create private session directory");
        // Publishing renames a fresh private file over the target, so an
        // existing file with a broader mode is replaced, not reused.
        std::fs::write(&history, b"old").expect("test precondition");
        std::fs::set_permissions(&history, std::fs::Permissions::from_mode(0o644))
            .expect("test precondition");

        save_to_path(&session, &empty_snapshot(), None).expect("test precondition");
        save_history_to_path(&history, Some(&history_with("private-secret")))
            .expect("test precondition");

        for path in [&session, &history] {
            let mode = std::fs::metadata(path)
                .expect("test precondition")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
        }
        assert_eq!(
            entry_names(&data_dir),
            ["session-history.json", "session.json"],
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
        save_to_path(&path, &snap, None).expect("test precondition");

        let parsed =
            parse_session_file(&std::fs::read_to_string(&path).expect("test precondition"))
                .expect("test precondition");
        assert_eq!(parsed.snapshot.active, Some(3));
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
        save_to_path(&link, &empty_snapshot(), None).expect("test precondition");
        assert!(target.try_exists().expect("test stat"));

        clear_path(&link).expect("test precondition");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        assert!(!target.try_exists().expect("test stat"));
        // Clearing again with the link dangling is a no-op.
        clear_path(&link).expect("test precondition");
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
        assert_eq!(
            resolved
                .expect_err("an unreadable path is not absent")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            cleared.expect_err("a clear must not guess").kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn save_to_path_resolves_relative_symlink() {
        let session = temp_session_path("relative-symlink");
        let dir = session.parent().expect("test precondition");
        std::fs::create_dir_all(dir).expect("test precondition");
        let target = dir.join("real.json");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).expect("test precondition");

        save_to_path(&link, &empty_snapshot(), None).expect("test precondition");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        assert!(target.try_exists().expect("test stat"));
    }
}
