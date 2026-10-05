//! Recovery copies of the saved session: the backup a first save or clear
//! makes of a file restore could not fully use, and the periodic layout
//! snapshots kept as recovery points. This file decides when a copy is due,
//! writes it, and prunes old copies; the save sequence that calls it is in
//! `writer`.

use std::io::{self, Seek};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::files;
use super::schema::{DirectionSnapshot, LayoutSnapshot, SessionSnapshot};
use crate::limits::{BACKUP_LIMIT, RECOVERY_SEQUENCE_LIMIT, SNAPSHOT_INTERVAL, SNAPSHOT_LIMIT};

/// Whether the source file must be copied before the writer replaces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionBackupPolicy {
    /// Preserve the existing session file before the first replacement.
    PreserveExisting,
    /// Restore used the file in full, or there is no source file to preserve.
    NoBackupNeeded,
}

// The layout's shape and each pane's public number, which tell whether two
// layouts are the same for recovery-copy cadence (the writer's snapshot
// history). It is compared only in memory and never stored, so the encoding
// itself is the fingerprint.
#[derive(Clone, PartialEq, Eq)]
struct LayoutFingerprint(Vec<u8>);

fn layout_fingerprint(snapshot: &SessionSnapshot) -> LayoutFingerprint {
    // The encoding uses tagged tree nodes and fixed-width little-endian counts,
    // numbers, and ratios. Each leaf carries its pane's public number; other
    // saved fields (cwds, labels, agent sessions) do not count.
    let mut encoding = Vec::new();
    append_fingerprint_count(snapshot.workspaces.len(), &mut encoding);
    for workspace in &snapshot.workspaces {
        append_layout_fingerprint(&workspace.layout, &mut encoding);
    }
    LayoutFingerprint(encoding)
}

fn append_fingerprint_count(count: usize, encoding: &mut Vec<u8>) {
    encoding.extend_from_slice(&count.to_le_bytes());
}

fn append_layout_fingerprint(layout: &LayoutSnapshot, encoding: &mut Vec<u8>) {
    match layout {
        LayoutSnapshot::Pane(pane) => {
            encoding.push(0);
            append_fingerprint_count(pane.public_number.get(), encoding);
        }
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            encoding.push(1);
            encoding.push(match direction {
                DirectionSnapshot::Horizontal => 0,
                DirectionSnapshot::Vertical => 1,
            });
            encoding.extend_from_slice(&ratio.get().to_bits().to_le_bytes());
            append_layout_fingerprint(first, encoding);
            append_layout_fingerprint(second, encoding);
        }
    }
}

struct CachedSnapshotLayout {
    path: PathBuf,
    stamp: Option<shepr_platform::FileStamp>,
    layout: Option<SavedLayout>,
}

#[derive(Clone, PartialEq, Eq)]
enum SavedLayout {
    Empty,
    Known(LayoutFingerprint),
    Unknown,
}

/// Parsed layout fingerprints of the session file and of the newest snapshot,
/// reused while the writer owns unchanged files.
#[derive(Default)]
pub(super) struct SnapshotFingerprintCache {
    current: Option<CachedSnapshotLayout>,
    latest: Option<CachedSnapshotLayout>,
}

impl SnapshotFingerprintCache {
    fn current(&mut self, path: &Path) -> io::Result<Option<SavedLayout>> {
        Self::read_or_reuse(path, &mut self.current)
    }

    fn latest(&mut self, path: &Path) -> io::Result<Option<SavedLayout>> {
        Self::read_or_reuse(path, &mut self.latest)
    }

    fn read_or_reuse(
        path: &Path,
        cached: &mut Option<CachedSnapshotLayout>,
    ) -> io::Result<Option<SavedLayout>> {
        let stamp = files::regular_file_stamp(path)?;
        if let Some(cached) = cached
            && cached.path.as_path() == path
            && cached.stamp == stamp
        {
            return Ok(cached.layout.clone());
        }
        // The writer owns the session lease, so its own publications update
        // these entries directly. Metadata avoids rereading unchanged JSON on
        // every save after the recovery interval; oversized or malformed
        // files stay unknown and are handled conservatively by the caller.
        let layout = if stamp.is_some() {
            match files::read_session_file(path) {
                Ok(content) => Some(content),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let layout = match layout {
            Some(content) => {
                let snapshot = super::schema::parse_session_file(&content).ok();
                Some(snapshot.map_or(SavedLayout::Unknown, |snapshot| saved_layout(&snapshot)))
            }
            None => stamp.map(|_| SavedLayout::Unknown),
        };
        *cached = Some(CachedSnapshotLayout {
            path: path.to_path_buf(),
            stamp,
            layout: layout.clone(),
        });
        Ok(layout)
    }

    /// The writer just published `snapshot` at `path`.
    pub(super) fn remember_current(&mut self, path: &Path, snapshot: &SessionSnapshot) {
        self.current = match files::regular_file_stamp(path) {
            Ok(Some(stamp)) => Some(CachedSnapshotLayout {
                path: path.to_path_buf(),
                stamp: Some(stamp),
                layout: Some(saved_layout(snapshot)),
            }),
            Ok(None) | Err(_) => None,
        };
    }

    pub(super) fn forget_current(&mut self) {
        self.current = None;
    }

    fn forget_latest(&mut self) {
        self.latest = None;
    }
}

/// What snapshot history needs around one write of the session file.
#[derive(Clone, Copy)]
pub(super) enum SnapshotHistoryPlan {
    /// Nothing to preserve: the newest copy is inside the snapshot interval,
    /// or both layouts already match it.
    Skip,
    /// The layout on disk differs from the newest copy: preserve it now.
    PreserveBeforeWrite,
    /// Only the replacement layout differs from the newest copy: preserve it
    /// once it is committed.
    PreserveAfterWrite,
    /// The decision could not be made before the write; make it again after.
    RetryAfterWrite,
}

// limits-exempt: a filename field width of the recovery-copy name format.
const RECOVERY_TIMESTAMP_DIGITS: usize = 39;
// limits-exempt: a filename field width of the recovery-copy name format.
const RECOVERY_SEQUENCE_DIGITS: usize = 3;

/// What names one recovery copy, and orders the copies of a directory: the
/// file is `session-<timestamp>-<sequence>.json`, both fields zero-padded to
/// a fixed width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RecoveryKey {
    timestamp: u128,
    sequence: usize,
}

impl RecoveryKey {
    fn parse(name: &str) -> Option<Self> {
        let fields = name.strip_prefix("session-")?.strip_suffix(".json")?;
        let (timestamp, sequence) = fields.split_once('-')?;
        if timestamp.len() != RECOVERY_TIMESTAMP_DIGITS
            || sequence.len() != RECOVERY_SEQUENCE_DIGITS
            || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
            || !sequence.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            timestamp: timestamp.parse().ok()?,
            sequence: sequence.parse().ok()?,
        })
    }

    fn file_name(self) -> String {
        let timestamp = self.timestamp;
        let sequence = self.sequence;
        format!(
            "session-{timestamp:0RECOVERY_TIMESTAMP_DIGITS$}-{sequence:0RECOVERY_SEQUENCE_DIGITS$}.json"
        )
    }
}

#[derive(Clone, Copy)]
enum RecoveryKind {
    Snapshot,
    Backup,
}

impl RecoveryKind {
    fn directory(self, path: &Path) -> PathBuf {
        match self {
            Self::Snapshot => files::snapshot_directory(path),
            Self::Backup => files::backup_directory(path),
        }
    }

    fn keep_limit(self) -> usize {
        match self {
            Self::Snapshot => SNAPSHOT_LIMIT,
            Self::Backup => BACKUP_LIMIT,
        }
    }

    fn event(self) -> &'static str {
        match self {
            Self::Snapshot => "persist.snapshot",
            Self::Backup => "persist.backup",
        }
    }
}

// Every sequence must fit the fixed width `RecoveryKey` parses.
const _: () = {
    let mut largest = RECOVERY_SEQUENCE_LIMIT - 1;
    let mut digits = 1;
    while largest >= 10 {
        largest /= 10;
        digits += 1;
    }
    assert!(digits <= RECOVERY_SEQUENCE_DIGITS);
};

/// Decides which layout needs preserving before the caller replaces the file.
/// The newest recovery copy controls both cadence and layout deduplication.
///
/// `replacement` is `Some` before the write; `None` after it, when the file on
/// disk is the replacement and only `PreserveBeforeWrite` ("the layout on disk
/// differs from the newest copy") is meaningful, whatever the write order. One
/// function with an optional replacement keeps both decisions on the same rules.
fn snapshot_history_decision(
    path: &Path,
    replacement: Option<&SessionSnapshot>,
    now: SystemTime,
    fingerprints: &mut SnapshotFingerprintCache,
) -> io::Result<SnapshotHistoryPlan> {
    let directory = RecoveryKind::Snapshot.directory(path);
    let existing = match recovery_files(&directory) {
        Ok(files) => files,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    let latest = existing.last().map(|(_, path)| path);
    if let Some(latest) = latest {
        let modified = std::fs::metadata(latest)?.modified()?;
        if now
            .duration_since(modified)
            .is_ok_and(|age| age < SNAPSHOT_INTERVAL)
        {
            return Ok(SnapshotHistoryPlan::Skip);
        }
    }
    let latest_layout = match latest {
        Some(latest) => fingerprints.latest(latest)?,
        None => None,
    };
    let previous = fingerprints.current(path)?;
    if previous
        .as_ref()
        .is_some_and(|layout| layout_differs_from_latest(layout, latest_layout.as_ref()))
    {
        return Ok(SnapshotHistoryPlan::PreserveBeforeWrite);
    }
    if replacement.is_some_and(|snapshot| {
        layout_differs_from_latest(&saved_layout(snapshot), latest_layout.as_ref())
    }) {
        return Ok(SnapshotHistoryPlan::PreserveAfterWrite);
    }
    Ok(SnapshotHistoryPlan::Skip)
}

fn saved_layout(snapshot: &SessionSnapshot) -> SavedLayout {
    if snapshot.workspaces.is_empty() {
        return SavedLayout::Empty;
    }
    SavedLayout::Known(layout_fingerprint(snapshot))
}

fn layout_differs_from_latest(layout: &SavedLayout, latest: Option<&SavedLayout>) -> bool {
    match layout {
        SavedLayout::Empty => false,
        SavedLayout::Known(fingerprint) => match latest {
            Some(SavedLayout::Known(latest)) => latest != fingerprint,
            _ => true,
        },
        // A file whose layout could not be read cannot be proven identical
        // to a recovery copy, so preserve it conservatively.
        SavedLayout::Unknown => true,
    }
}

/// The snapshot step before the session file at `path` is replaced by
/// `replacement`: decides whether a snapshot is due, and preserves the layout
/// on disk now when it is the one that differs. What remains to do after the
/// write is the returned plan.
pub(super) fn plan_snapshot_history(
    path: &Path,
    replacement: &SessionSnapshot,
    now: SystemTime,
    fingerprints: &mut SnapshotFingerprintCache,
) -> SnapshotHistoryPlan {
    // Preserve snapshot errors as their own tracing events; the platform's
    // session helpers emit through tracing too but label save outcomes.
    match snapshot_history_decision(path, Some(replacement), now, fingerprints) {
        Ok(SnapshotHistoryPlan::PreserveBeforeWrite) => {
            match preserve_existing_in(path, RecoveryKind::Snapshot, now) {
                Ok(_) => {
                    fingerprints.forget_latest();
                    SnapshotHistoryPlan::Skip
                }
                Err(err) => {
                    tracing::warn!(
                        event = "persist.snapshot", subsystem = "persist", outcome = "error",
                        path = %path.display(), error = %err,
                        "failed to preserve session snapshot"
                    );
                    SnapshotHistoryPlan::RetryAfterWrite
                }
            }
        }
        Ok(plan) => plan,
        Err(err) => {
            tracing::warn!(
                event = "persist.snapshot", subsystem = "persist", outcome = "error",
                path = %path.display(),
                error = %err, "failed to inspect session snapshot history"
            );
            SnapshotHistoryPlan::RetryAfterWrite
        }
    }
}

/// The snapshot step after the session file at `path` was replaced (or
/// cleared), as `plan` left it.
pub(super) fn finish_snapshot_history(
    path: &Path,
    plan: SnapshotHistoryPlan,
    now: SystemTime,
    fingerprints: &mut SnapshotFingerprintCache,
) {
    match plan {
        SnapshotHistoryPlan::Skip | SnapshotHistoryPlan::PreserveBeforeWrite => {}
        SnapshotHistoryPlan::PreserveAfterWrite | SnapshotHistoryPlan::RetryAfterWrite => {
            preserve_snapshot_history(path, now, fingerprints);
        }
    }
}

/// Preserves the layout on disk as a snapshot when one is due, logging a
/// failure as its own event.
pub(super) fn preserve_snapshot_history(
    path: &Path,
    now: SystemTime,
    fingerprints: &mut SnapshotFingerprintCache,
) {
    if let Err(err) = preserve_snapshot_after_write(path, now, fingerprints) {
        // The platform's session helpers also emit tracing events, but
        // cover save, clear and restore outcomes only. Keep this distinct
        // so a snapshot failure is not mislabeled as a failed session save.
        // The event literal is also the log schema category used to query this path.
        tracing::warn!(
            event = "persist.snapshot", subsystem = "persist", outcome = "error",
            path = %path.display(),
            error = %err, "failed to preserve session snapshot"
        );
    }
}

fn preserve_snapshot_after_write(
    path: &Path,
    now: SystemTime,
    fingerprints: &mut SnapshotFingerprintCache,
) -> io::Result<()> {
    if matches!(
        snapshot_history_decision(path, None, now, fingerprints)?,
        SnapshotHistoryPlan::PreserveBeforeWrite
    ) && preserve_existing_in(path, RecoveryKind::Snapshot, now)?
    {
        fingerprints.forget_latest();
    }
    Ok(())
}

/// Why the required first-save backup could not be made.
#[derive(Debug)]
pub(super) enum BackupError {
    /// The current session file itself could not be opened. Retrying this
    /// server's save loop cannot make the original readable.
    SourceUnreadable(io::Error),
    /// The source opened, but the recovery copy could not be published. This
    /// can be a transient filesystem failure and keeps the ordinary retry path.
    Io(io::Error),
}

/// Copies the session file at `path` into the backup directory. `false` when
/// there is no file to copy. A source-open failure is kept distinct because
/// the writer must not replace that source without preserving it first.
pub(super) fn preserve_existing(path: &Path, now: SystemTime) -> Result<bool, BackupError> {
    let mut source = match files::open_regular(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(BackupError::SourceUnreadable(err)),
    };
    preserve_opened_source(path, RecoveryKind::Backup, now, &mut source).map_err(BackupError::Io)
}

/// The bool is whether a session file existed to copy. The snapshot caller
/// needs it to know whether to forget its newest-copy fingerprint, and the
/// backup caller passes it on, so a named type would only rename it.
fn preserve_existing_in(path: &Path, kind: RecoveryKind, now: SystemTime) -> io::Result<bool> {
    // The source is opened once, through its type check, and the copy is read
    // from this very descriptor: reopening the path could meet another object
    // (a FIFO swapped in would block the copy). A session path that is not a
    // regular file fails the preservation, and with it the save, with a
    // message naming it.
    let mut source = match files::open_regular(path) {
        Ok(file) => file,
        // Recheck on the next mutation until a fresh session is actually saved.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    preserve_opened_source(path, kind, now, &mut source)
}

fn preserve_opened_source(
    path: &Path,
    kind: RecoveryKind,
    now: SystemTime,
    source: &mut std::fs::File,
) -> io::Result<bool> {
    let directory = kind.directory(path);
    let keep = kind.keep_limit();
    shepr_platform::create_private_directory_all(&directory)?;
    let older = recovery_files(&directory)?;
    let timestamp_now = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Keep creation order even when the wall clock moves backwards.
    let timestamp = match older.last() {
        Some((previous, _)) => timestamp_now.max(
            previous
                .timestamp
                .checked_add(1)
                .ok_or_else(|| io::Error::other("session recovery sequence exhausted"))?,
        ),
        None => timestamp_now,
    };
    for sequence in 0..RECOVERY_SEQUENCE_LIMIT {
        let backup = directory.join(
            RecoveryKey {
                timestamp,
                sequence,
            }
            .file_name(),
        );
        if backup.try_exists()? {
            continue;
        }

        // The `AlreadyExists` arm below is a backstop: under the data
        // directory lease there is one writer, and the existence check above
        // already skips a taken name, so only a file appearing in between
        // (which the lease rules out) reaches it. A refused copy has read the
        // source, so every attempt starts from its beginning.
        source.rewind()?;
        match copy_recovery(source, &backup) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
        // Recovery-copy events use their own labels; the platform's session
        // helpers emit through tracing too but only cover session mutations.
        log_recovery_preserved(kind, path, &backup);
        if let Err(err) = prune_recovery_copies(&older, keep) {
            // The new copy is durable, so preserve it and let the caller
            // complete its save or clear. The unloaded-session guard consumes
            // this copy once, and snapshot saves use their normal cadence;
            // either path retries pruning on a later preservation attempt.
            log_recovery_prune_failure(kind, path, &directory, &err);
        }
        return Ok(true);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate session recovery copy",
    ))
}

fn log_recovery_preserved(kind: RecoveryKind, path: &Path, backup: &Path) {
    match kind {
        RecoveryKind::Snapshot => tracing::info!(
            event = kind.event(), subsystem = "persist", outcome = "ok",
            path = %path.display(), backup_path = %backup.display(),
            "preserved session snapshot"
        ),
        RecoveryKind::Backup => tracing::info!(
            event = kind.event(), subsystem = "persist", outcome = "ok",
            path = %path.display(), backup_path = %backup.display(),
            "preserved session recovery copy"
        ),
    }
}

fn log_recovery_prune_failure(kind: RecoveryKind, path: &Path, directory: &Path, err: &io::Error) {
    match kind {
        RecoveryKind::Snapshot => tracing::warn!(
            event = kind.event(), subsystem = "persist", outcome = "prune_error",
            path = %path.display(), recovery_directory = %directory.display(), error = %err,
            "preserved session snapshot but could not prune old copies"
        ),
        RecoveryKind::Backup => tracing::warn!(
            event = kind.event(), subsystem = "persist", outcome = "prune_error",
            path = %path.display(), recovery_directory = %directory.display(), error = %err,
            "preserved session recovery copy but could not prune old copies"
        ),
    }
}

fn copy_recovery(source: &mut impl io::Read, backup: &Path) -> io::Result<()> {
    // A create-only publish withdraws the copy when the directory sync fails
    // and comes back as an error, so `NotDurable` cannot happen here; treat it
    // as a failure anyway rather than count an unsynced copy as a recovery copy.
    if let files::Published::NotDurable(err) =
        files::publish_private_file(source, backup, files::PublishTarget::CreateOnly)?
    {
        files::remove_after_failed_publish(backup);
        return Err(err);
    }
    // The recovery directory may have just been created; its own entry must be
    // durable too, or the copy is not a recovery copy at all.
    let directory = files::containing_directory(backup);
    if let Err(err) = shepr_platform::sync_directory(files::containing_directory(directory)) {
        files::remove_after_failed_publish(backup);
        return Err(err);
    }
    Ok(())
}

fn recovery_files(directory: &Path) -> io::Result<Vec<(RecoveryKey, PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && let Some(key) = entry.file_name().to_str().and_then(RecoveryKey::parse)
        {
            files.push((key, entry.path()));
        }
    }
    files.sort();
    Ok(files)
}

fn prune_recovery_copies(older: &[(RecoveryKey, PathBuf)], keep: usize) -> io::Result<()> {
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

/// The recovery copies of the layout in `directory`, oldest first, for tests
/// that read what a save preserved.
#[cfg(test)]
pub(super) fn recovery_layouts_for_test(directory: &Path) -> io::Result<Vec<(u128, PathBuf)>> {
    Ok(recovery_files(directory)?
        .into_iter()
        .map(|(key, path)| (key.timestamp, path))
        .collect())
}

#[cfg(test)]
fn recovery_filename(timestamp: u128, sequence: usize) -> String {
    RecoveryKey {
        timestamp,
        sequence,
    }
    .file_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_layout_is_not_treated_as_identical() {
        let known = SavedLayout::Known(LayoutFingerprint(b"known".to_vec()));
        let same = SavedLayout::Known(LayoutFingerprint(b"same".to_vec()));
        let changed = SavedLayout::Known(LayoutFingerprint(b"changed".to_vec()));
        assert!(layout_differs_from_latest(
            &SavedLayout::Unknown,
            Some(&known)
        ));
        assert!(layout_differs_from_latest(&SavedLayout::Unknown, None));
        assert!(!layout_differs_from_latest(&same, Some(&same)));
        assert!(layout_differs_from_latest(&changed, Some(&same)));
    }

    #[test]
    fn pruning_continues_past_an_undeletable_entry() {
        let scratch = shepr_test_support::ScratchDir::new("prune-undeletable");
        let directory = scratch.path();
        let first = RecoveryKey {
            timestamp: 1,
            sequence: 0,
        };
        let second = RecoveryKey {
            timestamp: 2,
            sequence: 0,
        };
        let locked = directory.join(first.file_name());
        std::fs::create_dir(&locked).expect("test precondition");
        let removable = directory.join(second.file_name());
        std::fs::write(&removable, b"old").expect("test precondition");
        let older = [(first, locked.clone()), (second, removable.clone())];
        assert!(prune_recovery_copies(&older, 2).is_ok());
        assert!(locked.try_exists().expect("test stat"));
        assert!(!removable.try_exists().expect("test stat"));
        assert!(prune_recovery_copies(&[(first, locked)], 1).is_err());
    }

    #[test]
    fn recovery_filename_round_trips() {
        let key = RecoveryKey {
            timestamp: 1_729_123_456_789_012_345_678_901_234_567_890u128,
            sequence: 7,
        };
        let filename = key.file_name();
        let timestamp = key.timestamp;
        assert_eq!(
            filename,
            format!("session-{timestamp:0RECOVERY_TIMESTAMP_DIGITS$}-007.json")
        );
        assert_eq!(RecoveryKey::parse(&filename), Some(key));
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
        let scratch = shepr_test_support::ScratchDir::new("interrupted-recovery-copy");
        let directory = scratch.path();
        let backup = directory.join(recovery_filename(1, 0));
        let mut source = io::Cursor::new(b"partial").chain(Interrupted);
        #[expect(
            clippy::disallowed_methods,
            reason = "a panic mid-copy stands in for a crash after the prefix is written; \
                      catching it lets the test inspect what that crash leaves on disk"
        )]
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            copy_recovery(&mut source, &backup)
        }));
        assert!(interrupted.is_err());
        assert!(
            !backup.try_exists().expect("test stat"),
            "an interrupted copy must not look complete"
        );
        // Unwinding drops the prepared file, which removes its staging name; a
        // real crash would leave the prefix under that name, still not a
        // recovery file.
        assert_eq!(
            std::fs::read_dir(directory)
                .expect("test precondition")
                .count(),
            0,
            "an unwound copy removes its staging file"
        );
        assert!(
            recovery_files(directory)
                .expect("test precondition")
                .is_empty()
        );
    }

    #[test]
    fn a_completed_copy_leaves_only_the_recovery_file() {
        let scratch = shepr_test_support::ScratchDir::new("completed-recovery-copy");
        let directory = scratch.path();
        let backup = directory.join(recovery_filename(123, 0));

        copy_recovery(&mut io::Cursor::new(b"complete copy"), &backup).expect("copy recovery");

        assert_eq!(
            std::fs::read(&backup).expect("published recovery copy"),
            b"complete copy"
        );
        assert_eq!(
            std::fs::read_dir(directory)
                .expect("test precondition")
                .count(),
            1
        );
    }
}
