//! The save sequence: one layout save or clear, and the recovery copies made
//! around it. The files themselves are `files` and the recovery copies are
//! `recovery`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::error::SaveError;
use super::files;
use super::recovery::{self, SessionBackupPolicy, SnapshotPlan, SnapshotState};
use super::schema::SessionSnapshot;

// The session save and clear events. These literals are the emitted log
// schema, so the names stay visible at the event site.

fn session_saved(path: &Path, workspaces: usize) {
    tracing::info!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        workspaces,
        "session saved"
    );
}

fn session_save_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        error = err,
        "failed to save session"
    );
}

fn session_cleared(path: &Path) {
    tracing::info!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        "session cleared"
    );
}

fn session_clear_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        error = err,
        "failed to clear session"
    );
}

/// Shared by autosave, pane-exit checkpoints, and shutdown.
pub(super) struct SessionWriter {
    path: PathBuf,
    backup_policy: SessionBackupPolicy,
    _lease: super::lock::DataDirLease,
    /// The session file's and the newest snapshot's layouts, which the
    /// snapshot cadence decides from.
    snapshot_fingerprints: SnapshotState,
}

impl SessionWriter {
    pub(super) fn new(
        lease: super::lock::DataDirLease,
        backup_policy: SessionBackupPolicy,
    ) -> Self {
        let path = files::session_path(lease.directory());
        Self {
            path,
            backup_policy,
            _lease: lease,
            snapshot_fingerprints: SnapshotState::default(),
        }
    }

    fn preserve_unloaded(&mut self, now: SystemTime) -> Result<(), SaveError> {
        self.preserve_unloaded_with(now, recovery::preserve_existing)
    }

    fn preserve_unloaded_with(
        &mut self,
        now: SystemTime,
        preserve_existing: impl FnOnce(&Path, SystemTime) -> Result<bool, recovery::BackupError>,
    ) -> Result<(), SaveError> {
        if self.backup_policy != SessionBackupPolicy::PreserveExisting {
            return Ok(());
        }
        match preserve_existing(&self.path, now) {
            Ok(true) => self.backup_policy = SessionBackupPolicy::NoBackupNeeded,
            Ok(false) => {}
            Err(recovery::BackupError::SourceUnreadable(error)) => {
                return Err(SaveError::BlockedOnBackup(error));
            }
            Err(recovery::BackupError::Io(error)) => return Err(SaveError::Io(error)),
        }
        Ok(())
    }

    /// Saves the layout, reporting durability failures: an error may follow
    /// publication if syncing its directory fails. `now` supplies the time
    /// used for recovery-copy naming and preservation.
    pub(super) fn save(
        &mut self,
        snapshot: &SessionSnapshot,
        now: SystemTime,
    ) -> Result<(), SaveError> {
        self.save_with_preserve(snapshot, now, recovery::preserve_existing)
    }

    fn save_with_preserve(
        &mut self,
        snapshot: &SessionSnapshot,
        now: SystemTime,
        preserve_existing: impl FnOnce(&Path, SystemTime) -> Result<bool, recovery::BackupError>,
    ) -> Result<(), SaveError> {
        if let Err(error) = self.preserve_unloaded_with(now, preserve_existing) {
            session_save_failed(&self.path, &error.to_string());
            return Err(error);
        }
        let snapshot_plan =
            recovery::plan_snapshot(&self.path, snapshot, now, &mut self.snapshot_fingerprints);
        let result = files::save_to_path(&self.path, snapshot);
        self.finish_save(result, snapshot, snapshot_plan, now)
    }

    fn finish_save(
        &mut self,
        result: io::Result<files::Published>,
        snapshot: &SessionSnapshot,
        snapshot_plan: SnapshotPlan,
        now: SystemTime,
    ) -> Result<(), SaveError> {
        let failure = match result {
            Ok(files::Published::Durable) => None,
            // The new layout already replaced the old file; only its
            // directory entry may not be on disk yet. That is still our
            // committed layout, so the unloaded-file guard is released and
            // the snapshot step runs, exactly as for a durable save.
            Ok(files::Published::NotDurable(err)) => {
                tracing::warn!(
                    event = "persist.save", subsystem = "persist", outcome = "not_durable",
                    path = %self.path.display(), error = %err,
                    "session saved but not confirmed durable"
                );
                Some(SaveError::PublishedNotDurable(err))
            }
            Err(err) => {
                session_save_failed(&self.path, &err.to_string());
                return Err(SaveError::Io(err));
            }
        };
        self.snapshot_fingerprints.remember_current(snapshot);
        self.backup_policy = SessionBackupPolicy::NoBackupNeeded;
        recovery::finish_snapshot(
            &self.path,
            snapshot_plan,
            now,
            &mut self.snapshot_fingerprints,
        );
        match failure {
            None => {
                session_saved(&self.path, snapshot.workspaces.len());
                Ok(())
            }
            Some(failure) => Err(failure),
        }
    }

    /// Clears the layout, reporting a clear failure. `now` supplies the time
    /// used for recovery-copy naming and preservation.
    pub(super) fn clear(&mut self, now: SystemTime) -> Result<(), SaveError> {
        self.clear_with_operation(now, files::clear_path)
    }

    fn clear_with_operation(
        &mut self,
        now: SystemTime,
        clear: impl FnOnce(&Path) -> io::Result<files::ClearOutcome>,
    ) -> Result<(), SaveError> {
        if let Err(error) = self.preserve_unloaded(now) {
            session_clear_failed(&self.path, &error.to_string());
            return Err(error);
        }
        recovery::preserve_snapshot(&self.path, now, &mut self.snapshot_fingerprints);
        let result = clear(&self.path);
        match result {
            Ok(files::ClearOutcome::Durable) => {
                self.snapshot_fingerprints.forget_current();
                session_cleared(&self.path);
                Ok(())
            }
            Ok(files::ClearOutcome::NotDurable(err)) => {
                self.snapshot_fingerprints.forget_current();
                session_clear_failed(
                    &self.path,
                    &format!("session was cleared, but syncing its directory failed: {err}"),
                );
                Err(SaveError::PublishedNotDurable(err))
            }
            Err(err) => {
                session_clear_failed(&self.path, &err.to_string());
                Err(SaveError::Io(err))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::recovery::recovery_layouts_for_test;
    use super::*;
    use crate::limits::{SNAPSHOT_INTERVAL, SNAPSHOT_LIMIT};
    use std::fs::File;
    use std::time::UNIX_EPOCH;

    impl SessionWriter {
        /// A save at the real current time, for tests whose subject is not
        /// the recovery cadence.
        fn save_for_test(&mut self, snapshot: &SessionSnapshot) -> Result<(), SaveError> {
            self.save(snapshot, SystemTime::now())
        }

        /// A clear at the real current time; see [`Self::save_for_test`].
        fn clear_for_test(&mut self) -> Result<(), SaveError> {
            self.clear(SystemTime::now())
        }

        /// A clear at the real current time whose directory sync is
        /// `sync_directory`, so a test can make the sync fail.
        fn clear_with_directory_sync_for_test(
            &mut self,
            sync_directory: impl FnMut(&Path) -> io::Result<()>,
        ) -> Result<(), SaveError> {
            self.clear_with_operation(SystemTime::now(), move |path| {
                files::clear_path_with_directory_sync(path, sync_directory)
            })
        }
    }

    fn writer(preserve_existing: bool) -> SessionWriter {
        let directory = crate::test_support::ScratchDir::new("session-recovery");
        SessionWriter::new(
            super::super::lock::DataDirLease::acquire(&directory).expect("lease"),
            if preserve_existing {
                SessionBackupPolicy::PreserveExisting
            } else {
                SessionBackupPolicy::NoBackupNeeded
            },
        )
    }

    fn test_name(name: &str) -> crate::terminal::Label {
        crate::terminal::Label::new(name).expect("test workspace name")
    }

    fn snapshot() -> SessionSnapshot {
        serde_json::from_value(serde_json::json!({
            "version": super::super::schema::SNAPSHOT_VERSION,
            "host_theme": super::super::schema::SavedHostTheme::default(),
            "workspaces": [{
                "id": "w1",
                "name": "w",
                "next_public_pane_number": 2,
                "layout": {
                    "Pane": {
                        "cwd": "/shepr-writer-test",
                        "public_number": 1,
                        "label": null
                    }
                },
                "zoomed": false,
                "focused": 1,
                "root_pane": 1
            }],
            "active": 0
        }))
        .expect("test precondition")
    }

    /// Gives the fixture's only pane another public number, which changes
    /// the layout the recovery cadence compares.
    fn renumber_only_pane(snapshot: &mut SessionSnapshot) {
        let workspace = &mut snapshot.workspaces[0];
        let second = shepr_protocol::PanePublicNumber::new(2).expect("nonzero literal");
        let super::super::schema::LayoutSnapshot::Pane(pane) = &mut workspace.layout else {
            panic!("the fixture is a single pane");
        };
        pane.public_number = second;
        workspace.next_public_pane_number = second.checked_next().expect("a successor");
        workspace.focused = second;
        workspace.root_pane = second;
    }

    fn backups(writer: &SessionWriter) -> Vec<Vec<u8>> {
        let directory = files::backup_directory(&writer.path);
        if !directory.try_exists().expect("test stat") {
            return Vec::new();
        }
        recovery_layouts_for_test(&directory)
            .expect("test precondition")
            .into_iter()
            .map(|(_, path)| std::fs::read(path).expect("test precondition"))
            .collect()
    }

    fn snapshots(writer: &SessionWriter) -> Vec<(u128, PathBuf)> {
        recovery_layouts_for_test(&files::snapshot_directory(&writer.path))
            .expect("test precondition")
    }

    #[test]
    fn snapshot_survives_exit_bursts_clears_and_writer_restarts() {
        use std::os::unix::fs::PermissionsExt;

        let mut writer = writer(false);
        let original = snapshot();
        writer.save_for_test(&original).expect("save");
        let files = snapshots(&writer);
        assert_eq!(files.len(), 1);
        let snapshot_directory = super::super::files::snapshot_directory(&writer.path);
        assert_eq!(
            std::fs::metadata(snapshot_directory)
                .expect("snapshot directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let saved = std::fs::read(&files[0].1).expect("test precondition");
        for i in 0..100 {
            let mut shrinking = snapshot();
            shrinking.workspaces[0].name = test_name(&format!("remaining pane {i}"));
            writer.save_for_test(&shrinking).expect("save");
            let path = writer.path.clone();
            drop(writer);
            writer = SessionWriter::new(
                super::super::lock::DataDirLease::acquire(path.parent().expect("directory"))
                    .expect("lease"),
                SessionBackupPolicy::NoBackupNeeded,
            );
        }
        writer.clear_for_test().expect("clear");
        assert!(
            !writer.path.try_exists().expect("test stat"),
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
    fn a_retried_clear_syncs_the_directory_after_the_first_sync_failed() {
        use std::cell::Cell;

        let mut writer = writer(false);
        writer
            .save_for_test(&snapshot())
            .expect("save before clear");
        let sync_calls = Cell::new(0);

        let first = writer.clear_with_directory_sync_for_test(|_| {
            sync_calls.set(sync_calls.get() + 1);
            Err(io::Error::other("directory sync failed"))
        });
        assert!(matches!(first, Err(SaveError::PublishedNotDurable(_))));
        assert!(!writer.path.try_exists().expect("test stat"));

        let retry = writer.clear_with_directory_sync_for_test(|_| {
            sync_calls.set(sync_calls.get() + 1);
            Ok(())
        });
        assert!(retry.is_ok(), "the retry confirms the directory state");
        assert_eq!(sync_calls.get(), 2, "NotFound still syncs the directory");
    }

    #[test]
    fn snapshot_history_is_bounded_and_does_not_rotate_identical_layouts() {
        let mut writer = writer(false);
        let directory = files::snapshot_directory(&writer.path);
        std::fs::create_dir(&directory).expect("test precondition");
        let manual = directory.join("my-layout.json");
        std::fs::write(&manual, b"manual").expect("test precondition");
        for i in 0..SNAPSHOT_LIMIT {
            std::fs::write(
                directory.join(format!("session-{i:039}-000.json")),
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
        writer.save_for_test(&snapshot()).expect("save");
        assert_eq!(snapshots(&writer).len(), SNAPSHOT_LIMIT);
        assert!(
            !directory
                .join(format!("session-{:039}-000.json", 0))
                .try_exists()
                .expect("test stat")
        );
        assert!(manual.try_exists().expect("test stat"));

        for (_, path) in snapshots(&writer) {
            std::fs::remove_file(path).expect("test precondition");
        }
        let old = directory.join(format!("session-{:039}-000.json", 1));
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
        let path = writer.path.clone();
        drop(writer);
        writer = SessionWriter::new(
            super::super::lock::DataDirLease::acquire(path.parent().expect("directory"))
                .expect("lease"),
            SessionBackupPolicy::NoBackupNeeded,
        );
        writer.save_for_test(&snapshot()).expect("save");
        assert_eq!(snapshots(&writer), vec![(1, old)]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_cadence_recovers_after_clock_rollback_and_restart() {
        let mut writer = writer(false);
        writer.save_for_test(&snapshot()).expect("save");
        // The supplied clock is one day behind the saved file's mtime.
        let saved_at = std::fs::metadata(&writer.path)
            .expect("session metadata")
            .modified()
            .expect("session mtime");
        let rolled_back = saved_at - std::time::Duration::from_secs(86400);
        let mut changed = snapshot();
        renumber_only_pane(&mut changed);
        writer.save(&changed, rolled_back).expect("save");
        assert_eq!(snapshots(&writer).len(), 2);
        let path = writer.path.clone();
        drop(writer);
        writer = SessionWriter::new(
            super::super::lock::DataDirLease::acquire(path.parent().expect("directory"))
                .expect("lease"),
            SessionBackupPolicy::NoBackupNeeded,
        );
        changed.workspaces[0].name = test_name("after restart");
        writer.save_for_test(&changed).expect("save");
        assert_eq!(
            snapshots(&writer).len(),
            2,
            "new mtime restores cadence across restart"
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_interval_uses_supplied_clock() {
        let mut writer = writer(false);
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1000);
        writer.save(&snapshot(), now).expect("initial save");
        let mut changed = snapshot();
        renumber_only_pane(&mut changed);
        writer
            .save(
                &changed,
                now + SNAPSHOT_INTERVAL - std::time::Duration::from_nanos(1),
            )
            .expect("save inside interval");
        assert_eq!(snapshots(&writer).len(), 1);
        writer
            .save(&changed, now + SNAPSHOT_INTERVAL)
            .expect("save at interval");
        assert_eq!(snapshots(&writer).len(), 2);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_failure_does_not_block_primary_save_and_clear() {
        let mut writer = writer(false);
        std::fs::write(files::snapshot_directory(&writer.path), b"blocked")
            .expect("test precondition");
        writer.save_for_test(&snapshot()).expect("save");
        assert!(writer.path.try_exists().expect("test stat"));
        writer.clear_for_test().expect("clear");
        assert!(!writer.path.try_exists().expect("test stat"));
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn healthy_and_fresh_sessions_save_and_clear_without_backups() {
        for preserve_existing in [false, true] {
            let mut writer = writer(preserve_existing);
            if !preserve_existing {
                files::save_to_path(&writer.path, &snapshot()).expect("test precondition");
            }
            writer.save_for_test(&snapshot()).expect("save");
            assert_eq!(writer.backup_policy, SessionBackupPolicy::NoBackupNeeded);
            assert!(writer.path.try_exists().expect("test stat"));
            writer.save_for_test(&snapshot()).expect("save");
            writer.clear_for_test().expect("clear");
            assert!(!writer.path.try_exists().expect("test stat"));
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
        let directory = files::backup_directory(&writer.path);
        std::fs::write(&directory, b"blocks recovery").expect("test precondition");
        writer
            .save_for_test(&snapshot())
            .expect_err("a blocked recovery copy must fail the save");
        writer
            .clear_for_test()
            .expect_err("a blocked recovery copy must fail the clear");
        assert_eq!(writer.backup_policy, SessionBackupPolicy::PreserveExisting);
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            original
        );

        std::fs::remove_file(&directory).expect("test precondition");
        writer.save_for_test(&snapshot()).expect("save");
        assert_eq!(writer.backup_policy, SessionBackupPolicy::NoBackupNeeded);
        writer.save_for_test(&snapshot()).expect("save");
        writer.clear_for_test().expect("clear");
        assert!(!writer.path.try_exists().expect("test stat"));
        assert_eq!(backups(&writer), vec![original.to_vec()]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn an_unreadable_source_blocks_before_replacing_the_existing_session() {
        let original = b"unreadable session source";
        let mut writer = writer(true);
        std::fs::write(&writer.path, original).expect("test precondition");

        let result = writer.save_with_preserve(&snapshot(), SystemTime::now(), |_, _| {
            Err(recovery::BackupError::SourceUnreadable(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission denied",
            )))
        });

        assert!(matches!(result, Err(SaveError::BlockedOnBackup(_))));
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            original
        );
        assert_eq!(writer.backup_policy, SessionBackupPolicy::PreserveExisting);
        assert!(backups(&writer).is_empty());
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test cleanup");
    }

    #[test]
    fn unsynced_layout_save_releases_the_guard() {
        let mut writer = writer(true);
        // The layout rename happened; only the directory sync after it failed.
        files::save_to_path(&writer.path, &snapshot()).expect("test precondition");
        assert!(
            writer
                .finish_save(
                    Ok(files::Published::NotDurable(io::Error::other(
                        "directory sync failed",
                    ))),
                    &snapshot(),
                    SnapshotPlan::PreserveAfterWrite,
                    UNIX_EPOCH,
                )
                .is_err()
        );
        assert_eq!(writer.backup_policy, SessionBackupPolicy::NoBackupNeeded);

        // A save that never reached the target changes nothing.
        let path = writer.path.clone();
        drop(writer);
        let mut failed = SessionWriter::new(
            super::super::lock::DataDirLease::acquire(path.parent().expect("directory"))
                .expect("lease"),
            SessionBackupPolicy::PreserveExisting,
        );
        assert!(
            failed
                .finish_save(
                    Err(io::Error::other("write failed")),
                    &snapshot(),
                    SnapshotPlan::PreserveAfterWrite,
                    UNIX_EPOCH,
                )
                .is_err()
        );
        assert_eq!(failed.backup_policy, SessionBackupPolicy::PreserveExisting);
        std::fs::remove_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn writer_requires_an_acquired_lease() {
        let scratch = crate::test_support::ScratchDir::new("writer-lease");
        let directory = scratch.join("data");
        let _writer = SessionWriter::new(
            super::super::lock::DataDirLease::acquire(&directory).expect("lease"),
            SessionBackupPolicy::NoBackupNeeded,
        );
        assert_eq!(
            super::super::lock::DataDirLease::acquire(&directory)
                .err()
                .map(|err| err.kind()),
            Some(io::ErrorKind::ResourceBusy)
        );
    }

    #[test]
    fn retiring_consumes_the_writer_and_releases_the_directory() {
        let mut writer = writer(false);
        writer.save_for_test(&snapshot()).expect("save");
        let path = writer.path.clone();
        let lock_path = path.with_file_name(shepr_paths::DATA_DIR_LEASE_FILE_NAME);
        let lock = File::open(lock_path).expect("test precondition");
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        let saved = std::fs::read(&path).expect("test precondition");

        drop(writer);
        lock.try_lock().expect("the next server can take over");
        assert_eq!(std::fs::read(&path).expect("test precondition"), saved);
        drop(lock);
        std::fs::remove_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn pruning_leaves_user_named_recovery_files_alone() {
        let mut writer = writer(true);
        let directory = files::backup_directory(&writer.path);
        std::fs::create_dir(&directory).expect("test precondition");
        let manual = directory.join("session-000-manual.json");
        std::fs::write(&manual, b"manual recovery copy").expect("test precondition");
        for i in 0..5u8 {
            writer.backup_policy = SessionBackupPolicy::PreserveExisting;
            std::fs::write(&writer.path, [i]).expect("test precondition");
            writer.save_for_test(&snapshot()).expect("save");
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
        writer.clear_for_test().expect("clear");
        assert_eq!(writer.backup_policy, SessionBackupPolicy::PreserveExisting);
        std::fs::write(&writer.path, b"late layout").expect("test precondition");
        writer.clear_for_test().expect("clear");
        assert!(!writer.path.try_exists().expect("test stat"));
        assert_eq!(backups(&writer), vec![b"late layout".to_vec()]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn repeated_failed_saves_do_not_replace_a_completed_recovery_copy() {
        use std::os::unix::fs::PermissionsExt;

        let mut writer = writer(true);
        std::fs::write(&writer.path, b"original").expect("test precondition");
        let data_directory = writer
            .path
            .parent()
            .expect("test precondition")
            .to_path_buf();
        // The recovery directories exist already, so what fails below is the
        // layout publish into the unwritable data directory, after the
        // unloaded session was preserved.
        std::fs::create_dir(files::backup_directory(&writer.path)).expect("test precondition");
        std::fs::create_dir(files::snapshot_directory(&writer.path)).expect("test precondition");
        std::fs::set_permissions(&data_directory, std::fs::Permissions::from_mode(0o500))
            .expect("test precondition");
        let probe = data_directory.join("probe");
        if std::fs::write(&probe, b"").is_ok() {
            // A privileged runner writes there anyway; there is no failure
            // to observe.
            std::fs::remove_file(&probe).expect("test cleanup");
            std::fs::set_permissions(&data_directory, std::fs::Permissions::from_mode(0o700))
                .expect("test cleanup");
            return;
        }
        writer
            .save_for_test(&snapshot())
            .expect_err("an unwritable data directory must fail the save");
        writer
            .save_for_test(&snapshot())
            .expect_err("an unwritable data directory must fail the save");
        std::fs::set_permissions(&data_directory, std::fs::Permissions::from_mode(0o700))
            .expect("test cleanup");
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            b"original"
        );
        writer.save_for_test(&snapshot()).expect("save");
        assert_eq!(backups(&writer), vec![b"original".to_vec()]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn recovery_order_survives_clock_rollback() {
        let mut writer = writer(true);
        let directory = files::backup_directory(&writer.path);
        std::fs::create_dir(&directory).expect("test precondition");
        let future = 1_000_000_000_000_000_000_000u128;
        for i in 0..2u8 {
            std::fs::write(
                directory.join(format!("session-{:039}-000.json", future + u128::from(i))),
                [i],
            )
            .expect("test precondition");
        }
        for i in 2..4u8 {
            writer.backup_policy = SessionBackupPolicy::PreserveExisting;
            std::fs::write(&writer.path, [i]).expect("test precondition");
            writer.save_for_test(&snapshot()).expect("save");
        }
        assert_eq!(backups(&writer), vec![vec![1], vec![2], vec![3]]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn recovery_keeps_three_copies_and_healthy_saves_do_not_rotate_them() {
        let mut writer = writer(true);
        for i in 0..5u8 {
            writer.backup_policy = SessionBackupPolicy::PreserveExisting;
            std::fs::write(&writer.path, [i]).expect("test precondition");
            writer.save_for_test(&snapshot()).expect("save");
        }
        assert_eq!(backups(&writer), vec![vec![2], vec![3], vec![4]]);
        writer.save_for_test(&snapshot()).expect("save");
        writer.clear_for_test().expect("clear");
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
            writer.save_for_test(&snapshot()).expect("save");
            assert!(
                std::fs::symlink_metadata(&writer.path)
                    .expect("test precondition")
                    .file_type()
                    .is_symlink()
            );
            assert!(target.try_exists().expect("test stat"));
            if late_target {
                assert_eq!(backups(&writer), vec![b"late layout".to_vec()]);
                let backup = std::fs::read_dir(files::backup_directory(&writer.path))
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
            writer.clear_for_test().expect("clear");
            assert!(
                std::fs::symlink_metadata(&writer.path)
                    .expect("test precondition")
                    .file_type()
                    .is_symlink()
            );
            assert!(!target.try_exists().expect("test stat"));
            writer.save_for_test(&snapshot()).expect("save");
            assert!(
                std::fs::symlink_metadata(&writer.path)
                    .expect("test precondition")
                    .file_type()
                    .is_symlink()
            );
            assert!(target.try_exists().expect("test stat"));
            std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
                .expect("test precondition");
        }
    }
}
