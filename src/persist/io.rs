use std::path::{Path, PathBuf};

use tracing::warn;

use super::snapshot::{
    SessionHistorySnapshot, SessionSnapshot, parse_history_snapshot, parse_snapshot,
};

pub(super) fn session_path() -> PathBuf {
    crate::session::data_dir().join("session.json")
}

fn session_history_path() -> PathBuf {
    crate::session::data_dir().join("session-history.json")
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
/// one. `pending` must not exist yet.
///
/// With `replace` false an existing `target` is refused with `AlreadyExists`,
/// and a published target is withdrawn again when the directory sync fails,
/// so that mode only ever returns `Published::Durable` or an error.
/// With `replace` true the target is overwritten, and a completed rename is
/// kept even when the directory sync then reports an error; that comes back
/// as `Published::NotDurable`.
///
/// Both the live session files and the recovery copies go through here, so
/// they share one durability and permission policy. Session history holds
/// full pane scrollback, which can include tokens, so nothing here may be
/// group- or world-readable.
pub(super) fn publish_private_file(
    source: &mut impl std::io::Read,
    pending: &Path,
    target: &Path,
    replace: bool,
) -> std::io::Result<Published> {
    let directory = containing_directory(target);
    let mut output = crate::platform::create_config_temporary(pending, true)?;
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
        crate::platform::sync_directory(directory)
    })();
    match result {
        Ok(()) => Ok(Published::Durable),
        Err(err) if !published => {
            let _ = std::fs::remove_file(pending);
            Err(err)
        }
        Err(err) if replace => Ok(Published::NotDurable(err)),
        Err(err) => {
            let _ = std::fs::remove_file(target);
            Err(err)
        }
    }
}

// A crash between creating the temporary and renaming it leaves the file
// behind, and the exclusive create in `publish_private_file` would then refuse
// every later save. Unlinking a directory fails, so anything other than a
// leftover file in the way still fails the save. Removing it is safe because
// only one server writes a data directory: `SessionWriter` holds the
// directory's lock (`lock.rs`) before any write.
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
    let created = !directory.exists();
    std::fs::create_dir_all(directory)?;
    let pending = target.with_extension("json.tmp");
    remove_stale_temporary(&pending)?;
    let published = publish_private_file(&mut json.as_bytes(), &pending, &target, true)?;
    if created && matches!(published, Published::Durable) {
        // A freshly created data directory is itself only an unsynced entry
        // in its parent until that parent is synced.
        if let Err(err) = crate::platform::sync_directory(containing_directory(directory)) {
            return Ok(Published::NotDurable(err));
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
        Some(history) => save_history_json_to_path(path, &serialize_history(history)?),
        None => clear_path(path),
    }
}

pub(super) fn serialize_history(history: &SessionHistorySnapshot) -> std::io::Result<String> {
    Ok(serde_json::to_string_pretty(history)?)
}

/// Writes history that `serialize_history` already produced, so a caller that
/// needs the bytes too (to tell whether anything changed) serializes once.
pub(super) fn save_history_json_to_path(path: &Path, json: &str) -> std::io::Result<()> {
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
        Ok(()) => crate::platform::sync_directory(containing_directory(&target)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Reads the saved layout for restore. Restoring resumes native agent
/// sessions, so a server that does not own the data directory must not do it:
/// the directory's lock is claimed first, and while another server holds it
/// nothing is restored.
pub fn load() -> Option<SessionSnapshot> {
    let path = session_path();
    if let Err(err) = super::lock::claim(containing_directory(&path)) {
        if super::lock::is_owned_elsewhere(&err) {
            tracing::error!(
                event = "persist.restore", subsystem = "persist", outcome = "owned_elsewhere",
                path = %path.display(), err = %err,
                "another server owns this session's files; not restoring or saving them"
            );
            return None;
        }
        warn!(
            event = "persist.restore", subsystem = "persist", outcome = "lock_error",
            path = %path.display(), err = %err,
            "could not lock the session directory; session was not restored"
        );
        return None;
    }
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

pub fn load_history() -> Option<SessionHistorySnapshot> {
    let path = session_history_path();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
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
            .keep_until_exit()
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

        assert!(session_path.exists());
        assert!(!history_path.exists());
    }

    #[test]
    fn load_refuses_a_session_another_server_owns() {
        let env = crate::test_support::IsolatedEnv::new();
        env.set("XDG_CONFIG_HOME", env.path());
        let path = session_path();
        save_to_path(&path, &empty_snapshot()).expect("test precondition");
        let directory = containing_directory(&path).to_path_buf();
        let other_server = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(super::super::lock::LOCK_FILE_NAME))
            .expect("test precondition");
        other_server.try_lock().expect("test precondition");

        assert!(load().is_none(), "another server's session is not restored");

        drop(other_server);
        assert!(load().is_some());
        super::super::lock::release(&directory);
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot()).expect("test precondition");

        clear_path(&path).expect("test precondition");

        assert!(!path.exists());
    }

    #[test]
    fn clear_path_ignores_missing_session_file() {
        let path = temp_session_path("clear-missing");

        clear_path(&path).expect("test precondition");

        assert!(!path.exists());
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
        assert!(target.exists());
    }

    #[test]
    fn saved_session_and_history_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (session_path, history_path) = temp_session_paths("private-mode");
        std::fs::create_dir_all(session_path.parent().expect("test precondition"))
            .expect("test precondition");
        // A file left by an older build with default permissions is replaced,
        // not reused, so it does not keep its broader mode.
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
        assert!(!session_path.with_extension("json.tmp").exists());
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
        assert!(!path.with_extension("json.tmp").exists());
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
        assert!(target.exists());

        clear_path(&link).expect("test precondition");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("test precondition")
                .file_type()
                .is_symlink()
        );
        assert!(!target.exists());
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
        assert!(target.exists());
    }
}
