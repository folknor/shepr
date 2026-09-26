use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{SessionHistorySnapshot, SessionSnapshot};

/// Whether this writer may touch its data directory's session files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// No write attempted yet, or the last claim failed for a reason other
    /// than another owner (the lock file was unusable); claim on next write.
    Unclaimed,
    /// This process holds the data directory's lock.
    Owned,
    /// Another server held the lock when this one first tried to write. The
    /// session files are that server's, and this one never loaded them, so it
    /// stays out for its whole life rather than overwrite them later.
    Refused,
    /// The final shutdown save is done and the lock is released.
    Retired,
}

/// Shared by autosave, pane-exit checkpoints, and shutdown.
pub(crate) struct SessionWriter {
    path: PathBuf,
    protect_unloaded: bool,
    ownership: Ownership,
    /// Digest of the history JSON this writer last put on disk. History is
    /// the bulk of a save (full scrollback per pane) and is rewritten and
    /// fsynced on every save otherwise, even when no pane printed anything.
    written_history: Option<Vec<u8>>,
}

impl SessionWriter {
    pub(crate) fn new(data_dir: &Path, protect_unloaded: bool) -> Self {
        Self::at(super::io::session_path(data_dir), protect_unloaded)
    }

    fn at(path: PathBuf, protect_unloaded: bool) -> Self {
        Self {
            path,
            protect_unloaded,
            ownership: Ownership::Unclaimed,
            written_history: None,
        }
    }

    fn directory(&self) -> &Path {
        super::io::containing_directory(&self.path)
    }

    /// Claims the data directory before the first write; see `lock.rs`.
    fn may_write(&mut self) -> bool {
        match self.ownership {
            Ownership::Owned => true,
            Ownership::Refused | Ownership::Retired => false,
            Ownership::Unclaimed => match super::lock::claim(self.directory()) {
                Ok(()) => {
                    self.ownership = Ownership::Owned;
                    true
                }
                Err(err) if super::lock::is_owned_elsewhere(&err) => {
                    tracing::error!(
                        event = "persist.lock", subsystem = "persist", outcome = "owned_elsewhere",
                        path = %self.path.display(), err = %err,
                        "another server owns this session's files; this server will not save them"
                    );
                    self.ownership = Ownership::Refused;
                    false
                }
                Err(err) => {
                    tracing::error!(
                        event = "persist.lock", subsystem = "persist", outcome = "error",
                        path = %self.path.display(), err = %err,
                        "could not lock the session directory; session files were not written"
                    );
                    false
                }
            },
        }
    }

    /// Ends this writer's ownership after the final shutdown save, releasing
    /// the data directory for the next server before this process finishes
    /// tearing down. Later saves and clears are ignored.
    pub(crate) fn retire(&mut self) {
        // Also drops a claim `load` took for restore; a no-op when this
        // process never held one.
        super::lock::release(self.directory());
        self.ownership = Ownership::Retired;
    }

    fn preserve_unloaded(&mut self) -> io::Result<()> {
        if self.protect_unloaded && preserve_existing(&self.path)? {
            self.protect_unloaded = false;
        }
        Ok(())
    }

    fn preserve_snapshot_history(&self) {
        if let Err(err) = preserve_snapshot_history(&self.path) {
            tracing::warn!(
                event = "persist.snapshot", outcome = "error", path = %self.path.display(),
                err = %err, "failed to preserve session snapshot"
            );
        }
    }

    pub(crate) fn save(
        &mut self,
        snapshot: &SessionSnapshot,
        history: Option<&SessionHistorySnapshot>,
    ) {
        if !self.may_write() {
            return;
        }
        let result = self.preserve_unloaded().and_then(|()| {
            self.preserve_snapshot_history();
            super::io::save_to_path(&self.path, snapshot)
        });
        self.finish_save(result, snapshot, history);
    }

    fn finish_save(
        &mut self,
        result: io::Result<super::io::Published>,
        snapshot: &SessionSnapshot,
        history: Option<&SessionHistorySnapshot>,
    ) {
        match result {
            Ok(super::io::Published::Durable) => {}
            // The new layout already replaced the old file; only its
            // directory entry may not be on disk yet. That is still our
            // committed layout, so the history that pairs with it is written
            // too and the unloaded-file guard is released, exactly as for a
            // durable save.
            Ok(super::io::Published::NotDurable(err)) => {
                crate::logging::session_save_failed(
                    &self.path,
                    &format!("saved, but syncing its directory failed: {err}"),
                );
            }
            Err(err) => {
                crate::logging::session_save_failed(&self.path, &err.to_string());
                return;
            }
        }
        // Optional history failure must not reclassify our committed layout as unloaded.
        self.protect_unloaded = false;
        self.preserve_snapshot_history();
        let history_path = self.path.with_file_name("session-history.json");
        if let Err(err) = self.save_history(&history_path, history) {
            self.written_history = None;
            crate::logging::session_save_failed(&history_path, &err.to_string());
        }
        crate::logging::session_saved(&self.path, snapshot.workspaces.len());
    }

    /// Writes the history unless the file already holds exactly these bytes
    /// from this writer's previous save. The history names the layout it
    /// pairs with, so a changed layout always changes the bytes.
    fn save_history(
        &mut self,
        history_path: &Path,
        history: Option<&SessionHistorySnapshot>,
    ) -> io::Result<()> {
        use sha2::{Digest, Sha256};
        let Some(history) = history else {
            self.written_history = None;
            return super::io::save_history_to_path(history_path, None);
        };
        let json = super::io::serialize_history(history)?;
        let digest = Sha256::digest(json.as_bytes()).to_vec();
        if self.written_history.as_ref() == Some(&digest) {
            return Ok(());
        }
        self.written_history = None;
        super::io::save_history_json_to_path(history_path, &json)?;
        self.written_history = Some(digest);
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        if !self.may_write() {
            return;
        }
        self.written_history = None;
        let result = self.preserve_unloaded().and_then(|()| {
            self.preserve_snapshot_history();
            super::io::clear_path(&self.path)
        });
        if let Err(err) = result {
            crate::logging::session_clear_failed(&self.path, &err.to_string());
            return;
        }
        let history_path = self.path.with_file_name("session-history.json");
        if let Err(err) = super::io::clear_path(&history_path) {
            crate::logging::session_clear_failed(&history_path, &err.to_string());
        }
        crate::logging::session_cleared(&self.path);
    }
}

const SNAPSHOT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const SNAPSHOT_LIMIT: usize = 48;

fn preserve_snapshot_history(path: &Path) -> io::Result<()> {
    let directory = path.with_file_name("session-snapshots");
    let existing = match recovery_files(&directory) {
        Ok(files) => files,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    if let Some((_, latest)) = existing.last() {
        let modified = std::fs::metadata(latest)?.modified()?;
        if SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age < SNAPSHOT_INTERVAL)
        {
            return Ok(());
        }
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let Ok(snapshot) = serde_json::from_slice::<SessionSnapshot>(&bytes) else {
        return Ok(());
    };
    if snapshot.version != super::snapshot::SNAPSHOT_VERSION || snapshot.workspaces.is_empty() {
        return Ok(());
    }
    if let Some((_, latest)) = existing.last() {
        let previous_bytes = std::fs::read(latest)?;
        if let Ok(previous) = serde_json::from_slice::<SessionSnapshot>(&previous_bytes)
            && super::snapshot::layout_fingerprint(&snapshot).is_some_and(|fingerprint| {
                super::snapshot::layout_fingerprint(&previous).as_ref() == Some(&fingerprint)
            })
        {
            return Ok(());
        }
    }
    preserve_existing_in(path, "session-snapshots", SNAPSHOT_LIMIT)?;
    Ok(())
}

fn preserve_existing(path: &Path) -> io::Result<bool> {
    preserve_existing_in(path, "session-backups", 3)
}

fn preserve_existing_in(path: &Path, directory_name: &str, keep: usize) -> io::Result<bool> {
    let mut source = match File::open(path) {
        Ok(file) => file,
        // Recheck on the next mutation until a fresh session is actually saved.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if !source.metadata()?.is_file() {
        return Err(io::Error::other("session path is not a regular file"));
    }
    let directory = path.with_file_name(directory_name);
    std::fs::create_dir_all(&directory)?;
    let older = recovery_files(&directory)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Keep creation order even when the wall clock moves backwards.
    let timestamp = match older.last() {
        Some((previous, _)) => now.max(
            previous
                .checked_add(1)
                .ok_or_else(|| io::Error::other("session recovery sequence exhausted"))?,
        ),
        None => now,
    };
    for sequence in 0..128 {
        let backup = directory.join(format!(
            "session-{timestamp:039}-{}-{sequence}.json",
            std::process::id()
        ));
        match copy_recovery(&mut source, &backup) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
        tracing::info!(
            event = "persist.backup",
            subsystem = "persist",
            outcome = "ok",
            path = %path.display(),
            backup_path = %backup.display(),
            "preserved session recovery copy"
        );
        if let Err(err) = prune_backups(&older, keep) {
            if directory_name == "session-snapshots" {
                std::fs::remove_file(&backup)?;
                return Err(err);
            }
            tracing::warn!(
                event = "persist.backup", subsystem = "persist", outcome = "prune_error",
                path = %directory.display(), err = %err, "failed to prune session recovery copies"
            );
        }
        return Ok(true);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate session recovery copy",
    ))
}

fn copy_recovery(source: &mut impl io::Read, backup: &Path) -> io::Result<()> {
    // Without `replace` a failed directory sync withdraws the copy and comes
    // back as an error, so `NotDurable` cannot happen here; treat it as a
    // failure anyway rather than count an unsynced copy as a recovery copy.
    if let super::io::Published::NotDurable(err) =
        super::io::publish_private_file(source, &backup.with_extension("pending"), backup, false)?
    {
        let _ = std::fs::remove_file(backup);
        return Err(err);
    }
    // The recovery directory may have just been created; its own entry must be
    // durable too, or the copy is not a recovery copy at all.
    let directory = super::io::containing_directory(backup);
    if let Err(err) = crate::platform::sync_directory(super::io::containing_directory(directory)) {
        let _ = std::fs::remove_file(backup);
        return Err(err);
    }
    Ok(())
}

fn recovery_files(directory: &Path) -> io::Result<Vec<(u128, PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && let Some(timestamp) = entry.file_name().to_str().and_then(recovery_timestamp)
        {
            files.push((timestamp, entry.path()));
        }
    }
    files.sort();
    Ok(files)
}

fn prune_backups(older: &[(u128, PathBuf)], keep: usize) -> io::Result<()> {
    // The new copy is durable before any of the previous copies are removed.
    let mut remaining = older.len().saturating_sub(keep.saturating_sub(1));
    let mut failure = None;
    for (_, path) in older {
        if remaining == 0 {
            return Ok(());
        }
        match std::fs::remove_file(path) {
            Ok(()) => remaining -= 1,
            Err(err) => failure = Some(err),
        }
    }
    if remaining > 0
        && let Some(err) = failure
    {
        return Err(err);
    }
    Ok(())
}

fn recovery_timestamp(name: &str) -> Option<u128> {
    let fields = name.strip_prefix("session-")?.strip_suffix(".json")?;
    let fields: Vec<_> = fields.split('-').collect();
    if fields.len() == 3
        && fields[0].len() == 39
        && fields
            .iter()
            .all(|field| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        fields[0].parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(protect_unloaded: bool) -> SessionWriter {
        let directory = crate::test_support::ScratchDir::new("session-recovery").keep_until_exit();
        SessionWriter::at(directory.join("session.json"), protect_unloaded)
    }

    fn snapshot() -> SessionSnapshot {
        serde_json::from_value(serde_json::json!({
            "version": super::super::snapshot::SNAPSHOT_VERSION,
            "workspaces": [{
                "id": "w1",
                "identity_cwd": "/tmp/shepr-writer-test",
                "tabs": [{
                    "layout": { "Pane": 0 },
                    "panes": {
                        "0": { "cwd": "/tmp/shepr-writer-test" }
                    },
                    "zoomed": false,
                    "focused": 0,
                    "root_pane": 0
                }],
                "active_tab": 0
            }],
            "active": 0,
            "selected": 0
        }))
        .expect("test precondition")
    }

    fn backups(writer: &SessionWriter) -> Vec<Vec<u8>> {
        let directory = writer.path.with_file_name("session-backups");
        if !directory.exists() {
            return Vec::new();
        }
        let mut entries: Vec<_> = std::fs::read_dir(directory)
            .expect("test precondition")
            .map(|entry| entry.expect("test precondition").path())
            .collect();
        entries.sort();
        entries
            .into_iter()
            .map(|path| std::fs::read(path).expect("test precondition"))
            .collect()
    }

    fn snapshots(writer: &SessionWriter) -> Vec<(u128, PathBuf)> {
        recovery_files(&writer.path.with_file_name("session-snapshots")).expect("test precondition")
    }

    #[test]
    fn snapshot_survives_exit_bursts_clears_and_writer_restarts() {
        let mut writer = writer(false);
        let original = snapshot();
        writer.save(&original, None);
        let files = snapshots(&writer);
        assert_eq!(files.len(), 1);
        let saved = std::fs::read(&files[0].1).expect("test precondition");
        for i in 0..100 {
            let mut shrinking = snapshot();
            shrinking.workspaces[0].custom_name = Some(format!("remaining pane {i}"));
            writer.save(&shrinking, None);
            writer = SessionWriter::at(writer.path.clone(), false);
        }
        writer.clear();
        assert!(
            !writer.path.exists(),
            "intentional clear must still persist"
        );
        assert_eq!(snapshots(&writer), files);
        assert_eq!(
            std::fs::read(&files[0].1).expect("test precondition"),
            saved
        );
        assert!(backups(&writer).is_empty());
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_history_is_bounded_and_does_not_rotate_identical_layouts() {
        let mut writer = writer(false);
        let directory = writer.path.with_file_name("session-snapshots");
        std::fs::create_dir(&directory).expect("test precondition");
        let manual = directory.join("my-layout.json");
        std::fs::write(&manual, b"manual").expect("test precondition");
        for i in 0..SNAPSHOT_LIMIT {
            std::fs::write(
                directory.join(format!("session-{i:039}-1-0.json")),
                b"old snapshot",
            )
            .expect("test precondition");
        }
        for (_, path) in snapshots(&writer) {
            File::options()
                .write(true)
                .open(path)
                .expect("test precondition")
                .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH))
                .expect("test precondition");
        }
        writer.save(&snapshot(), None);
        assert_eq!(snapshots(&writer).len(), SNAPSHOT_LIMIT);
        assert!(
            !directory
                .join(format!("session-{:039}-1-0.json", 0))
                .exists()
        );
        assert!(manual.exists());

        for (_, path) in snapshots(&writer) {
            std::fs::remove_file(path).expect("test precondition");
        }
        let old = directory.join(format!("session-{:039}-1-0.json", 1));
        let saved = std::fs::read(&writer.path).expect("test precondition");
        let reordered: serde_json::Value =
            serde_json::from_slice(&saved).expect("test precondition");
        let equivalent = serde_json::to_vec(&reordered).expect("test precondition");
        assert_ne!(saved, equivalent);
        std::fs::write(&old, equivalent).expect("test precondition");
        File::options()
            .write(true)
            .open(&old)
            .expect("test precondition")
            .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .expect("test precondition");
        writer.save(&snapshot(), None);
        assert_eq!(snapshots(&writer), vec![(1, old)]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_cadence_recovers_after_clock_rollback_and_restart() {
        let mut writer = writer(false);
        writer.save(&snapshot(), None);
        let file = snapshots(&writer).pop().expect("test precondition").1;
        File::options()
            .write(true)
            .open(&file)
            .expect("test precondition")
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() + std::time::Duration::from_secs(86400)),
            )
            .expect("test precondition");
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("after clock rollback".into());
        writer.save(&changed, None);
        assert_eq!(snapshots(&writer).len(), 2);
        writer = SessionWriter::at(writer.path.clone(), false);
        changed.workspaces[0].custom_name = Some("after restart".into());
        writer.save(&changed, None);
        assert_eq!(
            snapshots(&writer).len(),
            2,
            "new mtime restores cadence across restart"
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn pruning_continues_past_an_undeletable_entry() {
        let writer = writer(false);
        let locked = writer.path.with_file_name("undeletable");
        std::fs::create_dir(&locked).expect("test precondition");
        let removable = writer.path.with_file_name("removable");
        std::fs::write(&removable, b"old").expect("test precondition");
        assert!(prune_backups(&[(1, locked.clone()), (2, removable.clone())], 2).is_ok());
        assert!(locked.exists());
        assert!(!removable.exists());
        assert!(prune_backups(&[(1, locked)], 1).is_err());
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_failure_does_not_block_primary_save_and_clear() {
        let mut writer = writer(false);
        std::fs::write(writer.path.with_file_name("session-snapshots"), b"blocked")
            .expect("test precondition");
        writer.save(&snapshot(), None);
        assert!(writer.path.exists());
        writer.clear();
        assert!(!writer.path.exists());
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn healthy_and_fresh_sessions_save_and_clear_without_backups() {
        for protect_unloaded in [false, true] {
            let mut writer = writer(protect_unloaded);
            if !protect_unloaded {
                super::super::io::save_to_path(&writer.path, &snapshot())
                    .expect("test precondition");
            }
            writer.save(&snapshot(), None);
            assert!(!writer.protect_unloaded);
            assert!(writer.path.exists());
            writer.save(&snapshot(), None);
            writer.clear();
            assert!(!writer.path.exists());
            assert!(backups(&writer).is_empty());
            std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
                .expect("test precondition");
        }
    }

    #[test]
    fn failed_recovery_blocks_mutations_then_retries_preserving_exact_bytes_once() {
        let original = b"invalid utf8 \xff";
        let mut writer = writer(true);
        std::fs::write(&writer.path, original).expect("test precondition");
        let history_path = writer.path.with_file_name("session-history.json");
        std::fs::write(&history_path, b"history").expect("test precondition");
        let directory = writer.path.with_file_name("session-backups");
        std::fs::write(&directory, b"blocks recovery").expect("test precondition");
        writer.save(&snapshot(), None);
        writer.clear();
        assert!(writer.protect_unloaded);
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            original
        );
        assert_eq!(
            std::fs::read(&history_path).expect("test precondition"),
            b"history"
        );

        std::fs::remove_file(&directory).expect("test precondition");
        writer.save(&snapshot(), None);
        assert!(!writer.protect_unloaded);
        writer.save(&snapshot(), None);
        writer.clear();
        assert!(!writer.path.exists());
        assert_eq!(backups(&writer), vec![original.to_vec()]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn optional_history_failure_does_not_block_later_layout_saves() {
        let mut writer = writer(true);
        let history = writer.path.with_file_name("session-history.json");
        std::fs::create_dir(&history).expect("test precondition");
        std::fs::write(
            writer.path.with_file_name("session-backups"),
            b"unavailable",
        )
        .expect("test precondition");
        writer.save(&snapshot(), None);
        assert!(
            !writer.protect_unloaded,
            "structural session was saved successfully"
        );
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("latest layout".into());
        writer.save(&changed, None);
        let saved: SessionSnapshot =
            serde_json::from_slice(&std::fs::read(&writer.path).expect("test precondition"))
                .expect("test precondition");
        assert_eq!(
            saved.workspaces[0].custom_name.as_deref(),
            Some("latest layout")
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn unsynced_layout_save_still_writes_history_and_releases_the_guard() {
        let history = SessionHistorySnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            layout_fingerprint: None,
            workspaces: Vec::new(),
        };
        let mut writer = writer(true);
        let history_path = writer.path.with_file_name("session-history.json");
        // The layout rename happened; only the directory sync after it failed.
        super::super::io::save_to_path(&writer.path, &snapshot()).expect("test precondition");
        writer.finish_save(
            Ok(super::super::io::Published::NotDurable(io::Error::other(
                "directory sync failed",
            ))),
            &snapshot(),
            Some(&history),
        );
        assert!(!writer.protect_unloaded);
        assert!(history_path.exists(), "history pairs with the new layout");

        // A save that never reached the target changes nothing else.
        let mut failed = SessionWriter::at(writer.path.clone(), true);
        std::fs::remove_file(&history_path).expect("test precondition");
        failed.finish_save(
            Err(io::Error::other("write failed")),
            &snapshot(),
            Some(&history),
        );
        assert!(failed.protect_unloaded);
        assert!(!history_path.exists());
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    /// Holds `writer`'s data directory the way another server process would:
    /// through its own open file description.
    fn foreign_lock(writer: &SessionWriter) -> File {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(
                writer
                    .path
                    .with_file_name(super::super::lock::LOCK_FILE_NAME),
            )
            .expect("test precondition");
        file.try_lock().expect("test precondition");
        file
    }

    #[test]
    fn a_directory_owned_by_another_server_is_never_written() {
        let mut writer = writer(true);
        std::fs::write(&writer.path, b"other server's layout").expect("test precondition");
        let other = foreign_lock(&writer);

        writer.save(&snapshot(), None);
        writer.clear();
        assert_eq!(writer.ownership, Ownership::Refused);
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            b"other server's layout"
        );

        // The other server going away does not hand its files to this one,
        // which never loaded them.
        drop(other);
        writer.save(&snapshot(), None);
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            b"other server's layout"
        );
        assert!(backups(&writer).is_empty());
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn retiring_releases_the_directory_and_ignores_later_saves() {
        let mut writer = writer(false);
        writer.save(&snapshot(), None);
        assert_eq!(writer.ownership, Ownership::Owned);
        let lock = File::open(
            writer
                .path
                .with_file_name(super::super::lock::LOCK_FILE_NAME),
        )
        .expect("test precondition");
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));

        writer.retire();
        lock.try_lock().expect("the next server can take over");
        let saved = std::fs::read(&writer.path).expect("test precondition");
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("after shutdown".into());
        writer.save(&changed, None);
        writer.clear();
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            saved
        );
        drop(lock);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn unchanged_history_is_not_rewritten() {
        let history = |fingerprint: &str| SessionHistorySnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            layout_fingerprint: Some(fingerprint.into()),
            workspaces: Vec::new(),
        };
        let mut writer = writer(false);
        let history_path = writer.path.with_file_name("session-history.json");
        writer.save(&snapshot(), Some(&history("one")));
        let written = std::fs::read(&history_path).expect("test precondition");

        // A marker only survives if the identical history is skipped.
        std::fs::write(&history_path, b"untouched").expect("test precondition");
        writer.save(&snapshot(), Some(&history("one")));
        assert_eq!(
            std::fs::read(&history_path).expect("test precondition"),
            b"untouched"
        );

        writer.save(&snapshot(), Some(&history("two")));
        let changed = std::fs::read(&history_path).expect("test precondition");
        assert_ne!(changed, written);
        assert!(String::from_utf8_lossy(&changed).contains("two"));

        // A clear forgets what was written, so the same history is written
        // again afterwards.
        writer.clear();
        assert!(!history_path.exists());
        writer.save(&snapshot(), Some(&history("two")));
        assert_eq!(
            std::fs::read(&history_path).expect("test precondition"),
            changed
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn pruning_leaves_user_named_recovery_files_alone() {
        let mut writer = writer(true);
        let directory = writer.path.with_file_name("session-backups");
        std::fs::create_dir(&directory).expect("test precondition");
        let manual = directory.join("session-000-manual.json");
        std::fs::write(&manual, b"manual recovery copy").expect("test precondition");
        for i in 0..5u8 {
            writer.protect_unloaded = true;
            std::fs::write(&writer.path, [i]).expect("test precondition");
            writer.save(&snapshot(), None);
        }
        assert_eq!(
            std::fs::read(manual).expect("test precondition"),
            b"manual recovery copy"
        );
        assert_eq!(
            std::fs::read_dir(directory)
                .expect("test precondition")
                .count(),
            4
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn first_clear_preserves_an_unloaded_file_even_after_an_earlier_missing_clear() {
        let mut writer = writer(true);
        writer.clear();
        assert!(writer.protect_unloaded);
        std::fs::write(&writer.path, b"late layout").expect("test precondition");
        writer.clear();
        assert!(!writer.path.exists());
        assert_eq!(backups(&writer), vec![b"late layout".to_vec()]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn repeated_failed_saves_do_not_replace_a_completed_recovery_copy() {
        let mut writer = writer(true);
        std::fs::write(&writer.path, b"original").expect("test precondition");
        let temporary = writer.path.with_extension("json.tmp");
        std::fs::create_dir(&temporary).expect("test precondition");
        writer.save(&snapshot(), None);
        writer.save(&snapshot(), None);
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            b"original"
        );
        std::fs::remove_dir(&temporary).expect("test precondition");
        writer.save(&snapshot(), None);
        assert_eq!(backups(&writer), vec![b"original".to_vec()]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn interrupted_copy_is_not_published_as_a_recovery_file() {
        use std::io::Read;
        struct Interrupted;
        impl Read for Interrupted {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("interrupt the copy after writing its prefix");
            }
        }
        let writer = writer(true);
        let backup = writer
            .path
            .with_file_name("session-000000000000000000000000000000000000001-1-0.json");
        let mut source = io::Cursor::new(b"partial").chain(Interrupted);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                copy_recovery(&mut source, &backup)
            }))
            .is_err()
        );
        assert!(
            !backup.exists(),
            "an interrupted copy must not look complete"
        );
        assert_eq!(
            std::fs::read(backup.with_extension("pending")).expect("test precondition"),
            b"partial"
        );
        assert!(
            recovery_files(writer.path.parent().expect("test precondition"))
                .expect("test precondition")
                .is_empty()
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn recovery_order_survives_clock_rollback() {
        let mut writer = writer(true);
        let directory = writer.path.with_file_name("session-backups");
        std::fs::create_dir(&directory).expect("test precondition");
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test precondition")
            .as_nanos()
            + 1_000_000_000_000_000;
        for i in 0..2u8 {
            std::fs::write(
                directory.join(format!("session-{:039}-1-0.json", future + u128::from(i))),
                [i],
            )
            .expect("test precondition");
        }
        for i in 2..4u8 {
            writer.protect_unloaded = true;
            std::fs::write(&writer.path, [i]).expect("test precondition");
            writer.save(&snapshot(), None);
        }
        assert_eq!(backups(&writer), vec![vec![1], vec![2], vec![3]]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn recovery_keeps_three_copies_and_healthy_saves_do_not_rotate_them() {
        let mut writer = writer(true);
        for i in 0..5u8 {
            writer.protect_unloaded = true;
            std::fs::write(&writer.path, [i]).expect("test precondition");
            writer.save(&snapshot(), None);
        }
        assert_eq!(backups(&writer), vec![vec![2], vec![3], vec![4]]);
        writer.save(&snapshot(), None);
        writer.clear();
        assert_eq!(backups(&writer), vec![vec![2], vec![3], vec![4]]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn dangling_symlink_allows_first_save_and_late_target_is_preserved() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        for late_target in [false, true] {
            let mut writer = writer(true);
            let target = writer.path.with_file_name("target.json");
            symlink("target.json", &writer.path).expect("test precondition");
            if late_target {
                std::fs::write(&target, b"late layout").expect("test precondition");
            }
            writer.save(&snapshot(), None);
            assert!(
                std::fs::symlink_metadata(&writer.path)
                    .expect("test precondition")
                    .file_type()
                    .is_symlink()
            );
            assert!(target.exists());
            if late_target {
                assert_eq!(backups(&writer), vec![b"late layout".to_vec()]);
                let backup = std::fs::read_dir(writer.path.with_file_name("session-backups"))
                    .expect("test precondition")
                    .next()
                    .expect("test precondition")
                    .expect("test precondition")
                    .path();
                assert_eq!(
                    std::fs::metadata(backup)
                        .expect("test precondition")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            } else {
                assert!(backups(&writer).is_empty());
            }
            // A clear removes the session behind the link and keeps the link,
            // so the next save writes through it again.
            writer.clear();
            assert!(
                std::fs::symlink_metadata(&writer.path)
                    .expect("test precondition")
                    .file_type()
                    .is_symlink()
            );
            assert!(!target.exists());
            writer.save(&snapshot(), None);
            assert!(
                std::fs::symlink_metadata(&writer.path)
                    .expect("test precondition")
                    .file_type()
                    .is_symlink()
            );
            assert!(target.exists());
            std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
                .expect("test precondition");
        }
    }
}
