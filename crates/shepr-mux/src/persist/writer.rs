//! The save sequence: one layout save or clear with its paired history file,
//! and the recovery copies made around it. The files themselves are `files`,
//! the history serializer is `history` and the recovery copies are `recovery`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::error::SaveError;
use super::files;
use super::history::{
    HistoryDigest, HistoryTrim, SerializedHistory, SessionHistory, history_digest,
    serialize_history,
};
use super::recovery::{self, SessionBackupPolicy, SnapshotFingerprintCache, SnapshotHistoryPlan};
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

struct WrittenHistory {
    digest: HistoryDigest,
    file: shepr_platform::FileStamp,
}

/// What a save does to the history file, decided before the layout is
/// written, since the layout names the history it pairs with.
enum HistoryIntent {
    /// Delete it: history is not persisted. The layout names none.
    Remove,
    /// Replace it with these serialized bytes, unless it already holds
    /// exactly them. The layout names `digest`, the hash of `json`.
    Write {
        json: Vec<u8>,
        digest: HistoryDigest,
    },
    /// The caller knows it already holds the history `digest` names.
    Keep(HistoryDigest),
    /// The history could not be serialized: the layout names none, and the
    /// error is the save's.
    Failed(io::Error),
}

impl HistoryIntent {
    /// The digest the layout written with this intent names.
    fn digest(&self) -> Option<HistoryDigest> {
        match self {
            Self::Write { digest, .. } | Self::Keep(digest) => Some(*digest),
            Self::Remove | Self::Failed(_) => None,
        }
    }
}

/// Shared by autosave, pane-exit checkpoints, and shutdown.
pub struct SessionWriter {
    path: PathBuf,
    backup_policy: SessionBackupPolicy,
    _lease: super::lock::DataDirLease,
    /// Digest and file stamp for the history JSON this writer last put on
    /// disk. History is the bulk of a save (full scrollback per pane) and is
    /// rewritten and fsynced on every save otherwise, even when no pane printed
    /// anything.
    written_history: Option<WrittenHistory>,
    /// Reuses parsed layout fingerprints while the writer owns unchanged files.
    snapshot_fingerprints: SnapshotFingerprintCache,
    /// Whether the last history save had to trim scrollback or workspace
    /// shape to fit the file cap, so the log records transitions only.
    trimming_history: bool,
}

impl SessionWriter {
    /// Canonical file name for the saved session layout.
    pub const SESSION_FILE_NAME: &'static str = files::SESSION_FILE_NAME;

    pub fn new(lease: super::lock::DataDirLease, backup_policy: SessionBackupPolicy) -> Self {
        let path = files::session_path(lease.directory());
        Self {
            path,
            backup_policy,
            _lease: lease,
            written_history: None,
            snapshot_fingerprints: SnapshotFingerprintCache::default(),
            trimming_history: false,
        }
    }

    /// Consumes the writer and releases its data-directory lease.
    pub fn retire(self) {
        drop(self);
    }

    fn preserve_unloaded(&mut self, now: SystemTime) -> io::Result<()> {
        if self.backup_policy == SessionBackupPolicy::PreserveExisting
            && recovery::preserve_existing(&self.path, now)?
        {
            self.backup_policy = SessionBackupPolicy::NoBackupNeeded;
        }
        Ok(())
    }

    /// Saves the layout and optional history, reporting durability failures.
    /// An error may follow publication if syncing or writing the history fails.
    /// `now` supplies the time used for recovery-copy naming and preservation.
    /// On success, returns the digest the published layout names its history
    /// by, `None` when it has none.
    pub fn save(
        &mut self,
        snapshot: &SessionSnapshot,
        history: Option<&SessionHistory>,
        now: SystemTime,
    ) -> Result<Option<HistoryDigest>, SaveError> {
        let history = match history {
            None => HistoryIntent::Remove,
            Some(history) => self.prepare_history(history),
        };
        self.save_with(snapshot, history, now)
    }

    /// Saves the layout, naming the history by `digest`, and leaves the
    /// history file as it is. Only right when [`history_is_current`] and the
    /// history the caller would save is known to be the one `digest` names.
    ///
    /// [`history_is_current`]: Self::history_is_current
    pub fn save_keeping_history(
        &mut self,
        snapshot: &SessionSnapshot,
        digest: &HistoryDigest,
        now: SystemTime,
    ) -> Result<Option<HistoryDigest>, SaveError> {
        self.save_with(snapshot, HistoryIntent::Keep(*digest), now)
    }

    /// Serializes `history` (trimmed to the file cap) and hashes exactly the
    /// bytes that would be written.
    fn prepare_history(&mut self, history: &SessionHistory) -> HistoryIntent {
        let history_path = files::session_history_path(files::containing_directory(&self.path));
        match serialize_history(history) {
            Ok(SerializedHistory { json, trimmed }) => {
                self.note_history_trim(&history_path, trimmed);
                let digest = history_digest(&json);
                HistoryIntent::Write { json, digest }
            }
            Err(error) => HistoryIntent::Failed(error),
        }
    }

    /// Whether the history file is still exactly what this writer last put on
    /// disk (one `stat`), so a caller that knows its history has not changed
    /// since may skip assembling it.
    pub fn history_is_current(&self) -> bool {
        let Some(written) = &self.written_history else {
            return false;
        };
        let history_path = files::session_history_path(files::containing_directory(&self.path));
        files::regular_file_stamp(&history_path)
            .ok()
            .flatten()
            .is_some_and(|file| file == written.file)
    }

    fn save_with(
        &mut self,
        snapshot: &SessionSnapshot,
        history: HistoryIntent,
        now: SystemTime,
    ) -> Result<Option<HistoryDigest>, SaveError> {
        let mut snapshot_history_plan = SnapshotHistoryPlan::RetryAfterWrite;
        let result = self.preserve_unloaded(now).and_then(|()| {
            snapshot_history_plan = recovery::plan_snapshot_history(
                &self.path,
                snapshot,
                now,
                &mut self.snapshot_fingerprints,
            );
            let digest = history.digest();
            files::save_to_path(&self.path, snapshot, digest.as_ref())
        });
        self.finish_save_with_snapshot_plan(result, snapshot, history, snapshot_history_plan, now)
    }

    fn finish_save_with_snapshot_plan(
        &mut self,
        result: io::Result<files::Published>,
        snapshot: &SessionSnapshot,
        history: HistoryIntent,
        snapshot_history_plan: SnapshotHistoryPlan,
        now: SystemTime,
    ) -> Result<Option<HistoryDigest>, SaveError> {
        let digest = history.digest();
        let mut failure = None;
        if result.is_ok() {
            self.snapshot_fingerprints
                .remember_current(&self.path, snapshot);
        }
        match result {
            Ok(files::Published::Durable) => {}
            // The new layout already replaced the old file; only its
            // directory entry may not be on disk yet. That is still our
            // committed layout, so the history that pairs with it is written
            // too and the unloaded-file guard is released, exactly as for a
            // durable save.
            Ok(files::Published::NotDurable(err)) => {
                session_save_failed(
                    &self.path,
                    &format!("saved, but syncing its directory failed: {err}"),
                );
                failure = Some(SaveError::PublishedNotDurable(err));
            }
            Err(err) => {
                session_save_failed(&self.path, &err.to_string());
                return Err(SaveError::Io(err));
            }
        }
        // Optional history failure must not reclassify our committed layout as unloaded.
        self.backup_policy = SessionBackupPolicy::NoBackupNeeded;
        let history_path = files::session_history_path(files::containing_directory(&self.path));
        if let Err(err) = self.save_history(&history_path, history) {
            self.written_history = None;
            session_save_failed(&history_path, &err.to_string());
            if failure.is_none() {
                failure = Some(SaveError::Io(err));
            }
        } else {
            // After-write snapshots must include the history just committed
            // for their layout. Before-write snapshots were already copied
            // with the old history in `plan_snapshot_history`.
            recovery::finish_snapshot_history(
                &self.path,
                snapshot_history_plan,
                now,
                &mut self.snapshot_fingerprints,
            );
        }
        if failure.is_none() {
            session_saved(&self.path, snapshot.workspaces.len());
        }
        failure.map_or(Ok(digest), Err)
    }

    /// Writes the history unless the file already holds exactly these bytes
    /// from this writer's previous save. The layout names the history by the
    /// digest of these bytes, so a layout only ever pairs with the history its
    /// own save serialized.
    fn save_history(&mut self, history_path: &Path, history: HistoryIntent) -> io::Result<()> {
        let (json, digest) = match history {
            HistoryIntent::Keep(_) => return Ok(()),
            HistoryIntent::Remove => {
                self.written_history = None;
                return files::save_history_to_path(history_path, None);
            }
            HistoryIntent::Failed(error) => return Err(error),
            HistoryIntent::Write { json, digest } => (json, digest),
        };
        if let Some(written) = self
            .written_history
            .as_ref()
            .filter(|written| written.digest == digest)
            && files::regular_file_stamp(history_path)? == Some(written.file)
        {
            return Ok(());
        }
        self.written_history = None;
        files::save_history_json_to_path(history_path, &json)?;
        // Metadata only guards the optimization. If it cannot be captured
        // after a successful write, future saves simply publish the history
        // again rather than trusting a cache with no matching file stamp.
        self.written_history = files::regular_file_stamp(history_path)
            .ok()
            .flatten()
            .map(|file| WrittenHistory { digest, file });
        Ok(())
    }

    fn note_history_trim(&mut self, history_path: &Path, trimmed: Option<HistoryTrim>) {
        match (trimmed, self.trimming_history) {
            (Some(trim), false) => tracing::warn!(
                event = "persist.save", subsystem = "persist", outcome = "history_trimmed",
                path = %history_path.display(), panes = trim.panes,
                dropped_bytes = trim.dropped_bytes,
                structure_dropped = trim.structure_dropped,
                "session history exceeds its file cap; saving what fits"
            ),
            (None, true) => tracing::info!(
                event = "persist.save", subsystem = "persist", outcome = "history_fits",
                path = %history_path.display(),
                "session history fits its file cap again; saving all scrollback"
            ),
            (Some(_), true) | (None, false) => {}
        }
        self.trimming_history = trimmed.is_some();
    }

    /// Clears the layout and history, reporting either file's clear failure.
    /// `now` supplies the time used for recovery-copy naming and preservation.
    pub fn clear(&mut self, now: SystemTime) -> Result<(), SaveError> {
        self.written_history = None;
        let result = self.preserve_unloaded(now).and_then(|()| {
            recovery::preserve_snapshot_history(&self.path, now, &mut self.snapshot_fingerprints);
            files::clear_path(&self.path)
        });
        if let Err(err) = result {
            session_clear_failed(&self.path, &err.to_string());
            return Err(SaveError::Io(err));
        }
        let history_path = files::session_history_path(files::containing_directory(&self.path));
        if let Err(err) = files::clear_path(&history_path) {
            session_clear_failed(&history_path, &err.to_string());
            return Err(SaveError::Io(err));
        }
        self.snapshot_fingerprints.forget_current();
        session_cleared(&self.path);
        Ok(())
    }
}

#[cfg(test)]
impl SessionWriter {
    fn finish_save(
        &mut self,
        result: io::Result<files::Published>,
        snapshot: &SessionSnapshot,
        history: Option<&SessionHistory>,
    ) -> Result<(), SaveError> {
        let history = match history {
            None => HistoryIntent::Remove,
            Some(history) => self.prepare_history(history),
        };
        self.finish_save_with_snapshot_plan(
            result,
            snapshot,
            history,
            SnapshotHistoryPlan::RetryAfterWrite,
            std::time::UNIX_EPOCH,
        )
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::recovery::{
        SNAPSHOT_INTERVAL_FOR_TEST, SNAPSHOT_LIMIT_FOR_TEST, recovery_history_path_for_test,
        recovery_layouts_for_test,
    };
    use super::*;
    use crate::persist::history::HistoryText;
    use shepr_protocol::PanePublicNumber;
    use std::fs::File;
    use std::time::UNIX_EPOCH;

    impl SessionWriter {
        /// A save at the real current time, for tests whose subject is not
        /// the recovery cadence.
        fn save_for_test(
            &mut self,
            snapshot: &SessionSnapshot,
            history: Option<&SessionHistory>,
        ) -> Result<(), SaveError> {
            self.save(snapshot, history, SystemTime::now()).map(drop)
        }

        /// A clear at the real current time; see [`Self::save_for_test`].
        fn clear_for_test(&mut self) -> Result<(), SaveError> {
            self.clear(SystemTime::now())
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

    fn snapshot() -> SessionSnapshot {
        serde_json::from_value(serde_json::json!({
            "version": super::super::schema::SNAPSHOT_VERSION,
            "host_theme": super::super::schema::SavedHostTheme::default(),
            "workspaces": [{
                "id": "w1",
                "custom_name": null,
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

    fn number(value: usize) -> PanePublicNumber {
        PanePublicNumber::new(value).expect("nonzero literal")
    }

    /// A history of one pane's text in one workspace.
    fn history_of(text: &str) -> SessionHistory {
        SessionHistory {
            version: super::super::schema::SNAPSHOT_VERSION,
            workspaces: vec![vec![(
                number(1),
                HistoryText::single(std::sync::Arc::from(text)),
            )]],
        }
    }

    /// A history with no workspaces.
    fn empty_history() -> SessionHistory {
        SessionHistory {
            version: super::super::schema::SNAPSHOT_VERSION,
            workspaces: Vec::new(),
        }
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

    fn paired_history_backups(directory: &Path) -> Vec<Vec<u8>> {
        recovery_layouts_for_test(directory)
            .expect("test precondition")
            .into_iter()
            .filter_map(|(_, layout)| {
                let history = recovery_history_path_for_test(&layout).expect("valid recovery name");
                history
                    .try_exists()
                    .expect("test stat")
                    .then(|| std::fs::read(history).expect("test precondition"))
            })
            .collect()
    }

    #[test]
    fn snapshot_survives_exit_bursts_clears_and_writer_restarts() {
        use std::os::unix::fs::PermissionsExt;

        let mut writer = writer(false);
        let original = snapshot();
        writer.save_for_test(&original, None).expect("save");
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
            shrinking.workspaces[0].custom_name = Some(format!("remaining pane {i}"));
            writer.save_for_test(&shrinking, None).expect("save");
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
    fn session_snapshots_keep_history_with_the_layout_they_preserve() {
        let mut writer = writer(false);
        let history_path = files::session_history_path(files::containing_directory(&writer.path));
        files::save_to_path(&writer.path, &snapshot(), None).expect("test precondition");
        std::fs::write(&history_path, b"matching screen history").expect("test precondition");

        writer.save_for_test(&snapshot(), None).expect("save");

        let saved_layouts = snapshots(&writer);
        assert_eq!(saved_layouts.len(), 1);
        assert_eq!(
            std::fs::read(
                recovery_history_path_for_test(&saved_layouts[0].1).expect("valid recovery name")
            )
            .expect("paired history snapshot"),
            b"matching screen history"
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_history_is_bounded_and_does_not_rotate_identical_layouts() {
        let mut writer = writer(false);
        let directory = files::snapshot_directory(&writer.path);
        std::fs::create_dir(&directory).expect("test precondition");
        let manual = directory.join("my-layout.json");
        std::fs::write(&manual, b"manual").expect("test precondition");
        for i in 0..SNAPSHOT_LIMIT_FOR_TEST {
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
        writer.save_for_test(&snapshot(), None).expect("save");
        assert_eq!(snapshots(&writer).len(), SNAPSHOT_LIMIT_FOR_TEST);
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
        writer.save_for_test(&snapshot(), None).expect("save");
        assert_eq!(snapshots(&writer), vec![(1, old)]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_cadence_recovers_after_clock_rollback_and_restart() {
        let mut writer = writer(false);
        writer.save_for_test(&snapshot(), None).expect("save");
        // The supplied clock is one day behind the saved file's mtime.
        let saved_at = std::fs::metadata(&writer.path)
            .expect("session metadata")
            .modified()
            .expect("session mtime");
        let rolled_back = saved_at - std::time::Duration::from_secs(86400);
        let mut changed = snapshot();
        renumber_only_pane(&mut changed);
        writer.save(&changed, None, rolled_back).expect("save");
        assert_eq!(snapshots(&writer).len(), 2);
        let path = writer.path.clone();
        drop(writer);
        writer = SessionWriter::new(
            super::super::lock::DataDirLease::acquire(path.parent().expect("directory"))
                .expect("lease"),
            SessionBackupPolicy::NoBackupNeeded,
        );
        changed.workspaces[0].custom_name = Some("after restart".into());
        writer.save_for_test(&changed, None).expect("save");
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
        writer
            .save_for_test(&snapshot(), None)
            .expect("initial save");
        let latest = snapshots(&writer).pop().expect("initial snapshot").1;
        let modified = std::fs::metadata(latest)
            .expect("snapshot metadata")
            .modified()
            .expect("snapshot mtime");
        let mut changed = snapshot();
        renumber_only_pane(&mut changed);
        writer
            .save(
                &changed,
                None,
                modified + SNAPSHOT_INTERVAL_FOR_TEST - std::time::Duration::from_nanos(1),
            )
            .expect("save inside interval");
        assert_eq!(snapshots(&writer).len(), 1);
        writer
            .save(&changed, None, modified + SNAPSHOT_INTERVAL_FOR_TEST)
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
        writer.save_for_test(&snapshot(), None).expect("save");
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
                files::save_to_path(&writer.path, &snapshot(), None).expect("test precondition");
            }
            writer.save_for_test(&snapshot(), None).expect("save");
            assert_eq!(writer.backup_policy, SessionBackupPolicy::NoBackupNeeded);
            assert!(writer.path.try_exists().expect("test stat"));
            writer.save_for_test(&snapshot(), None).expect("save");
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
        let history_path = files::session_history_path(files::containing_directory(&writer.path));
        std::fs::write(&history_path, b"history").expect("test precondition");
        let directory = files::backup_directory(&writer.path);
        std::fs::write(&directory, b"blocks recovery").expect("test precondition");
        writer
            .save_for_test(&snapshot(), None)
            .expect_err("a blocked recovery copy must fail the save");
        writer
            .clear_for_test()
            .expect_err("a blocked recovery copy must fail the clear");
        assert_eq!(writer.backup_policy, SessionBackupPolicy::PreserveExisting);
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            original
        );
        assert_eq!(
            std::fs::read(&history_path).expect("test precondition"),
            b"history"
        );

        std::fs::remove_file(&directory).expect("test precondition");
        writer.save_for_test(&snapshot(), None).expect("save");
        assert_eq!(writer.backup_policy, SessionBackupPolicy::NoBackupNeeded);
        writer.save_for_test(&snapshot(), None).expect("save");
        writer.clear_for_test().expect("clear");
        assert!(!writer.path.try_exists().expect("test stat"));
        assert_eq!(backups(&writer), vec![original.to_vec()]);
        assert_eq!(
            paired_history_backups(&files::backup_directory(&writer.path)),
            vec![b"history".to_vec()]
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn optional_history_failure_is_reported_after_layout_saves() {
        let mut writer = writer(true);
        let history = files::session_history_path(files::containing_directory(&writer.path));
        std::fs::create_dir(&history).expect("test precondition");
        std::fs::write(files::backup_directory(&writer.path), b"unavailable")
            .expect("test precondition");
        assert!(writer.save_for_test(&snapshot(), None).is_err());
        assert!(
            writer.backup_policy == SessionBackupPolicy::NoBackupNeeded,
            "structural session was saved successfully"
        );
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("latest layout".into());
        assert!(writer.save_for_test(&changed, None).is_err());
        let saved: super::super::schema::SessionFile<SessionSnapshot> =
            serde_json::from_slice(&std::fs::read(&writer.path).expect("test precondition"))
                .expect("test precondition");
        assert_eq!(
            saved.snapshot.workspaces[0].custom_name.as_deref(),
            Some("latest layout")
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn unsynced_layout_save_still_writes_history_and_releases_the_guard() {
        let history = empty_history();
        let mut writer = writer(true);
        let history_path = files::session_history_path(files::containing_directory(&writer.path));
        // The layout rename happened; only the directory sync after it failed.
        files::save_to_path(&writer.path, &snapshot(), None).expect("test precondition");
        assert!(
            writer
                .finish_save(
                    Ok(files::Published::NotDurable(io::Error::other(
                        "directory sync failed",
                    ))),
                    &snapshot(),
                    Some(&history),
                )
                .is_err()
        );
        assert_eq!(writer.backup_policy, SessionBackupPolicy::NoBackupNeeded);
        assert!(
            history_path.try_exists().expect("test stat"),
            "history pairs with the new layout"
        );

        // A save that never reached the target changes nothing else.
        let path = writer.path.clone();
        drop(writer);
        let mut failed = SessionWriter::new(
            super::super::lock::DataDirLease::acquire(path.parent().expect("directory"))
                .expect("lease"),
            SessionBackupPolicy::PreserveExisting,
        );
        std::fs::remove_file(&history_path).expect("test precondition");
        assert!(
            failed
                .finish_save(
                    Err(io::Error::other("write failed")),
                    &snapshot(),
                    Some(&history),
                )
                .is_err()
        );
        assert_eq!(failed.backup_policy, SessionBackupPolicy::PreserveExisting);
        assert!(!history_path.try_exists().expect("test stat"));
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
        writer.save_for_test(&snapshot(), None).expect("save");
        let path = writer.path.clone();
        let lock_path = path.with_file_name(super::super::lock::LOCK_FILE_NAME);
        let lock = File::open(lock_path).expect("test precondition");
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        let saved = std::fs::read(&path).expect("test precondition");

        writer.retire();
        lock.try_lock().expect("the next server can take over");
        assert_eq!(std::fs::read(&path).expect("test precondition"), saved);
        drop(lock);
        std::fs::remove_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn unchanged_history_is_not_rewritten() {
        let mut writer = writer(false);
        let history_path = files::session_history_path(files::containing_directory(&writer.path));
        writer
            .save_for_test(&snapshot(), Some(&history_of("one")))
            .expect("save");
        // A rewrite replaces the file, so the inode tells whether one happened.
        // (A chmod would not do as a marker: the stamp includes the change
        // time, so it would itself count as an outside change.)
        use std::os::unix::fs::MetadataExt;
        let inode = || {
            std::fs::metadata(&history_path)
                .expect("test precondition")
                .ino()
        };
        let written = inode();
        writer
            .save_for_test(&snapshot(), Some(&history_of("one")))
            .expect("save");
        assert_eq!(
            inode(),
            written,
            "unchanged history should keep the file written by the previous save"
        );

        writer
            .save_for_test(&snapshot(), Some(&history_of("two")))
            .expect("save");
        let changed = std::fs::read(&history_path).expect("test precondition");
        assert!(String::from_utf8_lossy(&changed).contains("two"));

        // A clear forgets what was written, so the same history is written
        // again afterwards.
        writer.clear_for_test().expect("clear");
        assert!(!history_path.try_exists().expect("test stat"));
        writer
            .save_for_test(&snapshot(), Some(&history_of("two")))
            .expect("save");
        assert_eq!(
            std::fs::read(&history_path).expect("test precondition"),
            changed
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn every_layout_names_the_digest_of_the_history_it_pairs_with() {
        let history = history_of("screen");
        let mut writer = writer(false);
        let history_path = files::session_history_path(files::containing_directory(&writer.path));
        let named = |writer: &SessionWriter| {
            let layout: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&writer.path).expect("layout"))
                    .expect("layout json");
            layout["history_digest"].as_str().map(str::to_owned)
        };
        let file_digest =
            || history_digest(&std::fs::read(&history_path).expect("history")).to_hex();

        let digest = writer
            .save(&snapshot(), Some(&history), SystemTime::now())
            .expect("save")
            .expect("a history was saved");
        let digest_hex = digest.to_hex();
        assert_eq!(named(&writer).as_deref(), Some(digest_hex.as_str()));
        assert_eq!(file_digest(), digest_hex);

        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("layout only".into());
        let kept = writer
            .save_keeping_history(&changed, &digest, SystemTime::now())
            .expect("save");
        assert_eq!(kept, Some(digest));
        assert_eq!(named(&writer).as_deref(), Some(digest_hex.as_str()));
        assert_eq!(file_digest(), digest_hex);

        assert_eq!(
            writer
                .save(&changed, None, SystemTime::now())
                .expect("save"),
            None
        );
        assert_eq!(named(&writer), None, "a layout without history names none");
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn a_history_path_that_is_not_a_regular_file_costs_only_history() {
        let history = empty_history();
        let mut writer = writer(true);
        std::fs::write(&writer.path, b"unloaded session").expect("test precondition");
        let history_path = files::session_history_path(files::containing_directory(&writer.path));
        std::fs::create_dir(&history_path).expect("test precondition");
        // The unloaded session is still preserved, without its history, and
        // the layout is saved; only the history write fails.
        assert!(writer.save_for_test(&snapshot(), Some(&history)).is_err());
        assert_eq!(writer.backup_policy, SessionBackupPolicy::NoBackupNeeded);
        assert_eq!(backups(&writer), vec![b"unloaded session".to_vec()]);
        assert!(
            std::fs::metadata(&history_path)
                .expect("test stat")
                .is_dir(),
            "the obstruction is left alone"
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn deleted_unchanged_history_is_written_again() {
        let mut writer = writer(false);
        let history_path = files::session_history_path(files::containing_directory(&writer.path));
        writer
            .save_for_test(&snapshot(), Some(&empty_history()))
            .expect("save");
        let expected = std::fs::read(&history_path).expect("test precondition");

        std::fs::remove_file(&history_path).expect("test precondition");
        writer
            .save_for_test(&snapshot(), Some(&empty_history()))
            .expect("save after history deletion");

        assert_eq!(
            std::fs::read(&history_path).expect("history is restored"),
            expected
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
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
            writer.save_for_test(&snapshot(), None).expect("save");
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
            .save_for_test(&snapshot(), None)
            .expect_err("an unwritable data directory must fail the save");
        writer
            .save_for_test(&snapshot(), None)
            .expect_err("an unwritable data directory must fail the save");
        std::fs::set_permissions(&data_directory, std::fs::Permissions::from_mode(0o700))
            .expect("test cleanup");
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            b"original"
        );
        writer.save_for_test(&snapshot(), None).expect("save");
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
            writer.save_for_test(&snapshot(), None).expect("save");
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
            let history_path =
                files::session_history_path(files::containing_directory(&writer.path));
            std::fs::write(&history_path, [i + 10]).expect("test precondition");
            writer.save_for_test(&snapshot(), None).expect("save");
        }
        assert_eq!(backups(&writer), vec![vec![2], vec![3], vec![4]]);
        assert_eq!(
            paired_history_backups(&files::backup_directory(&writer.path)),
            vec![vec![12], vec![13], vec![14]]
        );
        writer.save_for_test(&snapshot(), None).expect("save");
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
            writer.save_for_test(&snapshot(), None).expect("save");
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
            writer.save_for_test(&snapshot(), None).expect("save");
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
