use crate::limits::{MAX_SESSION_HISTORY_FILE_BYTES, MAX_SESSION_PATH_SYMLINK_HOPS};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::ser::{Formatter, PrettyFormatter};
use tracing::warn;

use super::lock::DataDirLease;
use super::snapshot::{
    HistoryText, SessionHistory, SessionHistorySnapshot, SessionSnapshot, parse_history_snapshot,
    parse_snapshot,
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

fn read_history_file(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut content = String::new();
    file.take((MAX_SESSION_HISTORY_FILE_BYTES as u64).saturating_add(1))
        .read_to_string(&mut content)?;
    ensure_history_size(content.len())?;
    Ok(content)
}

// Follow symlinks manually so a write through a (possibly dangling) symlink
// lands on the target. `fs::canonicalize` requires the target to exist, which
// excludes the dangling-symlink case stow users hit on the very first save.
fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SESSION_PATH_SYMLINK_HOPS {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(_) => return Ok(current),
        };
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let link = std::fs::read_link(&current)?;
        current = if link.is_absolute() {
            link
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(link)
        };
    }
    Ok(current)
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
#[derive(Debug)]
pub(super) enum Published {
    /// The content and its directory entry are on disk.
    Durable,
    /// The rename happened, so readers already see the new content, but a
    /// later directory sync failed and a crash could still bring back the
    /// previous file. The save is not a failure to undo: the new content is
    /// the best copy there is, and follow-up work may proceed.
    NotDurable(std::io::Error),
}

/// Publishes `source` at `target` through a private (0600) temporary at
/// `pending`: write, fsync the file, rename, fsync the directory. A crash
/// leaves either the previous file or the complete new one, never a truncated
/// one. Before creating the temporary, a leftover file from an interrupted
/// publish is removed. The caller must hold the data directory lease so this
/// cannot remove another live writer's temporary.
///
/// With `replace` false an existing `target` is refused with `AlreadyExists`,
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
    let directory = containing_directory(target);
    remove_stale_temporary(pending)?;
    let mut output = shepr_platform::create_private_file(pending)?;
    let mut published = false;
    let result = (|| {
        if !replace {
            match std::fs::symlink_metadata(target) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "publish target already exists",
                    ));
                }
            }
        }
        std::io::copy(source, &mut output)?;
        output.sync_all()?;
        drop(output);
        std::fs::rename(pending, target)?;
        published = true;
        shepr_platform::sync_directory(directory)
    })();
    match result {
        Ok(()) => Ok(Published::Durable),
        Err(err) if !published => {
            remove_after_failed_publish(pending);
            Err(err)
        }
        Err(err) if replace => Ok(Published::NotDurable(err)),
        Err(err) => {
            remove_after_failed_publish(target);
            Err(err)
        }
    }
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

pub(super) fn save_to_path(path: &Path, snapshot: &SessionSnapshot) -> std::io::Result<Published> {
    save_json_to_path(path, snapshot)
}

fn save_json_to_path<T: serde::Serialize>(path: &Path, snapshot: &T) -> std::io::Result<Published> {
    save_serialized_to_path(path, &serde_json::to_vec_pretty(snapshot)?)
}

fn save_serialized_to_path(path: &Path, json: &[u8]) -> std::io::Result<Published> {
    let target = resolve_write_target(path)?;
    let directory = containing_directory(&target);
    let missing_directories = missing_directory_chain(directory)?;
    std::fs::create_dir_all(directory)?;
    let pending = target.with_extension("json.tmp");
    let mut source = json;
    let published = publish_private_file(&mut source, &pending, &target, true)?;
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
    let share = fair_share(sizes.clone(), room);

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
        cuts.push((kept > 0).then_some(cut));
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

/// A sink that keeps what is written to it while it is at most `cap` bytes,
/// and only counts once it is not: a history that turns out too big to save
/// costs no more memory than one that fits, and its size is still known.
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
    /// Each pane from a cut, in workspace and pane order; `None` leaves
    /// the pane out.
    Cut(&'a [Option<Cut>]),
    /// No workspaces, only the layout fingerprint the history pairs with.
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
    if let Some(fingerprint) = &history.layout_fingerprint {
        write_key(out, &mut fmt, false, "layout_fingerprint")?;
        serde_json::to_writer(&mut *out, fingerprint)?;
        fmt.end_object_value(out)?;
    }
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
                    let cut = cuts.get(pane_index).copied().flatten();
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
fn fair_share(mut sizes: Vec<usize>, room: usize) -> usize {
    sizes.sort_unstable();
    let count = sizes.len();
    let mut remaining = room;
    for (index, size) in sizes.into_iter().enumerate() {
        let share = remaining / (count - index);
        if size > share {
            return share;
        }
        remaining -= size;
    }
    usize::MAX
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
    let target = resolve_write_target(path)?;
    match std::fs::remove_file(&target) {
        Ok(()) => shepr_platform::sync_directory(containing_directory(&target)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Reads the saved layout while the caller owns the data directory.
pub fn load(lease: &DataDirLease) -> Option<SessionSnapshot> {
    if !lease.is_active() {
        return None;
    }
    let path = session_path(lease.directory());
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                event = "persist.restore", subsystem = "persist", outcome = "missing",
                path = %path.display(), "session file is missing"
            );
            return None;
        }
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), error = %err, "failed to read session file"
            );
            return None;
        }
    };
    match parse_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), error = %err, "failed to parse session file, ignoring"
            );
            None
        }
    }
}

pub fn load_history(lease: &DataDirLease) -> Option<SessionHistorySnapshot> {
    if !lease.is_active() {
        return None;
    }
    let path = session_history_path(lease.directory());
    let content = match read_history_file(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), error = %err, "failed to read session history file"
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::pane::HistoryPiece;
    use crate::persist::snapshot::SNAPSHOT_VERSION;

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
            selected: 0,
        }
    }

    #[test]
    fn released_lease_cannot_load_session_files() {
        let scratch = crate::test_support::ScratchDir::new("released-session-lease");
        let mut lease = DataDirLease::acquire(&scratch).expect("lease");
        save_to_path(&session_path(lease.directory()), &empty_snapshot()).expect("save");
        assert!(load(&lease).is_some());
        lease.release();
        assert!(load(&lease).is_none());
        assert!(load_history(&lease).is_none());
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
            layout_fingerprint: history.layout_fingerprint.clone(),
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
        assert_eq!(
            restored.layout_fingerprint, history.layout_fingerprint,
            "the history stays paired with the saved layout"
        );
    }

    fn single(text: &str) -> HistoryText {
        HistoryText::single(Arc::from(text))
    }

    fn history_snapshot(secret: &str) -> SessionHistory {
        SessionHistory {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: None,
            workspaces: vec![vec![(0, single(secret))]],
        }
    }

    fn history_with_panes(panes: &[(u32, &str)]) -> SessionHistory {
        history_with_texts(panes.iter().map(|(id, ansi)| (*id, single(ansi))).collect())
    }

    fn history_with_texts(panes: Vec<(u32, HistoryText)>) -> SessionHistory {
        SessionHistory {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: Some("layout".into()),
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
            layout_fingerprint: Some("fp \"x\"".into()),
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

        let without_fingerprint = SessionHistory {
            layout_fingerprint: None,
            ..history
        };
        let serialized = serialize_history(&without_fingerprint).expect("serialize");
        assert_eq!(serialized.json, reference_json(&without_fingerprint));
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
    /// result still parses and pairs with the same layout.
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
        assert_eq!(restored.layout_fingerprint.as_deref(), Some("layout"));
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
        assert_eq!(fair_share(vec![10, 100, 100], 110), 50);
        assert_eq!(fair_share(vec![10, 20], 30), usize::MAX);
        assert_eq!(fair_share(vec![40, 40], 30), 15);
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_path(&session_path, &empty_snapshot()).expect("test precondition");
        save_history_to_path(&history_path, Some(&history_snapshot("split-secret")))
            .expect("test precondition");

        let session = std::fs::read_to_string(&session_path).expect("test precondition");
        let history = std::fs::read_to_string(&history_path).expect("test precondition");
        assert!(!session.contains("split-secret"));
        assert!(!session.contains("history"));
        assert!(history.contains("split-secret"));
    }

    #[test]
    fn save_to_paths_removes_stale_history_when_history_is_disabled() {
        let (session_path, history_path) = temp_session_paths("clear-history");
        save_to_path(&session_path, &empty_snapshot()).expect("test precondition");
        save_history_to_path(&history_path, Some(&history_snapshot("stale-secret")))
            .expect("test precondition");

        save_history_to_path(&history_path, None).expect("test precondition");

        assert!(session_path.try_exists().expect("test stat"));
        assert!(!history_path.try_exists().expect("test stat"));
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot()).expect("test precondition");

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
        save_to_path(&target, &empty_snapshot()).expect("test precondition");
        std::os::unix::fs::symlink(&target, &link).expect("test precondition");

        let mut snap = empty_snapshot();
        snap.selected = 7;
        save_to_path(&link, &snap).expect("test precondition");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        let parsed = parse_snapshot(&std::fs::read_to_string(&target).expect("test precondition"))
            .expect("test precondition");
        assert_eq!(parsed.selected, 7);
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

    #[test]
    fn saved_session_and_history_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (session_path, history_path) = temp_session_paths("private-mode");
        std::fs::create_dir_all(session_path.parent().expect("test precondition"))
            .expect("test precondition");
        // Publishing renames a fresh private file over the target, so an
        // existing file with a broader mode is replaced, not reused.
        std::fs::write(&history_path, b"old").expect("test precondition");
        std::fs::set_permissions(&history_path, std::fs::Permissions::from_mode(0o644))
            .expect("test precondition");

        save_to_path(&session_path, &empty_snapshot()).expect("test precondition");
        save_history_to_path(&history_path, Some(&history_snapshot("private-secret")))
            .expect("test precondition");

        for path in [&session_path, &history_path] {
            let mode = std::fs::metadata(path)
                .expect("test precondition")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
        }
        assert!(
            !session_path
                .with_extension("json.tmp")
                .try_exists()
                .expect("test stat")
        );
    }

    #[test]
    fn leftover_temporary_from_a_crash_does_not_block_saves() {
        let path = temp_session_path("stale-temporary");
        std::fs::create_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
        std::fs::write(path.with_extension("json.tmp"), b"{\"trunc").expect("test precondition");

        let mut snap = empty_snapshot();
        snap.selected = 3;
        save_to_path(&path, &snap).expect("test precondition");

        let parsed = parse_snapshot(&std::fs::read_to_string(&path).expect("test precondition"))
            .expect("test precondition");
        assert_eq!(parsed.selected, 3);
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
        save_to_path(&link, &empty_snapshot()).expect("test precondition");
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
