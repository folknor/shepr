use crate::limits::{BACKUP_LIMIT, RECOVERY_SEQUENCE_LIMIT, SNAPSHOT_INTERVAL, SNAPSHOT_LIMIT};
use std::io::{self, Seek};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::SessionSnapshot;
use super::error::{SaveError, SaveRefusal};
use super::snapshot::SessionHistory;

fn history_file_stamp(path: &Path) -> io::Result<Option<shepr_platform::FileStamp>> {
    let resolved = super::io::SessionPath::resolve(path)?;
    let Some(metadata) = resolved.regular_metadata(path)? else {
        return Ok(None);
    };
    Ok(Some(shepr_platform::FileStamp::from_metadata(metadata)))
}

struct WrittenHistory {
    digest: super::io::HistoryDigest,
    file: shepr_platform::FileStamp,
}

struct CachedSnapshotLayout {
    path: PathBuf,
    stamp: Option<shepr_platform::FileStamp>,
    layout: Option<SavedLayout>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SavedLayout {
    Empty,
    Known(super::snapshot::LayoutFingerprint),
    Unknown,
}

#[derive(Default)]
struct SnapshotFingerprintCache {
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
        let stamp = history_file_stamp(path)?;
        if let Some(cached) = cached
            && cached.path.as_path() == path
            && cached.stamp == stamp
        {
            return Ok(cached.layout);
        }
        // The writer owns the session lease, so its own publications update
        // these entries directly. Metadata avoids rereading unchanged JSON on
        // every save after the recovery interval; oversized or malformed
        // files stay unknown and are handled conservatively by the caller.
        let layout = if stamp.is_some() {
            match super::io::read_session_file(path) {
                Ok(content) => Some(content),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let layout = match layout {
            Some(content) => {
                let snapshot = super::snapshot::parse_session_file(&content)
                    .ok()
                    .map(|file| file.snapshot);
                Some(snapshot.map_or(SavedLayout::Unknown, |snapshot| saved_layout(&snapshot)))
            }
            None => stamp.map(|_| SavedLayout::Unknown),
        };
        *cached = Some(CachedSnapshotLayout {
            path: path.to_path_buf(),
            stamp,
            layout,
        });
        Ok(layout)
    }

    fn remember_current(&mut self, path: &Path, snapshot: &SessionSnapshot) {
        self.current = match history_file_stamp(path) {
            Ok(Some(stamp)) => Some(CachedSnapshotLayout {
                path: path.to_path_buf(),
                stamp: Some(stamp),
                layout: Some(saved_layout(snapshot)),
            }),
            Ok(None) | Err(_) => None,
        };
    }

    fn forget_current(&mut self) {
        self.current = None;
    }

    fn forget_latest(&mut self) {
        self.latest = None;
    }
}

/// What snapshot history needs around one write of the session file.
#[derive(Clone, Copy)]
enum SnapshotHistoryPlan {
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

/// What a save does to the history file, decided before the layout is
/// written, since the layout names the history it pairs with.
enum HistoryIntent {
    /// Delete it: history is not persisted. The layout names none.
    Remove,
    /// Replace it with these serialized bytes, unless it already holds
    /// exactly them. The layout names `digest`, the hash of `json`.
    Write {
        json: Vec<u8>,
        digest: super::io::HistoryDigest,
    },
    /// The caller knows it already holds the history `digest` names.
    Keep(super::io::HistoryDigest),
    /// The history could not be serialized: the layout names none, and the
    /// error is the save's.
    Failed(io::Error),
}

impl HistoryIntent {
    /// The digest the layout written with this intent names.
    fn digest(&self) -> Option<super::io::HistoryDigest> {
        match self {
            Self::Write { digest, .. } | Self::Keep(digest) => Some(*digest),
            Self::Remove | Self::Failed(_) => None,
        }
    }
}

/// Shared by autosave, pane-exit checkpoints, and shutdown.
pub struct SessionWriter {
    path: PathBuf,
    protect_unloaded: bool,
    lease: Option<super::lock::DataDirLease>,
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
    pub const SESSION_FILE_NAME: &'static str = super::io::SESSION_FILE_NAME;

    pub fn new(lease: super::lock::DataDirLease, protect_unloaded: bool) -> Self {
        let path = super::io::session_path(lease.directory());
        Self {
            path,
            protect_unloaded,
            lease: Some(lease),
            written_history: None,
            snapshot_fingerprints: SnapshotFingerprintCache::default(),
            trimming_history: false,
        }
    }

    fn may_write(&self) -> Result<bool, SaveError> {
        // A missing lease means this writer was retired and should quietly
        // ignore later direct calls. A present but released lease must refuse
        // writes because another server may own the directory now.
        match self.lease.as_ref() {
            None => Ok(false),
            Some(lease) if lease.is_active() => Ok(true),
            Some(_) => Err(SaveError::Refused(SaveRefusal::InactiveLease)),
        }
    }

    /// Release ownership after the final shutdown save. Later saves and
    /// clears are ignored.
    pub fn retire(&mut self) {
        if let Some(mut lease) = self.lease.take() {
            lease.release();
        }
    }

    fn preserve_unloaded(&mut self, now: SystemTime) -> io::Result<()> {
        if self.protect_unloaded && preserve_existing(&self.path, now)? {
            self.protect_unloaded = false;
        }
        Ok(())
    }

    fn preserve_snapshot_history(&mut self, now: SystemTime) {
        if let Err(err) =
            preserve_snapshot_after_write(&self.path, now, &mut self.snapshot_fingerprints)
        {
            // The platform's session helpers also emit tracing events, but
            // cover save, clear and restore outcomes only. Keep this distinct
            // so a snapshot failure is not mislabeled as a failed session save.
            tracing::warn!(
                event = "persist.snapshot", subsystem = "persist", outcome = "error",
                path = %self.path.display(),
                error = %err, "failed to preserve session snapshot"
            );
        }
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
    ) -> Result<Option<super::io::HistoryDigest>, SaveError> {
        if !self.may_write()? {
            return Ok(None);
        }
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
        digest: &super::io::HistoryDigest,
        now: SystemTime,
    ) -> Result<Option<super::io::HistoryDigest>, SaveError> {
        if !self.may_write()? {
            return Ok(None);
        }
        self.save_with(snapshot, HistoryIntent::Keep(*digest), now)
    }

    /// Serializes `history` (trimmed to the file cap) and hashes exactly the
    /// bytes that would be written.
    fn prepare_history(&mut self, history: &SessionHistory) -> HistoryIntent {
        let history_path =
            super::io::session_history_path(super::io::containing_directory(&self.path));
        match super::io::serialize_history(history) {
            Ok(super::io::SerializedHistory { json, trimmed }) => {
                self.note_history_trim(&history_path, trimmed);
                let digest = super::io::history_digest(&json);
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
        let history_path =
            super::io::session_history_path(super::io::containing_directory(&self.path));
        history_file_stamp(&history_path)
            .ok()
            .flatten()
            .is_some_and(|file| file == written.file)
    }

    fn save_with(
        &mut self,
        snapshot: &SessionSnapshot,
        history: HistoryIntent,
        now: SystemTime,
    ) -> Result<Option<super::io::HistoryDigest>, SaveError> {
        let mut snapshot_history_plan = SnapshotHistoryPlan::RetryAfterWrite;
        let result = self.preserve_unloaded(now).and_then(|()| {
            snapshot_history_plan = self.prepare_snapshot_history(snapshot, now);
            let digest = history.digest();
            super::io::save_to_path(&self.path, snapshot, digest.as_ref())
        });
        self.finish_save_with_snapshot_plan(result, snapshot, history, snapshot_history_plan, now)
    }

    fn finish_save_with_snapshot_plan(
        &mut self,
        result: io::Result<super::io::Published>,
        snapshot: &SessionSnapshot,
        history: HistoryIntent,
        snapshot_history_plan: SnapshotHistoryPlan,
        now: SystemTime,
    ) -> Result<Option<super::io::HistoryDigest>, SaveError> {
        let digest = history.digest();
        let mut failure = None;
        if result.is_ok() {
            self.snapshot_fingerprints
                .remember_current(&self.path, snapshot);
        }
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
                failure = Some(SaveError::PublishedNotDurable(err));
            }
            Err(err) => {
                crate::logging::session_save_failed(&self.path, &err.to_string());
                return Err(SaveError::Io(err));
            }
        }
        // Optional history failure must not reclassify our committed layout as unloaded.
        self.protect_unloaded = false;
        let history_path =
            super::io::session_history_path(super::io::containing_directory(&self.path));
        if let Err(err) = self.save_history(&history_path, history) {
            self.written_history = None;
            crate::logging::session_save_failed(&history_path, &err.to_string());
            if failure.is_none() {
                failure = Some(SaveError::Io(err));
            }
        } else {
            // After-write snapshots must include the history just committed
            // for their layout. Before-write snapshots were already copied
            // with the old history in `prepare_snapshot_history`.
            self.finish_snapshot_history(snapshot_history_plan, now);
        }
        if failure.is_none() {
            crate::logging::session_saved(&self.path, snapshot.workspaces.len());
        }
        failure.map_or(Ok(digest), Err)
    }

    fn prepare_snapshot_history(
        &mut self,
        replacement: &SessionSnapshot,
        now: SystemTime,
    ) -> SnapshotHistoryPlan {
        // Preserve snapshot errors as their own tracing events; the platform's
        // session helpers emit through tracing too but label save outcomes.
        match snapshot_history_decision(
            &self.path,
            Some(replacement),
            now,
            &mut self.snapshot_fingerprints,
        ) {
            Ok(SnapshotHistoryPlan::PreserveBeforeWrite) => {
                match preserve_existing_in(&self.path, RecoveryKind::Snapshot, now) {
                    Ok(_) => {
                        self.snapshot_fingerprints.forget_latest();
                        SnapshotHistoryPlan::Skip
                    }
                    Err(err) => {
                        tracing::warn!(
                            event = "persist.snapshot", subsystem = "persist", outcome = "error",
                            path = %self.path.display(), error = %err,
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
                    path = %self.path.display(),
                    error = %err, "failed to inspect session snapshot history"
                );
                SnapshotHistoryPlan::RetryAfterWrite
            }
        }
    }

    fn finish_snapshot_history(&mut self, plan: SnapshotHistoryPlan, now: SystemTime) {
        match plan {
            SnapshotHistoryPlan::Skip | SnapshotHistoryPlan::PreserveBeforeWrite => {}
            SnapshotHistoryPlan::PreserveAfterWrite | SnapshotHistoryPlan::RetryAfterWrite => {
                self.preserve_snapshot_history(now);
            }
        }
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
                return super::io::save_history_to_path(history_path, None);
            }
            HistoryIntent::Failed(error) => return Err(error),
            HistoryIntent::Write { json, digest } => (json, digest),
        };
        if let Some(written) = self
            .written_history
            .as_ref()
            .filter(|written| written.digest == digest)
            && history_file_stamp(history_path)? == Some(written.file)
        {
            return Ok(());
        }
        self.written_history = None;
        super::io::save_history_json_to_path(history_path, &json)?;
        // Metadata only guards the optimization. If it cannot be captured
        // after a successful write, future saves simply publish the history
        // again rather than trusting a cache with no matching file stamp.
        self.written_history = history_file_stamp(history_path)
            .ok()
            .flatten()
            .map(|file| WrittenHistory { digest, file });
        Ok(())
    }

    fn note_history_trim(&mut self, history_path: &Path, trimmed: Option<super::io::HistoryTrim>) {
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
        if !self.may_write()? {
            return Ok(());
        }
        self.written_history = None;
        let result = self.preserve_unloaded(now).and_then(|()| {
            self.preserve_snapshot_history(now);
            super::io::clear_path(&self.path)
        });
        if let Err(err) = result {
            crate::logging::session_clear_failed(&self.path, &err.to_string());
            return Err(SaveError::Io(err));
        }
        let history_path =
            super::io::session_history_path(super::io::containing_directory(&self.path));
        if let Err(err) = super::io::clear_path(&history_path) {
            crate::logging::session_clear_failed(&history_path, &err.to_string());
            return Err(SaveError::Io(err));
        }
        self.snapshot_fingerprints.forget_current();
        crate::logging::session_cleared(&self.path);
        Ok(())
    }
}

// limits-exempt: a filename field width of the recovery-copy name format.
const RECOVERY_TIMESTAMP_DIGITS: usize = 39;
// limits-exempt: a filename field width of the recovery-copy name format.
const RECOVERY_SEQUENCE_DIGITS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RecoveryKey {
    timestamp: u128,
    sequence: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryFileKind {
    Layout,
    History,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecoveryName {
    key: RecoveryKey,
    kind: RecoveryFileKind,
}

impl RecoveryName {
    fn new(timestamp: u128, sequence: usize, kind: RecoveryFileKind) -> Self {
        Self {
            key: RecoveryKey {
                timestamp,
                sequence,
            },
            kind,
        }
    }

    fn parse(name: &str) -> Option<Self> {
        let (kind, fields) = if let Some(fields) = name.strip_prefix("session-history-") {
            (RecoveryFileKind::History, fields)
        } else {
            (RecoveryFileKind::Layout, name.strip_prefix("session-")?)
        };
        let fields = fields.strip_suffix(".json")?;
        let (timestamp, sequence) = fields.split_once('-')?;
        if timestamp.len() != RECOVERY_TIMESTAMP_DIGITS
            || sequence.len() != RECOVERY_SEQUENCE_DIGITS
            || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
            || !sequence.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Some(Self::new(
            timestamp.parse().ok()?,
            sequence.parse().ok()?,
            kind,
        ))
    }

    fn pair(self) -> Self {
        let kind = match self.kind {
            RecoveryFileKind::Layout => RecoveryFileKind::History,
            RecoveryFileKind::History => RecoveryFileKind::Layout,
        };
        Self { kind, ..self }
    }

    fn file_name(self) -> String {
        let prefix = match self.kind {
            RecoveryFileKind::Layout => "session-",
            RecoveryFileKind::History => "session-history-",
        };
        let timestamp = self.key.timestamp;
        let sequence = self.key.sequence;
        format!(
            "{prefix}{timestamp:0RECOVERY_TIMESTAMP_DIGITS$}-{sequence:0RECOVERY_SEQUENCE_DIGITS$}.json"
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
            Self::Snapshot => super::io::snapshot_directory(path),
            Self::Backup => super::io::backup_directory(path),
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

// Every sequence must fit the fixed width `RecoveryName` parses.
const _: () = {
    let mut largest = RECOVERY_SEQUENCE_LIMIT - 1;
    let mut digits = 1;
    while largest >= 10 {
        largest /= 10;
        digits += 1;
    }
    assert!(digits <= RECOVERY_SEQUENCE_DIGITS);
};

fn recovery_history_path(layout_path: &Path) -> io::Result<PathBuf> {
    let name = layout_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(RecoveryName::parse)
        .filter(|name| name.kind == RecoveryFileKind::Layout)
        .ok_or_else(|| io::Error::other("invalid recovery layout name"))?;
    Ok(layout_path.with_file_name(name.pair().file_name()))
}

/// Decides which layout needs preserving before the caller replaces the file.
/// The newest recovery copy controls both cadence and layout deduplication.
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
    match super::snapshot::layout_fingerprint(snapshot) {
        Some(fingerprint) => SavedLayout::Known(fingerprint),
        None => SavedLayout::Unknown,
    }
}

fn layout_differs_from_latest(layout: &SavedLayout, latest: Option<&SavedLayout>) -> bool {
    match layout {
        SavedLayout::Empty => false,
        SavedLayout::Known(fingerprint) => match latest {
            Some(SavedLayout::Known(latest)) => latest != fingerprint,
            _ => true,
        },
        // A nonempty layout with no fingerprint cannot be proven identical to
        // a recovery copy, so preserve it conservatively.
        SavedLayout::Unknown => true,
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

fn preserve_existing(path: &Path, now: SystemTime) -> io::Result<bool> {
    preserve_existing_in(path, RecoveryKind::Backup, now)
}

fn preserve_existing_in(path: &Path, kind: RecoveryKind, now: SystemTime) -> io::Result<bool> {
    // Both sources are opened once, through their type check, and the copies
    // are read from these very descriptors: reopening the paths could meet
    // other objects (a FIFO swapped in would block the copy). A session path
    // that is not a regular file fails the preservation, and with it the save,
    // with a message naming it; one that is a history path only leaves the
    // copy without history.
    let mut source = match super::io::open_regular(path) {
        Ok(file) => file,
        // Recheck on the next mutation until a fresh session is actually saved.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    let history_path = super::io::session_history_path(super::io::containing_directory(path));
    let mut history_source = match super::io::open_regular(&history_path) {
        Ok(file) => Some(file),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) if super::io::is_not_regular(&err) => {
            tracing::warn!(
                event = kind.event(), subsystem = "persist", outcome = "history_skipped",
                path = %history_path.display(), error = %err,
                "preserving the session recovery copy without its history"
            );
            None
        }
        Err(err) => return Err(err),
    };
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
        let layout_name = RecoveryName::new(timestamp, sequence, RecoveryFileKind::Layout);
        let backup = directory.join(layout_name.file_name());
        let history_backup = directory.join(layout_name.pair().file_name());
        if backup.try_exists()? || history_backup.try_exists()? {
            continue;
        }

        // Publish history first. The layout filename is the recovery copy's
        // commit marker, so a layout backup never appears without its pair.
        // The `AlreadyExists` arms below are a backstop: under the data
        // directory lease there is one writer, and the existence check above
        // already skips a taken name, so only a file appearing in between
        // (which the lease rules out) reaches them.
        let copied_history = match history_source.as_mut() {
            Some(history) => {
                history.rewind()?;
                match copy_recovery(history, &history_backup) {
                    Ok(()) => true,
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(err) => return Err(err),
                }
            }
            None => false,
        };
        if let Err(err) = source.rewind() {
            if copied_history {
                remove_recovery_history_copy(&history_backup)?;
            }
            return Err(err);
        }
        match copy_recovery(&mut source, &backup) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if copied_history {
                    remove_recovery_history_copy(&history_backup)?;
                }
                continue;
            }
            Err(err) => {
                if copied_history {
                    remove_recovery_history_copy(&history_backup)?;
                }
                return Err(err);
            }
        }
        // Recovery-copy events use their own labels; the platform's session
        // helpers emit through tracing too but only cover session mutations.
        log_recovery_preserved(kind, path, &backup);
        let copy_prune_error = prune_recovery_copies(&older, keep).err();
        let orphan_prune_error = prune_orphaned_recovery_histories(&directory, &backup).err();
        if let Some(err) = copy_prune_error.or(orphan_prune_error) {
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
    // Without `replace` a failed directory sync withdraws the copy and comes
    // back as an error, so `NotDurable` cannot happen here; treat it as a
    // failure anyway rather than count an unsynced copy as a recovery copy.
    if let super::io::Published::NotDurable(err) =
        super::io::publish_private_file(source, &backup.with_extension("pending"), backup, false)?
    {
        super::io::remove_after_failed_publish(backup);
        return Err(err);
    }
    // The recovery directory may have just been created; its own entry must be
    // durable too, or the copy is not a recovery copy at all.
    let directory = super::io::containing_directory(backup);
    if let Err(err) = shepr_platform::sync_directory(super::io::containing_directory(directory)) {
        super::io::remove_after_failed_publish(backup);
        return Err(err);
    }
    Ok(())
}

fn recovery_files(directory: &Path) -> io::Result<Vec<(RecoveryKey, PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && let Some(name) = entry.file_name().to_str().and_then(RecoveryName::parse)
            && name.kind == RecoveryFileKind::Layout
        {
            files.push((name.key, entry.path()));
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
            Ok(()) => {
                remaining -= 1;
                if let Err(err) = remove_recovery_history_copy(&recovery_history_path(path)?) {
                    failure = Some(err);
                }
            }
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

fn remove_recovery_history_copy(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn prune_orphaned_recovery_histories(directory: &Path, newest_layout: &Path) -> io::Result<()> {
    let newest_key = newest_layout
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(RecoveryName::parse)
        .filter(|name| name.kind == RecoveryFileKind::Layout)
        .map(|name| name.key)
        .ok_or_else(|| io::Error::other("invalid newest recovery layout name"))?;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().and_then(RecoveryName::parse) else {
            continue;
        };
        if name.kind != RecoveryFileKind::History || name.key >= newest_key {
            continue;
        }
        let layout = directory.join(name.pair().file_name());
        match std::fs::symlink_metadata(&layout) {
            Ok(metadata) if metadata.file_type().is_file() => continue,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
fn recovery_filename(timestamp: u128, sequence: usize) -> String {
    RecoveryName::new(timestamp, sequence, RecoveryFileKind::Layout).file_name()
}

#[cfg(test)]
fn history_recovery_filename(timestamp: u128, sequence: usize) -> String {
    RecoveryName::new(timestamp, sequence, RecoveryFileKind::History).file_name()
}

#[cfg(test)]
fn recovery_timestamp(name: &str) -> Option<u128> {
    let name = RecoveryName::parse(name)?;
    (name.kind == RecoveryFileKind::Layout).then_some(name.key.timestamp)
}

#[cfg(test)]
impl SessionWriter {
    fn finish_save(
        &mut self,
        result: io::Result<super::io::Published>,
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
            UNIX_EPOCH,
        )
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

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

    fn writer(protect_unloaded: bool) -> SessionWriter {
        let directory = crate::test_support::ScratchDir::new("session-recovery");
        SessionWriter::new(
            super::super::lock::DataDirLease::acquire(&directory).expect("lease"),
            protect_unloaded,
        )
    }

    fn snapshot() -> SessionSnapshot {
        serde_json::from_value(serde_json::json!({
            "version": super::super::snapshot::SNAPSHOT_VERSION,
            "host_theme": super::super::snapshot::SavedHostTheme::default(),
            "workspaces": [{
                "id": "w1",
                "custom_name": null,
                "next_public_pane_number": 2,
                "layout": { "Pane": 0 },
                "panes": {
                    "0": {
                        "cwd": "/shepr-writer-test",
                        "public_number": 1,
                        "label": null
                    }
                },
                "zoomed": false,
                "focused": 0,
                "root_pane": 0
            }],
            "active": 0
        }))
        .expect("test precondition")
    }

    fn backups(writer: &SessionWriter) -> Vec<Vec<u8>> {
        let directory = super::super::io::backup_directory(&writer.path);
        if !directory.try_exists().expect("test stat") {
            return Vec::new();
        }
        recovery_files(&directory)
            .expect("test precondition")
            .into_iter()
            .map(|(_, path)| std::fs::read(path).expect("test precondition"))
            .collect()
    }

    fn snapshots(writer: &SessionWriter) -> Vec<(u128, PathBuf)> {
        recovery_files(&super::super::io::snapshot_directory(&writer.path))
            .expect("test precondition")
            .into_iter()
            .map(|(key, path)| (key.timestamp, path))
            .collect()
    }

    fn paired_history_backups(directory: &Path) -> Vec<Vec<u8>> {
        recovery_files(directory)
            .expect("test precondition")
            .into_iter()
            .filter_map(|(_, layout)| {
                let history = recovery_history_path(&layout).expect("valid recovery name");
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
        let snapshot_directory = super::super::io::snapshot_directory(&writer.path);
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
                false,
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
        let history_path = super::super::io::session_history_path(
            super::super::io::containing_directory(&writer.path),
        );
        super::super::io::save_to_path(&writer.path, &snapshot(), None).expect("test precondition");
        std::fs::write(&history_path, b"matching screen history").expect("test precondition");

        writer.save_for_test(&snapshot(), None).expect("save");

        let saved_layouts = snapshots(&writer);
        assert_eq!(saved_layouts.len(), 1);
        assert_eq!(
            std::fs::read(recovery_history_path(&saved_layouts[0].1).expect("valid recovery name"))
                .expect("paired history snapshot"),
            b"matching screen history"
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_history_is_bounded_and_does_not_rotate_identical_layouts() {
        let mut writer = writer(false);
        let directory = super::super::io::snapshot_directory(&writer.path);
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
        writer.save_for_test(&snapshot(), None).expect("save");
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
        writer.save_for_test(&snapshot(), None).expect("save");
        assert_eq!(snapshots(&writer), vec![(1, old)]);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn an_unfingerprintable_layout_is_not_treated_as_identical() {
        let known = SavedLayout::Known(super::super::snapshot::LayoutFingerprint::from_bytes(
            b"known",
        ));
        let same = SavedLayout::Known(super::super::snapshot::LayoutFingerprint::from_bytes(
            b"same",
        ));
        let changed = SavedLayout::Known(super::super::snapshot::LayoutFingerprint::from_bytes(
            b"changed",
        ));
        assert!(layout_differs_from_latest(
            &SavedLayout::Unknown,
            Some(&known)
        ));
        assert!(layout_differs_from_latest(&SavedLayout::Unknown, None));
        assert!(!layout_differs_from_latest(&same, Some(&same)));
        assert!(layout_differs_from_latest(&changed, Some(&same)));
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
        let workspace = &mut changed.workspaces[0];
        let pane = workspace.panes.remove(&0).expect("test precondition");
        workspace.panes.insert(1, pane);
        workspace.layout = super::super::snapshot::LayoutSnapshot::Pane(1);
        workspace.focused = 1;
        workspace.root_pane = 1;
        writer.save(&changed, None, rolled_back).expect("save");
        assert_eq!(snapshots(&writer).len(), 2);
        let path = writer.path.clone();
        drop(writer);
        writer = SessionWriter::new(
            super::super::lock::DataDirLease::acquire(path.parent().expect("directory"))
                .expect("lease"),
            false,
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
        let workspace = &mut changed.workspaces[0];
        let pane = workspace.panes.remove(&0).expect("test precondition");
        workspace.panes.insert(1, pane);
        workspace.layout = super::super::snapshot::LayoutSnapshot::Pane(1);
        workspace.focused = 1;
        workspace.root_pane = 1;
        writer
            .save(
                &changed,
                None,
                modified + SNAPSHOT_INTERVAL - std::time::Duration::from_nanos(1),
            )
            .expect("save inside interval");
        assert_eq!(snapshots(&writer).len(), 1);
        writer
            .save(&changed, None, modified + SNAPSHOT_INTERVAL)
            .expect("save at interval");
        assert_eq!(snapshots(&writer).len(), 2);
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn pruning_continues_past_an_undeletable_entry() {
        let writer = writer(false);
        let directory = super::super::io::backup_directory(&writer.path);
        std::fs::create_dir(&directory).expect("test precondition");
        let first = RecoveryName::new(1, 0, RecoveryFileKind::Layout);
        let second = RecoveryName::new(2, 0, RecoveryFileKind::Layout);
        let locked = directory.join(first.file_name());
        std::fs::create_dir(&locked).expect("test precondition");
        let removable = directory.join(second.file_name());
        std::fs::write(&removable, b"old").expect("test precondition");
        let older = [(first.key, locked.clone()), (second.key, removable.clone())];
        assert!(prune_recovery_copies(&older, 2).is_ok());
        assert!(locked.try_exists().expect("test stat"));
        assert!(!removable.try_exists().expect("test stat"));
        assert!(prune_recovery_copies(&[(first.key, locked)], 1).is_err());
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn snapshot_failure_does_not_block_primary_save_and_clear() {
        let mut writer = writer(false);
        std::fs::write(
            super::super::io::snapshot_directory(&writer.path),
            b"blocked",
        )
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
        for protect_unloaded in [false, true] {
            let mut writer = writer(protect_unloaded);
            if !protect_unloaded {
                super::super::io::save_to_path(&writer.path, &snapshot(), None)
                    .expect("test precondition");
            }
            writer.save_for_test(&snapshot(), None).expect("save");
            assert!(!writer.protect_unloaded);
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
        let history_path = super::super::io::session_history_path(
            super::super::io::containing_directory(&writer.path),
        );
        std::fs::write(&history_path, b"history").expect("test precondition");
        let directory = super::super::io::backup_directory(&writer.path);
        std::fs::write(&directory, b"blocks recovery").expect("test precondition");
        writer
            .save_for_test(&snapshot(), None)
            .expect_err("a blocked recovery copy must fail the save");
        writer
            .clear_for_test()
            .expect_err("a blocked recovery copy must fail the clear");
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
        writer.save_for_test(&snapshot(), None).expect("save");
        assert!(!writer.protect_unloaded);
        writer.save_for_test(&snapshot(), None).expect("save");
        writer.clear_for_test().expect("clear");
        assert!(!writer.path.try_exists().expect("test stat"));
        assert_eq!(backups(&writer), vec![original.to_vec()]);
        assert_eq!(
            paired_history_backups(&super::super::io::backup_directory(&writer.path)),
            vec![b"history".to_vec()]
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn optional_history_failure_is_reported_after_layout_saves() {
        let mut writer = writer(true);
        let history = super::super::io::session_history_path(
            super::super::io::containing_directory(&writer.path),
        );
        std::fs::create_dir(&history).expect("test precondition");
        std::fs::write(
            super::super::io::backup_directory(&writer.path),
            b"unavailable",
        )
        .expect("test precondition");
        assert!(writer.save_for_test(&snapshot(), None).is_err());
        assert!(
            !writer.protect_unloaded,
            "structural session was saved successfully"
        );
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("latest layout".into());
        assert!(writer.save_for_test(&changed, None).is_err());
        let saved: super::super::snapshot::SessionFile<SessionSnapshot> =
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
        let history = SessionHistory {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: Vec::new(),
        };
        let mut writer = writer(true);
        let history_path = super::super::io::session_history_path(
            super::super::io::containing_directory(&writer.path),
        );
        // The layout rename happened; only the directory sync after it failed.
        super::super::io::save_to_path(&writer.path, &snapshot(), None).expect("test precondition");
        assert!(
            writer
                .finish_save(
                    Ok(super::super::io::Published::NotDurable(io::Error::other(
                        "directory sync failed",
                    ))),
                    &snapshot(),
                    Some(&history),
                )
                .is_err()
        );
        assert!(!writer.protect_unloaded);
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
            true,
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
        assert!(failed.protect_unloaded);
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
            false,
        );
        assert_eq!(
            super::super::lock::DataDirLease::acquire(&directory)
                .err()
                .map(|err| err.kind()),
            Some(io::ErrorKind::ResourceBusy)
        );
    }

    #[test]
    fn retiring_releases_the_directory_and_ignores_later_saves() {
        let mut writer = writer(false);
        writer.save_for_test(&snapshot(), None).expect("save");
        assert!(writer.may_write().expect("active lease"));
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
        writer
            .save_for_test(&changed, None)
            .expect("a retired writer ignores the save");
        writer
            .clear_for_test()
            .expect("a retired writer ignores the clear");
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
        let history = |text: &str| SessionHistory {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![vec![(
                0,
                super::super::snapshot::HistoryText::single(std::sync::Arc::from(text)),
            )]],
        };
        let mut writer = writer(false);
        let history_path = super::super::io::session_history_path(
            super::super::io::containing_directory(&writer.path),
        );
        writer
            .save_for_test(&snapshot(), Some(&history("one")))
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
            .save_for_test(&snapshot(), Some(&history("one")))
            .expect("save");
        assert_eq!(
            inode(),
            written,
            "unchanged history should keep the file written by the previous save"
        );

        writer
            .save_for_test(&snapshot(), Some(&history("two")))
            .expect("save");
        let changed = std::fs::read(&history_path).expect("test precondition");
        assert!(String::from_utf8_lossy(&changed).contains("two"));

        // A clear forgets what was written, so the same history is written
        // again afterwards.
        writer.clear_for_test().expect("clear");
        assert!(!history_path.try_exists().expect("test stat"));
        writer
            .save_for_test(&snapshot(), Some(&history("two")))
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
        let history = SessionHistory {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![vec![(
                0,
                super::super::snapshot::HistoryText::single(std::sync::Arc::from("screen")),
            )]],
        };
        let mut writer = writer(false);
        let history_path = super::super::io::session_history_path(
            super::super::io::containing_directory(&writer.path),
        );
        let named = |writer: &SessionWriter| {
            let layout: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&writer.path).expect("layout"))
                    .expect("layout json");
            layout["history_digest"].as_str().map(str::to_owned)
        };
        let file_digest = || {
            super::super::io::history_digest(&std::fs::read(&history_path).expect("history"))
                .to_hex()
        };

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
        let history = SessionHistory {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: Vec::new(),
        };
        let mut writer = writer(true);
        std::fs::write(&writer.path, b"unloaded session").expect("test precondition");
        let history_path = super::super::io::session_history_path(
            super::super::io::containing_directory(&writer.path),
        );
        std::fs::create_dir(&history_path).expect("test precondition");
        // The unloaded session is still preserved, without its history, and
        // the layout is saved; only the history write fails.
        assert!(writer.save_for_test(&snapshot(), Some(&history)).is_err());
        assert!(!writer.protect_unloaded);
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
        let history = || SessionHistory {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: Vec::new(),
        };
        let mut writer = writer(false);
        let history_path = super::super::io::session_history_path(
            super::super::io::containing_directory(&writer.path),
        );
        writer
            .save_for_test(&snapshot(), Some(&history()))
            .expect("save");
        let expected = std::fs::read(&history_path).expect("test precondition");

        std::fs::remove_file(&history_path).expect("test precondition");
        writer
            .save_for_test(&snapshot(), Some(&history()))
            .expect("save after history deletion");

        assert_eq!(
            std::fs::read(&history_path).expect("history is restored"),
            expected
        );
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn orphaned_history_copies_older_than_the_newest_layout_are_pruned() {
        let scratch = shepr_test_support::ScratchDir::new("orphan-history-copies");
        let directory = scratch.path();
        let write = |name: String| {
            std::fs::write(directory.join(name), "{}").expect("test precondition");
        };
        // A paired copy, an orphan older than the newest layout, the newest
        // layout, and a history copy newer than it (a copy still being made).
        write(recovery_filename(1, 0));
        write(history_recovery_filename(1, 0));
        write(history_recovery_filename(2, 0));
        write(recovery_filename(3, 0));
        write(history_recovery_filename(4, 0));

        prune_orphaned_recovery_histories(directory, &directory.join(recovery_filename(3, 0)))
            .expect("prune");

        let exists = |name: String| directory.join(name).try_exists().expect("test stat");
        assert!(exists(history_recovery_filename(1, 0)));
        assert!(!exists(history_recovery_filename(2, 0)));
        assert!(exists(history_recovery_filename(4, 0)));
        assert!(exists(recovery_filename(1, 0)));
        assert!(exists(recovery_filename(3, 0)));
    }

    #[test]
    fn recovery_filename_timestamp_round_trips() {
        let timestamp = 1_729_123_456_789_012_345_678_901_234_567_890u128;
        let name = RecoveryName::new(timestamp, 7, RecoveryFileKind::Layout);
        let filename = name.file_name();
        assert_eq!(
            filename,
            format!("session-{timestamp:0RECOVERY_TIMESTAMP_DIGITS$}-007.json")
        );
        assert_eq!(recovery_timestamp(&filename), Some(timestamp));
    }

    #[test]
    fn pruning_leaves_user_named_recovery_files_alone() {
        let mut writer = writer(true);
        let directory = super::super::io::backup_directory(&writer.path);
        std::fs::create_dir(&directory).expect("test precondition");
        let manual = directory.join("session-000-manual.json");
        std::fs::write(&manual, b"manual recovery copy").expect("test precondition");
        for i in 0..5u8 {
            writer.protect_unloaded = true;
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
        assert!(writer.protect_unloaded);
        std::fs::write(&writer.path, b"late layout").expect("test precondition");
        writer.clear_for_test().expect("clear");
        assert!(!writer.path.try_exists().expect("test stat"));
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
        writer
            .save_for_test(&snapshot(), None)
            .expect_err("a directory in the temporary's place must fail the save");
        writer
            .save_for_test(&snapshot(), None)
            .expect_err("a directory in the temporary's place must fail the save");
        assert_eq!(
            std::fs::read(&writer.path).expect("test precondition"),
            b"original"
        );
        std::fs::remove_dir(&temporary).expect("test precondition");
        writer.save_for_test(&snapshot(), None).expect("save");
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
            .with_file_name("session-000000000000000000000000000000000000001-000.json");
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
    fn stale_recovery_temporary_is_removed_before_reusing_its_name() {
        let writer = writer(true);
        let directory = super::super::io::backup_directory(&writer.path);
        std::fs::create_dir(&directory).expect("test precondition");
        let backup = directory.join(recovery_filename(123, 0));
        let pending = backup.with_extension("pending");
        std::fs::write(&pending, b"interrupted copy prefix").expect("test precondition");

        copy_recovery(&mut io::Cursor::new(b"complete copy"), &backup).expect("copy recovery");

        assert_eq!(
            std::fs::read(&backup).expect("published recovery copy"),
            b"complete copy"
        );
        assert!(!pending.try_exists().expect("test stat"));
        std::fs::remove_dir_all(writer.path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn recovery_order_survives_clock_rollback() {
        let mut writer = writer(true);
        let directory = super::super::io::backup_directory(&writer.path);
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
            writer.protect_unloaded = true;
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
            writer.protect_unloaded = true;
            std::fs::write(&writer.path, [i]).expect("test precondition");
            let history_path = super::super::io::session_history_path(
                super::super::io::containing_directory(&writer.path),
            );
            std::fs::write(&history_path, [i + 10]).expect("test precondition");
            writer.save_for_test(&snapshot(), None).expect("save");
        }
        assert_eq!(backups(&writer), vec![vec![2], vec![3], vec![4]]);
        assert_eq!(
            paired_history_backups(&super::super::io::backup_directory(&writer.path)),
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
                let backup = std::fs::read_dir(super::super::io::backup_directory(&writer.path))
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
