use std::io::Read;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::snapshot::{
    SessionHistorySnapshot, SessionSnapshot, parse_history_snapshot, parse_snapshot,
};

pub(super) fn session_path(data_dir: &Path) -> PathBuf {
    data_dir.join("session.json")
}

fn session_history_path(data_dir: &Path) -> PathBuf {
    data_dir.join("session-history.json")
}

// Bound restore input and files this build writes. Saves trim the oldest
// scrollback to stay under it (`serialize_history`), so a file over it was not
// written by this build and restore refuses it.
const MAX_SESSION_HISTORY_FILE_BYTES: usize = 256 * 1024 * 1024;

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
    for _ in 0..16 {
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
            path = %path.display(), err = %err,
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
    save_serialized_to_path(path, &serde_json::to_string_pretty(snapshot)?)
}

fn save_serialized_to_path(path: &Path, json: &str) -> std::io::Result<Published> {
    let target = resolve_write_target(path)?;
    let directory = containing_directory(&target);
    let missing_directories = missing_directory_chain(directory)?;
    std::fs::create_dir_all(directory)?;
    let pending = target.with_extension("json.tmp");
    let published = publish_private_file(&mut json.as_bytes(), &pending, &target, true)?;
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
    history: Option<&SessionHistorySnapshot>,
) -> std::io::Result<()> {
    match history {
        Some(history) => save_history_json_to_path(path, &serialize_history(history)?.json),
        None => clear_path(path),
    }
}

/// A history serialized to fit the file cap, and what was cut to make it fit.
pub(super) struct SerializedHistory {
    pub(super) json: String,
    pub(super) trimmed: Option<HistoryTrim>,
}

/// The oldest scrollback `serialize_history` left out of an oversized history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HistoryTrim {
    /// Panes that lost lines, including any that kept none.
    pub(super) panes: usize,
    /// Serialized bytes of pane history left out.
    pub(super) dropped_bytes: usize,
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
pub(super) fn serialize_history(
    history: &SessionHistorySnapshot,
) -> std::io::Result<SerializedHistory> {
    serialize_history_within(history, MAX_SESSION_HISTORY_FILE_BYTES)
}

fn serialize_history_within(
    history: &SessionHistorySnapshot,
    cap: usize,
) -> std::io::Result<SerializedHistory> {
    let json = serde_json::to_string_pretty(history)?;
    if json.len() <= cap {
        return Ok(SerializedHistory {
            json,
            trimmed: None,
        });
    }

    let mut sizes = Vec::new();
    for workspace in &history.workspaces {
        for tab in &workspace.tabs {
            for pane in tab.panes.values() {
                sizes.push(escaped_len(&pane.ansi)?);
            }
        }
    }
    let content: usize = sizes.iter().sum();
    let room = json
        .len()
        .checked_sub(content)
        .and_then(|structure| cap.checked_sub(structure))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session history structure alone exceeds the file cap",
            )
        })?;
    let share = fair_share(sizes, room);

    let mut trim = HistoryTrim {
        panes: 0,
        dropped_bytes: 0,
    };
    let mut workspaces = Vec::with_capacity(history.workspaces.len());
    for workspace in &history.workspaces {
        let mut tabs = Vec::with_capacity(workspace.tabs.len());
        for tab in &workspace.tabs {
            let mut panes = std::collections::HashMap::with_capacity(tab.panes.len());
            for (id, pane) in &tab.panes {
                let kept = recent_lines(&pane.ansi, share)?;
                if kept.len() < pane.ansi.len() {
                    trim.panes += 1;
                    trim.dropped_bytes += escaped_len(&pane.ansi)? - escaped_len(kept)?;
                }
                if !kept.is_empty() {
                    panes.insert(
                        *id,
                        super::snapshot::PaneHistorySnapshot {
                            ansi: kept.to_owned(),
                        },
                    );
                }
            }
            tabs.push(super::snapshot::TabHistorySnapshot { panes });
        }
        workspaces.push(super::snapshot::WorkspaceHistorySnapshot { tabs });
    }
    let json = serde_json::to_string_pretty(&SessionHistorySnapshot {
        version: history.version,
        layout_fingerprint: history.layout_fingerprint.clone(),
        workspaces,
    })?;
    if json.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("trimmed session history still exceeds {cap} bytes"),
        ));
    }
    Ok(SerializedHistory {
        json,
        trimmed: Some(trim),
    })
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

/// The longest suffix of `ansi` made of whole lines whose JSON-escaped size
/// is at most `limit`.
fn recent_lines(ansi: &str, limit: usize) -> std::io::Result<&str> {
    let bytes = ansi.as_bytes();
    let mut start = bytes.len();
    let mut kept = 0usize;
    while start > 0 {
        // The line ending at `start` begins after the newline before its own
        // terminator. `\n` never occurs inside a multi-byte character, so
        // every cut lands on a character boundary.
        let line_start = bytes[..start - 1]
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
        let line = ansi.get(line_start..start).unwrap_or_default();
        kept += escaped_len(line)?;
        if kept > limit {
            break;
        }
        start = line_start;
    }
    Ok(ansi.get(start..).unwrap_or_default())
}

/// Bytes `text` occupies inside a JSON string literal, quotes excluded.
/// Escaping is per character, so the sizes of adjacent pieces add up.
fn escaped_len(text: &str) -> std::io::Result<usize> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, text)?;
    Ok(count.0.saturating_sub(2))
}

/// Writes history that `serialize_history` already produced, so a caller that
/// needs the bytes too (to tell whether anything changed) serializes once.
pub(super) fn save_history_json_to_path(path: &Path, json: &str) -> std::io::Result<()> {
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

/// Reads the saved layout for restore. The server acquires a DataDirLease
/// before calling this, so native agent sessions cannot be restored twice.
pub fn load(data_dir: &Path) -> Option<SessionSnapshot> {
    let path = session_path(data_dir);
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
                path = %path.display(), err = %err, "failed to read session file"
            );
            return None;
        }
    };
    match parse_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), err = %err, "failed to parse session file, ignoring"
            );
            None
        }
    }
}

pub fn load_history(data_dir: &Path) -> Option<SessionHistorySnapshot> {
    let path = session_history_path(data_dir);
    let content = match read_history_file(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(err = %err, "failed to read session history file");
            return None;
        }
    };
    match parse_history_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            warn!(err = %err, "failed to parse session history file, ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::snapshot::{
        PaneHistorySnapshot, SNAPSHOT_VERSION, TabHistorySnapshot, WorkspaceHistorySnapshot,
    };

    /// A session file whose data directory does not exist yet, so saves
    /// exercise creating it.
    fn temp_session_path(name: &str) -> PathBuf {
        crate::test_support::ScratchDir::new(name)
            .join("data")
            .join("session.json")
    }

    fn temp_session_paths(name: &str) -> (PathBuf, PathBuf) {
        let session = temp_session_path(name);
        let history = session.with_file_name("session-history.json");
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

    /// The history limit is inclusive, and a refusal is an `InvalidData`
    /// error. Saves trim to stay under it, so only restore meets a file over
    /// it.
    #[test]
    fn history_size_limit_admits_the_limit_and_refuses_one_byte_more() {
        ensure_history_size(MAX_SESSION_HISTORY_FILE_BYTES).expect("the limit itself is allowed");
        let error = ensure_history_size(MAX_SESSION_HISTORY_FILE_BYTES + 1)
            .expect_err("one byte over is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    fn history_snapshot(secret: &str) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: None,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![TabHistorySnapshot {
                    panes: std::collections::HashMap::from([(
                        0,
                        PaneHistorySnapshot {
                            ansi: secret.to_string(),
                        },
                    )]),
                }],
            }],
        }
    }

    fn history_with_panes(panes: &[(u32, &str)]) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: Some("layout".into()),
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![TabHistorySnapshot {
                    panes: panes
                        .iter()
                        .map(|(id, ansi)| {
                            (
                                *id,
                                PaneHistorySnapshot {
                                    ansi: (*ansi).to_owned(),
                                },
                            )
                        })
                        .collect(),
                }],
            }],
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
        assert_eq!(
            serialized.json,
            serde_json::to_string_pretty(&history).expect("serialize")
        );
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
        let full = serde_json::to_string_pretty(&history).expect("serialize");
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

        let restored = parse_history_snapshot(&serialized.json).expect("trimmed history parses");
        assert_eq!(restored.layout_fingerprint.as_deref(), Some("layout"));
        let panes = &restored.workspaces[0].tabs[0].panes;
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

    #[test]
    fn recent_lines_counts_json_escaping_and_cuts_only_at_line_starts() {
        // ESC escapes to six bytes, CR and LF to two each, and multi-byte
        // characters are written as they are.
        let ansi = "old\r\n\x1b[1mnew\u{e9}\r\nprompt \u{e9}";
        assert_eq!(escaped_len("\x1b[1mnew\u{e9}\r\n").expect("len"), 18);
        let prompt = escaped_len("prompt \u{e9}").expect("len");
        assert_eq!(recent_lines(ansi, prompt - 1).expect("cut"), "");
        assert_eq!(recent_lines(ansi, prompt).expect("cut"), "prompt \u{e9}");
        assert_eq!(
            recent_lines(ansi, prompt + 18).expect("cut"),
            "\x1b[1mnew\u{e9}\r\nprompt \u{e9}"
        );
        assert_eq!(recent_lines(ansi, usize::MAX).expect("cut"), ansi);
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
