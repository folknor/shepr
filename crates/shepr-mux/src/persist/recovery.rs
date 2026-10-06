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
    /// No recovery copy is needed before the next replacement.
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

#[derive(Clone, PartialEq, Eq)]
enum SavedLayout {
    Empty,
    Known(LayoutFingerprint),
    Unknown,
}

/// What the leased writer knows about the session file and the newest
/// snapshot: their layouts, and when that snapshot was taken. Under the lease
/// the writer is the only process writing either, so they are read from disk
/// once, at the first snapshot decision after startup (the newest snapshot's
/// time is then its file's mtime), and from then on updated by the writer's own
/// publications and copies. The cadence decision is `snapshot_plan`, a pure
/// function of these facts and `now`.
#[derive(Default)]
pub(super) struct SnapshotState {
    initialized: bool,
    /// The layout of the session file on disk; `None` when there is none.
    current: Option<SavedLayout>,
    /// When the newest snapshot was taken, and its layout.
    latest: Option<(SystemTime, SavedLayout)>,
    /// The last snapshot failure logged, so a failure that repeats on every
    /// save is logged once until a copy succeeds or nothing is due.
    failure: Option<String>,
}

impl SnapshotState {
    fn initialize(&mut self, path: &Path) -> io::Result<()> {
        if self.initialized {
            return Ok(());
        }
        let directory = files::snapshot_directory(path);
        let existing = match recovery_files(&directory) {
            Ok(files) => files,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        self.current = read_saved_layout(path)?;
        self.latest = match existing.last() {
            Some((_, latest)) => Some((
                std::fs::metadata(latest)?.modified()?,
                read_saved_layout(latest)?.unwrap_or(SavedLayout::Unknown),
            )),
            None => None,
        };
        self.initialized = true;
        Ok(())
    }

    /// The writer just published `snapshot` as the session file.
    pub(super) fn remember_current(&mut self, snapshot: &SessionSnapshot) {
        self.current = Some(saved_layout(snapshot));
    }

    pub(super) fn forget_current(&mut self) {
        self.current = None;
    }

    /// The session file on disk was just copied as the newest snapshot.
    fn copied(&mut self, now: SystemTime) {
        if let Some(current) = &self.current {
            self.latest = Some((now, current.clone()));
        }
        self.failure = None;
    }

    fn report_failure(&mut self, path: &Path, error: &io::Error) {
        let message = error.to_string();
        if self.failure.as_ref() != Some(&message) {
            shepr_platform::structured_log!(
                WARN, event = persist.snapshot, outcome = "error",
                path = %path.display(), error = %error,
                "failed to preserve session snapshot"
            );
            self.failure = Some(message);
        }
    }
}

fn read_saved_layout(path: &Path) -> io::Result<Option<SavedLayout>> {
    match files::read_session_file(path) {
        Ok(content) => Ok(Some(
            super::schema::parse_session_file(&content)
                .map_or(SavedLayout::Unknown, |snapshot| saved_layout(&snapshot)),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(Some(SavedLayout::Unknown)),
        Err(error) => Err(error),
    }
}

/// What the recovery snapshots need around one write of the session file.
#[derive(Clone, Copy)]
pub(super) enum SnapshotPlan {
    /// Nothing to preserve: the newest copy is inside the snapshot interval,
    /// or both layouts already match it.
    Skip,
    /// The layout on disk differs from the newest copy: preserve it now.
    PreserveBeforeWrite,
    /// Only the replacement layout differs from the newest copy: preserve it
    /// once it is committed.
    PreserveAfterWrite,
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
fn snapshot_decision(
    path: &Path,
    replacement: Option<&SessionSnapshot>,
    now: SystemTime,
    fingerprints: &mut SnapshotState,
) -> io::Result<SnapshotPlan> {
    fingerprints.initialize(path)?;
    Ok(snapshot_plan(
        fingerprints.current.as_ref(),
        fingerprints.latest.as_ref(),
        replacement.map(saved_layout).as_ref(),
        now,
    ))
}

fn snapshot_plan(
    previous: Option<&SavedLayout>,
    latest: Option<&(SystemTime, SavedLayout)>,
    replacement: Option<&SavedLayout>,
    now: SystemTime,
) -> SnapshotPlan {
    if latest.is_some_and(|(when, _)| {
        now.duration_since(*when)
            .is_ok_and(|age| age < SNAPSHOT_INTERVAL)
    }) {
        return SnapshotPlan::Skip;
    }
    let latest_layout = latest.map(|(_, layout)| layout);
    if previous.is_some_and(|layout| layout_differs_from_latest(layout, latest_layout)) {
        return SnapshotPlan::PreserveBeforeWrite;
    }
    if replacement.is_some_and(|snapshot| layout_differs_from_latest(snapshot, latest_layout)) {
        return SnapshotPlan::PreserveAfterWrite;
    }
    SnapshotPlan::Skip
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
pub(super) fn plan_snapshot(
    path: &Path,
    replacement: &SessionSnapshot,
    now: SystemTime,
    fingerprints: &mut SnapshotState,
) -> SnapshotPlan {
    // Keep snapshot planning failures in this subsystem's tracing events.
    match snapshot_decision(path, Some(replacement), now, fingerprints) {
        Ok(SnapshotPlan::PreserveBeforeWrite) => {
            match preserve_existing_in(path, RecoveryKind::Snapshot, now) {
                Ok(true) => fingerprints.copied(now),
                // The file the writer last published is gone, so there is no
                // layout on disk to preserve or to compare later.
                Ok(false) => fingerprints.forget_current(),
                Err(err) => fingerprints.report_failure(path, &err),
            }
            SnapshotPlan::Skip
        }
        Ok(plan) => {
            // A pending copy after the write may still fail the same way, so
            // only a decision with nothing due ends a reported failure.
            if matches!(plan, SnapshotPlan::Skip) {
                fingerprints.failure = None;
            }
            plan
        }
        Err(err) => {
            fingerprints.report_failure(path, &err);
            SnapshotPlan::Skip
        }
    }
}

/// The snapshot step after the session file at `path` was replaced (or
/// cleared), as `plan` left it.
pub(super) fn finish_snapshot(
    path: &Path,
    plan: SnapshotPlan,
    now: SystemTime,
    fingerprints: &mut SnapshotState,
) {
    match plan {
        SnapshotPlan::Skip | SnapshotPlan::PreserveBeforeWrite => {}
        SnapshotPlan::PreserveAfterWrite => {
            preserve_snapshot(path, now, fingerprints);
        }
    }
}

/// Preserves the layout on disk as a snapshot when one is due, logging a
/// failure as its own event.
pub(super) fn preserve_snapshot(path: &Path, now: SystemTime, fingerprints: &mut SnapshotState) {
    if let Err(err) = preserve_snapshot_after_write(path, now, fingerprints) {
        fingerprints.report_failure(path, &err);
    }
}

fn preserve_snapshot_after_write(
    path: &Path,
    now: SystemTime,
    fingerprints: &mut SnapshotState,
) -> io::Result<()> {
    match snapshot_decision(path, None, now, fingerprints)? {
        SnapshotPlan::PreserveBeforeWrite => {
            if preserve_existing_in(path, RecoveryKind::Snapshot, now)? {
                fingerprints.copied(now);
            } else {
                fingerprints.forget_current();
            }
        }
        SnapshotPlan::Skip | SnapshotPlan::PreserveAfterWrite => fingerprints.failure = None,
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
/// needs it to know whether the copy is now the newest snapshot, and the
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

        // Non-regular entries are excluded from `older`, so one may occupy
        // the timestamp chosen above. The bounded sequence skips those names
        // without replacing them. The `AlreadyExists` arm is a backstop under the data
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
        // Persist owns the recovery event labels; publication returns I/O
        // errors without assigning them a session-level outcome.
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
        RecoveryKind::Snapshot => shepr_platform::structured_log!(
            INFO, event = persist.snapshot, outcome = "ok",
            path = %path.display(), backup_path = %backup.display(),
            "preserved session snapshot"
        ),
        RecoveryKind::Backup => shepr_platform::structured_log!(
            INFO, event = persist.backup, outcome = "ok",
            path = %path.display(), backup_path = %backup.display(),
            "preserved session recovery copy"
        ),
    }
}

fn log_recovery_prune_failure(kind: RecoveryKind, path: &Path, directory: &Path, err: &io::Error) {
    match kind {
        RecoveryKind::Snapshot => shepr_platform::structured_log!(
            WARN, event = persist.snapshot, outcome = "prune_error",
            path = %path.display(), recovery_directory = %directory.display(), error = %err,
            "preserved session snapshot but could not prune old copies"
        ),
        RecoveryKind::Backup => shepr_platform::structured_log!(
            WARN, event = persist.backup, outcome = "prune_error",
            path = %path.display(), recovery_directory = %directory.display(), error = %err,
            "preserved session recovery copy but could not prune old copies"
        ),
    }
}

fn copy_recovery(source: &mut impl io::Read, backup: &Path) -> io::Result<()> {
    // The publish API shares the replacement outcome type with create-only
    // writes. Keep this check until that API exposes create-only success as
    // `()`: an unsynced copy must never count as a recovery point.
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
    fn cadence_uses_owned_time_and_recovers_from_rollback() {
        let original = SavedLayout::Known(LayoutFingerprint(vec![1]));
        let changed = SavedLayout::Known(LayoutFingerprint(vec![2]));
        let now = UNIX_EPOCH + SNAPSHOT_INTERVAL;
        let latest = (now, original.clone());
        assert!(matches!(
            snapshot_plan(
                Some(&changed),
                Some(&latest),
                None,
                now + SNAPSHOT_INTERVAL - std::time::Duration::from_nanos(1)
            ),
            SnapshotPlan::Skip
        ));
        assert!(matches!(
            snapshot_plan(Some(&changed), Some(&latest), None, now + SNAPSHOT_INTERVAL),
            SnapshotPlan::PreserveBeforeWrite
        ));
        let rollback = now - std::time::Duration::from_secs(1);
        assert!(matches!(
            snapshot_plan(Some(&changed), Some(&latest), None, rollback),
            SnapshotPlan::PreserveBeforeWrite
        ));
        let copied = (rollback, changed.clone());
        assert!(matches!(
            snapshot_plan(Some(&original), Some(&copied), None, rollback),
            SnapshotPlan::Skip
        ));
    }

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
