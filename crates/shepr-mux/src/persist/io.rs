use crate::limits::{
    MAX_SESSION_FILE_BYTES, MAX_SESSION_HISTORY_FILE_BYTES, MAX_SESSION_PATH_SYMLINK_HOPS,
};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::ser::{Formatter, PrettyFormatter};
use tracing::warn;

use super::lock::DataDirLease;
use super::snapshot::{
    HistoryText, SessionFile, SessionHistory, SessionHistorySnapshot, SessionSnapshot,
    parse_history_snapshot, parse_session_file,
};

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

fn ensure_history_size(size: usize) -> std::io::Result<()> {
    if size > MAX_SESSION_HISTORY_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("session history file exceeds {MAX_SESSION_HISTORY_FILE_BYTES} bytes"),
        ));
    }
    Ok(())
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

/// SHA-256 of a history file's bytes: how a layout names the history it pairs
/// with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryDigest([u8; 32]);

impl HistoryDigest {
    pub(super) fn from_bytes(bytes: &[u8]) -> Self {
        Self(sha256_bytes(bytes))
    }

    pub(super) fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        // limits-exempt: a SHA-256 digest is 32 bytes by the hash definition.
        let mut bytes = [0; 32];
        for (index, [high, low]) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            bytes[index] = (hex_digit(*high)? << 4) | hex_digit(*low)?;
        }
        Some(Self(bytes))
    }

    pub(super) fn to_hex(self) -> String {
        encode_sha256(&self.0)
    }
}

impl serde::Serialize for HistoryDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for HistoryDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let hex = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::from_hex(&hex)
            .ok_or_else(|| serde::de::Error::custom("expected a 64-digit SHA-256 hex digest"))
    }
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    Sha256::digest(bytes).into()
}

fn encode_sha256(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(digest.len() * 2);
    for &byte in digest {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 15)]));
    }
    hex
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The SHA-256 of a history file's bytes: how a layout names the history it
/// pairs with.
pub(super) fn history_digest(json: &[u8]) -> HistoryDigest {
    HistoryDigest::from_bytes(json)
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

/// Publishes `source` at `target` through a private (0600) temporary at
/// `pending`: write, fsync the file, publish, fsync the directory. A crash
/// leaves either the previous file or the complete new one, never a truncated
/// one. Before creating the temporary, a leftover file from an interrupted
/// publish is removed. The caller must hold the data directory lease so this
/// cannot remove another live writer's temporary.
///
/// With `replace` false an existing `target` is atomically refused with `AlreadyExists`,
/// and a published target is withdrawn again when the directory sync fails,
/// so that mode only ever returns `Published::Durable` or an error.
/// With `replace` true the target is overwritten, and a completed rename is
/// kept even when the directory sync then reports an error; that comes back
/// as `Published::NotDurable`.
///
/// Both the live session files and the recovery copies go through here, so
/// they share one durability and permission policy. Session history can hold
/// full pane scrollback up to its file-size limit and can include tokens, so
/// nothing here may be group- or world-readable.
pub(super) fn publish_private_file(
    source: &mut impl std::io::Read,
    pending: &Path,
    target: &Path,
    replace: bool,
) -> std::io::Result<Published> {
    use shepr_platform::publish_file::{Durability, PreparedFile, PublishOptions};
    remove_stale_temporary(pending)?;
    PreparedFile::prepare_at(
        target,
        pending,
        source,
        &PublishOptions {
            preserve_metadata_from: None,
            refuse_symlink_target: false,
            durability: if replace {
                Durability::Directory
            } else {
                Durability::DirectoryOrWithdraw
            },
            replace,
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

// A crash between creating the temporary and renaming it leaves the file
// behind, and the exclusive create in `publish_private_file` would then refuse
// every later save. Unlinking a directory fails, so anything other than a
// leftover file in the way still fails the save. Removing it is safe because
// only one server writes a data directory: `SessionWriter` owns the
// directory lease (`lock.rs`) before any write.
fn remove_stale_temporary(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
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
    let pending = target.with_extension("json.tmp");
    let mut source = json;
    let published = publish_private_file(&mut source, &pending, target, true)?;
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

/// A history serialized to fit the file cap, and what was cut to make it fit.
pub(super) struct SerializedHistory {
    pub(super) json: Vec<u8>,
    pub(super) trimmed: Option<HistoryTrim>,
}

/// What `serialize_history` left out of an oversized history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HistoryTrim {
    /// Panes that lost lines, including any that kept none.
    pub(super) panes: usize,
    /// Serialized bytes of pane history left out.
    pub(super) dropped_bytes: usize,
    /// Whether oversized workspace structure had to be omitted.
    pub(super) structure_dropped: bool,
}

/// Serializes the history, trimming the oldest scrollback lines when the
/// whole file would exceed `MAX_SESSION_HISTORY_FILE_BYTES`.
///
/// The cap is on the file, but what fills it is per-pane scrollback, and
/// neither bound can be derived from the other: `advanced.scrollback_limit_bytes`
/// sizes each pane's in-memory cell grid (converted to a line count, at
/// least 1000 lines), not the formatted ANSI, whose size depends on styling
/// and JSON escaping, and the pane count is unbounded at runtime. So an
/// oversized history is not an error to retry. Refusing it instead would
/// turn every later save into a failed one: the save loop backs off and
/// retries, reformatting all scrollback each time, and the stale history
/// left on disk would still be restored whenever the layout had not
/// changed. Scrollback already drops its oldest lines at its limit, so
/// this does the same across panes: every pane may keep an equal share of
/// the room, a pane under its share keeps everything and leaves the rest to
/// the others, and a pane over it keeps its most recent whole lines. The
/// formatter closes all SGR and OSC 8 state at each line break, so a cut
/// there replays cleanly.
///
/// Nothing here assembles a pane's text or builds a serialized copy of the
/// history beyond the file's bytes: the JSON is written straight from the
/// pieces the pane's history cache holds, the trim works over those pieces
/// (dropping whole oldest pieces, then cutting inside the first one kept),
/// and what is written is never more than `cap` bytes plus one write.
pub(super) fn serialize_history(history: &SessionHistory) -> std::io::Result<SerializedHistory> {
    serialize_history_within(history, MAX_SESSION_HISTORY_FILE_BYTES)
}

fn serialize_history_within(
    history: &SessionHistory,
    cap: usize,
) -> std::io::Result<SerializedHistory> {
    let mut whole = CappedBuf::new(cap);
    write_history_json(&mut whole, history, Shape::Whole)?;
    if whole.len <= cap {
        return Ok(SerializedHistory {
            json: whole.bytes,
            trimmed: None,
        });
    }
    let total = whole.len;
    drop(whole);

    let texts: Vec<&HistoryText> = history
        .workspaces
        .iter()
        .flatten()
        .map(|(_, text)| text)
        .collect();
    let sizes: Vec<usize> = texts
        .iter()
        .map(|text| escaped_len_from(text, Cut::START))
        .collect();
    let content: usize = sizes.iter().sum();
    let room = total
        .checked_sub(content)
        .and_then(|structure| cap.checked_sub(structure));
    let Some(room) = room else {
        return compact_history_without_workspace_shape(history, cap, content);
    };
    let Some(share) = fair_share(sizes.clone(), room) else {
        return compact_history_without_workspace_shape(history, cap, content);
    };

    let mut trim = HistoryTrim {
        panes: 0,
        dropped_bytes: 0,
        structure_dropped: false,
    };
    let mut cuts = Vec::with_capacity(texts.len());
    for (text, size) in texts.iter().zip(&sizes) {
        let cut = recent_cut(text, share);
        let kept = escaped_len_from(text, cut);
        if kept < *size {
            trim.panes += 1;
            trim.dropped_bytes += *size - kept;
        }
        cuts.push(cut);
    }
    let mut trimmed = CappedBuf::new(cap);
    write_history_json(&mut trimmed, history, Shape::Cut(&cuts))?;
    if trimmed.len > cap {
        return compact_history_without_workspace_shape(history, cap, content);
    }
    Ok(SerializedHistory {
        json: trimmed.bytes,
        trimmed: Some(trim),
    })
}

fn compact_history_without_workspace_shape(
    history: &SessionHistory,
    cap: usize,
    dropped_bytes: usize,
) -> std::io::Result<SerializedHistory> {
    let mut compact = CappedBuf::new(cap);
    write_history_json(&mut compact, history, Shape::Compact)?;
    if compact.len > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("minimal session history exceeds {cap} bytes"),
        ));
    }
    let trim = HistoryTrim {
        panes: history
            .workspaces
            .iter()
            .flatten()
            .filter(|(_, text)| text.pieces.iter().any(|piece| !piece.text.is_empty()))
            .count(),
        dropped_bytes,
        structure_dropped: true,
    };
    Ok(SerializedHistory {
        json: compact.bytes,
        trimmed: Some(trim),
    })
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

/// How much of the history a write includes.
#[derive(Clone, Copy)]
enum Shape<'a> {
    /// Every pane whole.
    Whole,
    /// Each pane from a cut, in workspace and pane order. A pane whose cut
    /// keeps nothing is written as an empty string, never left out.
    Cut(&'a [Cut]),
    /// No workspaces: a history that restores no text.
    Compact,
}

/// Writes `history` as the JSON `serde_json::to_string_pretty` gives its
/// [`SessionHistorySnapshot`] form, driving the same pretty formatter, but
/// with each pane's text taken from its pieces instead of one string.
fn write_history_json<W: Write>(
    out: &mut W,
    history: &SessionHistory,
    shape: Shape<'_>,
) -> std::io::Result<()> {
    let mut fmt = PrettyFormatter::new();
    fmt.begin_object(out)?;
    write_key(out, &mut fmt, true, "version")?;
    serde_json::to_writer(&mut *out, &history.version)?;
    fmt.end_object_value(out)?;
    write_key(out, &mut fmt, false, "workspaces")?;
    fmt.begin_array(out)?;
    let workspaces: &[Vec<(u32, HistoryText)>] = match shape {
        Shape::Compact => &[],
        Shape::Whole | Shape::Cut(_) => &history.workspaces,
    };
    // Which pane, across the whole history, `Shape::Cut` speaks of next.
    let mut pane_index = 0usize;
    for (workspace_index, panes) in workspaces.iter().enumerate() {
        fmt.begin_array_value(out, workspace_index == 0)?;
        fmt.begin_object(out)?;
        write_key(out, &mut fmt, true, "panes")?;
        fmt.begin_object(out)?;
        let mut first = true;
        for (id, text) in panes {
            let cut = match shape {
                Shape::Cut(cuts) => {
                    let cut = cuts.get(pane_index).copied();
                    pane_index += 1;
                    cut
                }
                Shape::Whole | Shape::Compact => Some(Cut::START),
            };
            let Some(cut) = cut else {
                continue;
            };
            fmt.begin_object_key(out, first)?;
            write!(out, "\"{id}\"")?;
            fmt.end_object_key(out)?;
            fmt.begin_object_value(out)?;
            fmt.begin_object(out)?;
            write_key(out, &mut fmt, true, "ansi")?;
            write_text_from(out, text, cut)?;
            fmt.end_object_value(out)?;
            fmt.end_object(out)?;
            fmt.end_object_value(out)?;
            first = false;
        }
        fmt.end_object(out)?;
        fmt.end_object_value(out)?;
        fmt.end_object(out)?;
        fmt.end_array_value(out)?;
    }
    fmt.end_array(out)?;
    fmt.end_object_value(out)?;
    fmt.end_object(out)
}

/// The start of an object field: its key, up to where its value goes.
fn write_key<W: Write>(
    out: &mut W,
    fmt: &mut PrettyFormatter<'_>,
    first: bool,
    key: &str,
) -> std::io::Result<()> {
    fmt.begin_object_key(out, first)?;
    serde_json::to_writer(&mut *out, key)?;
    fmt.end_object_key(out)?;
    fmt.begin_object_value(out)
}

/// A place in a pane's history text: `offset` bytes into piece `piece`. The
/// text of a trimmed pane is what follows its cut, so a cut is only ever put
/// at the start of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cut {
    piece: usize,
    offset: usize,
}

impl Cut {
    /// The whole text.
    const START: Self = Self {
        piece: 0,
        offset: 0,
    };
}

/// JSON-escaped size of the `\r\n` between two pieces.
// limits-exempt: the JSON escape of CR LF is four bytes by the JSON format.
const BREAK_ESCAPED_LEN: usize = 4;

/// The text of piece `index` from `cut`, which is all of it unless the cut
/// is inside it.
fn piece_from(text: &HistoryText, index: usize, cut: Cut) -> &str {
    let piece = &*text.pieces[index].text;
    if index == cut.piece {
        piece.get(cut.offset..).unwrap_or_default()
    } else {
        piece
    }
}

/// Writes the text from `cut` on as a JSON string.
fn write_text_from<W: Write>(out: &mut W, text: &HistoryText, cut: Cut) -> std::io::Result<()> {
    out.write_all(b"\"")?;
    for index in cut.piece..text.pieces.len() {
        if index > cut.piece && text.pieces[index].break_before {
            out.write_all(b"\\r\\n")?;
        }
        write_escaped(out, piece_from(text, index, cut))?;
    }
    out.write_all(b"\"")
}

/// Bytes the text from `cut` on occupies inside a JSON string literal, quotes
/// excluded. Escaping is per character, so the sizes of adjacent pieces add
/// up.
fn escaped_len_from(text: &HistoryText, cut: Cut) -> usize {
    let mut len = 0;
    for index in cut.piece..text.pieces.len() {
        if index > cut.piece && text.pieces[index].break_before {
            len += BREAK_ESCAPED_LEN;
        }
        len += escaped_len(piece_from(text, index, cut));
    }
    len
}

/// Bytes `text` occupies inside a JSON string literal, quotes excluded. The
/// escapes are `serde_json`'s: quote, backslash, `\b`, `\t`, `\n`, `\f` and
/// `\r` by letter, the other control characters as `\u00xx`, everything else
/// (multi-byte characters included) as it is.
fn escaped_len(text: &str) -> usize {
    text.bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | 8 | 9 | 10 | 12 | 13 => 2,
            0..=31 => 6,
            _ => 1,
        })
        .sum()
}

/// Writes `text` escaped as [`escaped_len`] counts it.
fn write_escaped<W: Write>(out: &mut W, text: &str) -> std::io::Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = text.as_bytes();
    // Where the run of bytes that need no escape, ending here, began.
    let mut run = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        let short: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            8 => b"\\b",
            9 => b"\\t",
            10 => b"\\n",
            12 => b"\\f",
            13 => b"\\r",
            0..=31 => b"",
            _ => continue,
        };
        out.write_all(&bytes[run..index])?;
        run = index + 1;
        if short.is_empty() {
            out.write_all(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[usize::from(byte >> 4)],
                HEX[usize::from(byte & 15)],
            ])?;
        } else {
            out.write_all(short)?;
        }
    }
    out.write_all(&bytes[run..])
}

/// The largest per-pane size such that every pane capped at it fits `room`
/// in total. Panes smaller than an equal split keep everything, and what they
/// leave unused is shared among the rest.
fn fair_share(mut sizes: Vec<usize>, room: usize) -> Option<usize> {
    sizes.sort_unstable();
    let count = sizes.len();
    let mut remaining = room;
    for (index, size) in sizes.into_iter().enumerate() {
        let share = remaining / (count - index);
        if size > share {
            return Some(share);
        }
        remaining -= size;
    }
    None
}

/// The earliest line start in `text` from which the rest, JSON-escaped, is at
/// most `limit` bytes: what a pane over its share keeps is the whole lines
/// from there on. A line ends with its `\n`; the text between two pieces is a
/// line break when the later piece says so and otherwise is no break at all,
/// so a line may run through several pieces, and one too big for `limit` is
/// dropped whole. Can be the end of the text, which keeps nothing.
///
/// Works piece by piece from the newest: a piece that lies wholly before the
/// cut is never looked at again, and the walk stops at the first line start
/// that is too far back. `\n` never occurs inside a multi-byte character, so
/// every cut lands on a character boundary.
fn recent_cut(text: &HistoryText, limit: usize) -> Cut {
    let mut best = Cut {
        piece: text.pieces.len(),
        offset: 0,
    };
    // Escaped size of everything after the piece being walked, the line
    // breaks in front of those pieces included.
    let mut after = 0usize;
    for (index, piece) in text.pieces.iter().enumerate().rev() {
        if after > limit {
            return best;
        }
        let bytes = piece.text.as_bytes();
        let mut end = bytes.len();
        // Escaped size of the text from `end` on.
        let mut kept = after;
        if end > 0 && bytes[end - 1] == b'\n' {
            // The piece ends a line: its end is itself a line start.
            best = Cut {
                piece: index,
                offset: end,
            };
        } else if end == 0 && (index == 0 || piece.break_before) {
            // An empty piece is a line start where a break is in front of it.
            best = Cut {
                piece: index,
                offset: 0,
            };
        }
        while end > 0 {
            // The line ending at `end` begins after the newline before its
            // own terminator.
            let start = bytes[..end - 1]
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(0, |newline| newline + 1);
            kept += escaped_len(piece.text.get(start..end).unwrap_or_default());
            if kept > limit {
                return best;
            }
            if start > 0 || index == 0 || piece.break_before {
                // A line starts here: at the piece start only when a break
                // (or the start of the text) is in front of it.
                best = Cut {
                    piece: index,
                    offset: start,
                };
            }
            end = start;
        }
        after = kept
            + if piece.break_before {
                BREAK_ESCAPED_LEN
            } else {
                0
            };
    }
    best
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
    use crate::pane::HistoryPiece;
    use crate::persist::snapshot::SNAPSHOT_VERSION;

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
        let history = history_with_panes(&[(0, "saved scrollback\r\n")]);
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
            restored.workspaces[0].panes[&0].ansi,
            "saved scrollback\r\n"
        );

        // Any other history in the file, a layout naming none, or a digest
        // for other bytes restores nothing.
        assert!(load_history(&lease, None).is_none());
        let other_digest = history_digest_of("other");
        assert!(load_history(&lease, Some(&other_digest)).is_none());
        let other = serialize_history(&history_with_panes(&[(0, "swapped\r\n")]))
            .expect("serialize")
            .json;
        save_history_json_to_path(&session_history_path(lease.directory()), &other)
            .expect("write history");
        assert!(load_history(&lease, Some(&digest)).is_none());
    }

    fn history_digest_of(text: &str) -> HistoryDigest {
        history_digest(text.as_bytes())
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

    /// The reader admits the limit itself and refuses one byte more. The
    /// writer serializes and trims to this same file-size budget.
    #[test]
    fn history_size_limit_admits_the_limit_and_refuses_one_byte_more() {
        ensure_history_size(MAX_SESSION_HISTORY_FILE_BYTES).expect("the limit itself is allowed");
        let error = ensure_history_size(MAX_SESSION_HISTORY_FILE_BYTES + 1)
            .expect_err("one byte over is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// The history as `serde_json` serializes its read-side form.
    fn reference_json(history: &SessionHistory) -> Vec<u8> {
        serde_json::to_vec_pretty(&history.clone().into_snapshot()).expect("serialize")
    }

    fn json_text(serialized: &SerializedHistory) -> &str {
        std::str::from_utf8(&serialized.json).expect("history json is utf-8")
    }

    #[test]
    fn history_with_oversized_workspace_shape_is_saved_without_failing() {
        let mut history = history_with_panes(&[(0, "kept only when the shape fits\r\n")]);
        history
            .workspaces
            .extend((0..40).map(|_| Vec::<(u32, HistoryText)>::new()));
        let compact = SessionHistorySnapshot {
            version: history.version,
            workspaces: Vec::new(),
        };
        let compact_json = serde_json::to_vec_pretty(&compact).expect("serialize");
        let full = reference_json(&history);
        assert!(full.len() > compact_json.len());

        let serialized =
            serialize_history_within(&history, compact_json.len()).expect("compact to fit");

        assert_eq!(serialized.json, compact_json);
        let trim = serialized.trimmed.expect("omitted history is reported");
        assert!(trim.structure_dropped);
        assert_eq!(trim.panes, 1);
        assert!(trim.dropped_bytes > 0);
        let restored = parse_history_snapshot(json_text(&serialized)).expect("compact parses");
        assert!(restored.workspaces.is_empty());
    }

    fn single(text: &str) -> HistoryText {
        HistoryText::single(Arc::from(text))
    }

    fn history_snapshot(secret: &str) -> SessionHistory {
        SessionHistory {
            version: SNAPSHOT_VERSION,
            workspaces: vec![vec![(0, single(secret))]],
        }
    }

    fn history_with_panes(panes: &[(u32, &str)]) -> SessionHistory {
        history_with_texts(panes.iter().map(|(id, ansi)| (*id, single(ansi))).collect())
    }

    fn history_with_texts(panes: Vec<(u32, HistoryText)>) -> SessionHistory {
        SessionHistory {
            version: SNAPSHOT_VERSION,
            workspaces: vec![panes],
        }
    }

    fn numbered_lines(prefix: &str, count: usize) -> String {
        (0..count)
            .map(|line| format!("\x1b[0;1m{prefix}-{line:04}\x1b[0m\r\n"))
            .collect()
    }

    #[test]
    fn history_under_the_cap_is_saved_whole() {
        let history = history_with_panes(&[(1, "one\r\n"), (2, "two\r\n")]);
        let serialized = serialize_history(&history).expect("serialize");
        assert_eq!(serialized.trimmed, None);
        assert_eq!(serialized.json, reference_json(&history));
    }

    /// Texts with everything JSON escapes, in pieces of every kind (a line
    /// break before, a continued line), across several workspaces and
    /// panes (one empty), serialize to the bytes `serde_json` gives the same
    /// history as one string per pane.
    #[test]
    fn serializing_from_pieces_matches_serde_json_of_the_assembled_history() {
        let piece = |text: &str, break_before| HistoryPiece {
            text: Arc::from(text),
            break_before,
        };
        let mixed = HistoryText {
            pieces: vec![
                piece("plain \"quoted\" back\\slash\x08\x0c\t", false),
                piece("\x1b[1mline two\x1b[0m\x01\x1f\x7f", true),
                piece("continued \u{e9}\u{4e16}\u{1f600}\r\nnext", false),
                piece("after a break\n", true),
                piece("no break\r\n", false),
            ],
        };
        let history = SessionHistory {
            version: SNAPSHOT_VERSION,
            workspaces: vec![
                vec![(3, single("three")), (12, mixed.clone())],
                vec![],
                vec![(1, single("")), (2, mixed)],
                vec![(0, single("solo\r\n"))],
            ],
        };
        let serialized = serialize_history(&history).expect("serialize");
        assert_eq!(serialized.trimmed, None);
        assert_eq!(serialized.json, reference_json(&history));
    }

    #[test]
    fn escaping_matches_serde_json_character_for_character() {
        let characters = (0u8..128)
            .map(char::from)
            .chain(['\u{e9}', '\u{4e16}', '\u{1f600}']);
        for character in characters {
            let text = character.to_string();
            let mut written = Vec::new();
            write_escaped(&mut written, &text).expect("write");
            let json = serde_json::to_string(&text).expect("serialize");
            let inside = &json[1..json.len() - 1];
            assert_eq!(written, inside.as_bytes(), "{character:?}");
            assert_eq!(escaped_len(&text), inside.len(), "{character:?}");
        }
    }

    /// An oversized history is trimmed to fit, not refused: every pane keeps
    /// its most recent whole lines, a small pane keeps everything, and the
    /// result still parses.
    #[test]
    fn history_over_the_cap_keeps_the_most_recent_lines_of_each_pane() {
        let small = "small pane\r\n";
        let big_a = numbered_lines("a", 400);
        let big_b = numbered_lines("b", 400);
        let history = history_with_panes(&[(1, small), (2, &big_a), (3, &big_b)]);
        let full = reference_json(&history);
        let cap = full.len() / 2;

        let serialized = serialize_history_within(&history, cap).expect("trimmed to fit");
        assert!(
            serialized.json.len() <= cap,
            "{} > {cap}",
            serialized.json.len()
        );
        let trim = serialized.trimmed.expect("trimming is reported");
        assert_eq!(trim.panes, 2);
        assert!(trim.dropped_bytes >= full.len() - cap, "{trim:?}");

        let restored = parse_history_snapshot(json_text(&serialized)).expect("trimmed parses");
        let panes = &restored.workspaces[0].panes;
        assert_eq!(panes[&1].ansi, small);
        for (id, prefix, full) in [(2, "a", &big_a), (3, "b", &big_b)] {
            let kept = &panes[&id].ansi;
            assert!(full.ends_with(kept.as_str()), "pane {id} kept a suffix");
            assert!(kept.len() < full.len(), "pane {id} was trimmed");
            assert!(
                kept.starts_with("\x1b[0;1m"),
                "pane {id} was cut at a line start"
            );
            assert!(kept.ends_with(&format!("{prefix}-0399\x1b[0m\r\n")));
        }
    }

    /// The text from `cut` on, as one string.
    fn assemble_from(text: &HistoryText, cut: Cut) -> String {
        let mut out = String::new();
        for index in cut.piece..text.pieces.len() {
            if index > cut.piece && text.pieces[index].break_before {
                out.push_str("\r\n");
            }
            out.push_str(piece_from(text, index, cut));
        }
        out
    }

    /// Cuts `ansi` the way trimming did when a pane's text was one string.
    fn reference_recent_lines(ansi: &str, limit: usize) -> &str {
        let bytes = ansi.as_bytes();
        let mut start = bytes.len();
        let mut kept = 0usize;
        while start > 0 {
            let line_start = bytes[..start - 1]
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(0, |newline| newline + 1);
            kept += escaped_len(ansi.get(line_start..start).unwrap_or_default());
            if kept > limit {
                break;
            }
            start = line_start;
        }
        ansi.get(start..).unwrap_or_default()
    }

    #[test]
    fn recent_cut_counts_json_escaping_and_cuts_only_at_line_starts() {
        // ESC escapes to six bytes, CR and LF to two each, and multi-byte
        // characters are written as they are.
        let ansi = "old\r\n\x1b[1mnew\u{e9}\r\nprompt \u{e9}";
        let text = single(ansi);
        assert_eq!(escaped_len("\x1b[1mnew\u{e9}\r\n"), 18);
        let prompt = escaped_len("prompt \u{e9}");
        let kept = |limit| assemble_from(&text, recent_cut(&text, limit));
        assert_eq!(kept(prompt - 1), "");
        assert_eq!(kept(prompt), "prompt \u{e9}");
        assert_eq!(kept(prompt + 18), "\x1b[1mnew\u{e9}\r\nprompt \u{e9}");
        assert_eq!(kept(usize::MAX), ansi);
    }

    /// `lines` as pieces: lines are gathered `group` at a time into a piece
    /// (a line break before each but the first), and each piece is then cut
    /// into parts of `part_chars` characters, every part after the first
    /// continuing its line with no break. The assembled text is the lines
    /// joined with `\r\n`, whatever the grouping.
    fn pieces_of(lines: &[&str], group: usize, part_chars: usize) -> HistoryText {
        let mut pieces = Vec::new();
        for (index, lines) in lines.chunks(group).enumerate() {
            let joined = lines.join("\r\n");
            let chars: Vec<char> = joined.chars().collect();
            if chars.is_empty() {
                pieces.push(HistoryPiece {
                    text: Arc::from(""),
                    break_before: index > 0,
                });
            }
            for (part, chars) in chars.chunks(part_chars).enumerate() {
                pieces.push(HistoryPiece {
                    text: Arc::from(chars.iter().collect::<String>().as_str()),
                    break_before: index > 0 && part == 0,
                });
            }
        }
        HistoryText { pieces }
    }

    #[test]
    fn recent_cut_over_pieces_matches_trimming_the_assembled_text() {
        let lines = [
            "first \"line\"",
            "\x1b[1msecond\x1b[0m",
            "",
            "third \u{e9}\u{4e16}",
            "\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\",
            "fifth",
            "",
            "",
            "eighth line here",
        ];
        let assembled = lines.join("\r\n");
        for group in 1..=4 {
            for part_chars in [1, 2, 3, 7, 1000] {
                let text = pieces_of(&lines, group, part_chars);
                assert_eq!(assemble_from(&text, Cut::START), assembled);
                assert_eq!(text.assemble(), assembled);
                for limit in 0..=escaped_len(&assembled) + 8 {
                    let cut = recent_cut(&text, limit);
                    assert_eq!(
                        assemble_from(&text, cut),
                        reference_recent_lines(&assembled, limit),
                        "group {group}, part {part_chars}, limit {limit}"
                    );
                    assert!(
                        escaped_len_from(&text, cut) <= limit,
                        "group {group}, part {part_chars}, limit {limit}"
                    );
                }
            }
        }
    }

    /// A history that trims must give the same bytes whether a pane's text is
    /// one piece or many, and every such save must fit the cap.
    #[test]
    fn trimming_a_history_of_pieces_matches_trimming_the_assembled_history() {
        let numbered: Vec<String> = (0..300)
            .map(|line| format!("\x1b[0;1mline-{line:04}\x1b[0m"))
            .collect();
        let lines: Vec<&str> = numbered.iter().map(String::as_str).collect();
        let assembled = lines.join("\r\n");
        let whole = history_with_texts(vec![
            (1, single("small pane")),
            (2, single(&assembled)),
            (3, single(&assembled)),
        ]);
        let full = reference_json(&whole);
        for divisor in [2, 3, 5, 20] {
            let cap = full.len() / divisor;
            let expected = serialize_history_within(&whole, cap).expect("trimmed");
            for (group, part_chars) in [(1, 1000), (7, 1000), (1, 13), (50, 300)] {
                let pieced = history_with_texts(vec![
                    (1, single("small pane")),
                    (2, pieces_of(&lines, group, part_chars)),
                    (3, pieces_of(&lines, 3, part_chars)),
                ]);
                let serialized = serialize_history_within(&pieced, cap).expect("trimmed");
                assert!(serialized.json.len() <= cap);
                assert_eq!(
                    serialized.json, expected.json,
                    "cap 1/{divisor}, group {group}, part {part_chars}"
                );
                assert_eq!(serialized.trimmed, expected.trimmed);
            }
        }
    }

    /// A single line too big for a pane's share is dropped whole, however
    /// many pieces it runs through.
    #[test]
    fn a_line_longer_than_the_share_is_dropped_whole() {
        let long = HistoryText {
            pieces: vec![
                HistoryPiece {
                    text: Arc::from("head\r\nshort"),
                    break_before: false,
                },
                HistoryPiece {
                    text: Arc::from("a very long line, first part "),
                    break_before: true,
                },
                HistoryPiece {
                    text: Arc::from("second part of that very long line"),
                    break_before: false,
                },
            ],
        };
        let last_line =
            escaped_len("a very long line, first part second part of that very long line");
        let cut = recent_cut(&long, last_line - 1);
        assert_eq!(assemble_from(&long, cut), "");
        let cut = recent_cut(&long, last_line);
        assert_eq!(
            assemble_from(&long, cut),
            "a very long line, first part second part of that very long line"
        );
        let cut = recent_cut(
            &long,
            last_line + BREAK_ESCAPED_LEN + escaped_len("short") - 1,
        );
        assert_eq!(
            assemble_from(&long, cut),
            "a very long line, first part second part of that very long line"
        );
    }

    #[test]
    fn fair_share_leaves_what_small_panes_do_not_use_to_the_rest() {
        assert_eq!(fair_share(vec![10, 100, 100], 110), Some(50));
        assert_eq!(fair_share(vec![10, 20], 30), None);
        assert_eq!(fair_share(vec![40, 40], 30), Some(15));
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_path(&session_path, &empty_snapshot(), None).expect("test precondition");
        save_history_to_path(&history_path, Some(&history_snapshot("split-secret")))
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
        save_history_to_path(&history_path, Some(&history_snapshot("stale-secret")))
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
        save_history_to_path(&history, Some(&history_snapshot("private-secret")))
            .expect("test precondition");

        for path in [&session, &history] {
            let mode = std::fs::metadata(path)
                .expect("test precondition")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
        }
        assert!(
            !session
                .with_extension("json.tmp")
                .try_exists()
                .expect("test stat")
        );
        let directory_mode = std::fs::metadata(&data_dir)
            .expect("data directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
    }

    #[test]
    fn leftover_temporary_from_a_crash_does_not_block_saves() {
        let path = temp_session_path("stale-temporary");
        std::fs::create_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
        std::fs::write(path.with_extension("json.tmp"), b"{\"trunc").expect("test precondition");

        let mut snap = empty_snapshot();
        snap.active = Some(3);
        save_to_path(&path, &snap, None).expect("test precondition");

        let parsed =
            parse_session_file(&std::fs::read_to_string(&path).expect("test precondition"))
                .expect("test precondition");
        assert_eq!(parsed.snapshot.active, Some(3));
        assert!(
            !path
                .with_extension("json.tmp")
                .try_exists()
                .expect("test stat")
        );
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
