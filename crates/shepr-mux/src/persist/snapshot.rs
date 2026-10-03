use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::pane::{HistoryPiece, PaneRuntimeRegistry};
use crate::terminal::Label;
use crate::workspace::Workspace;
use shepr_core::layout::{Direction, Node};
use shepr_core::limits::PALETTE_COLOR_COUNT;
use shepr_protocol::TerminalId;

/// Current snapshot format version. Deserialization rejects every other value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SnapshotVersion(u32);

pub const SNAPSHOT_VERSION: SnapshotVersion = SnapshotVersion(1);

impl<'de> Deserialize<'de> for SnapshotVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u32::deserialize(deserializer)?;
        if value == SNAPSHOT_VERSION.0 {
            Ok(SNAPSHOT_VERSION)
        } else {
            Err(serde::de::Error::custom(format!(
                "snapshot version {value} is not supported (expected {})",
                SNAPSHOT_VERSION.0
            )))
        }
    }
}

/// Paths stay readable when they are UTF-8. Linux paths with arbitrary bytes
/// use a JSON byte sequence so one pane cannot make the whole save fail.
mod path_bytes {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use serde::de::{SeqAccess, Visitor};
    use serde::{Deserializer, Serialize, Serializer};

    pub(crate) fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(utf8) = path.to_str() {
            serializer.serialize_str(utf8)
        } else {
            path.as_os_str().as_bytes().serialize(serializer)
        }
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PathVisitor;

        impl<'de> Visitor<'de> for PathVisitor {
            type Value = PathBuf;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a UTF-8 path string or a sequence of path bytes")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(PathBuf::from(value))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(PathBuf::from(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut bytes = Vec::with_capacity(sequence.size_hint().unwrap_or_default());
                while let Some(byte) = sequence.next_element::<u8>()? {
                    bytes.push(byte);
                }
                Ok(PathBuf::from(OsString::from_vec(bytes)))
            }
        }

        deserializer.deserialize_any(PathVisitor)
    }
}

// The serde types below are the on-disk schema itself: every field is
// required, a nullable one included, so a key a save always writes cannot go
// missing unnoticed. A file that does not match fails to parse and takes the
// unusable-file path (backed up, then replaced), whole. Keeping a second,
// hand-written schema in front of lenient types, or `Option`s and defaults
// for damaged in-memory fixtures, lets the two drift; restore validates only
// what a type cannot express. The one tolerance is a pane's agent session
// (see `deserialize_agent_session`).

/// Serializable snapshot of the entire shepr session.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    /// Format version - used to detect incompatible changes.
    pub version: SnapshotVersion,
    pub host_theme: SavedHostTheme,
    pub workspaces: Vec<WorkspaceSnapshot>,
    /// The workspace the session's bookmark names: where a client with no
    /// location of its own starts.
    #[serde(deserialize_with = "required_nullable")]
    pub active: Option<usize>,
}

/// One saved layout file, including the history bytes it names. `T` is the
/// borrowed snapshot form while writing and the owned form while reading.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFile<T> {
    pub snapshot: T,
    #[serde(deserialize_with = "required_nullable")]
    pub history_digest: Option<super::HistoryDigest>,
}

// Serde fills a missing `Option` field with `None` unless the field has its
// own `deserialize_with`, which makes the key required while its value may
// still be `null`.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Last observed physical terminal colours, retained for headless resumes.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedHostTheme {
    #[serde(deserialize_with = "required_nullable")]
    pub foreground: Option<shepr_term::host::RgbColor>,
    #[serde(deserialize_with = "required_nullable")]
    pub background: Option<shepr_term::host::RgbColor>,
    #[serde(deserialize_with = "deserialize_palette")]
    pub palette: Vec<Option<shepr_term::host::RgbColor>>,
}

// serde has no array impl past 32 entries, so the palette is a `Vec` whose
// length is checked here.
fn deserialize_palette<'de, D>(
    deserializer: D,
) -> Result<Vec<Option<shepr_term::host::RgbColor>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let palette = Vec::<Option<shepr_term::host::RgbColor>>::deserialize(deserializer)?;
    if palette.len() != PALETTE_COLOR_COUNT {
        return Err(serde::de::Error::custom(format!(
            "palette has {} colors, expected {PALETTE_COLOR_COUNT}",
            palette.len()
        )));
    }
    Ok(palette)
}

impl Default for SavedHostTheme {
    fn default() -> Self {
        Self {
            foreground: None,
            background: None,
            palette: vec![None; PALETTE_COLOR_COUNT],
        }
    }
}

impl From<shepr_term::host::TerminalTheme> for SavedHostTheme {
    fn from(theme: shepr_term::host::TerminalTheme) -> Self {
        Self {
            foreground: theme.foreground,
            background: theme.background,
            palette: theme.palette.into(),
        }
    }
}

impl SavedHostTheme {
    pub fn to_theme(&self) -> shepr_term::host::TerminalTheme {
        let mut theme = shepr_term::host::TerminalTheme {
            foreground: self.foreground,
            background: self.background,
            ..Default::default()
        };
        for (index, color) in self.palette.iter().take(PALETTE_COLOR_COUNT).enumerate() {
            theme.palette[index] = *color;
        }
        theme
    }
}

#[derive(Serialize, Deserialize)]
pub struct SessionHistorySnapshot {
    /// Format version follows the matching session snapshot version.
    pub version: SnapshotVersion,
    pub workspaces: Vec<WorkspaceHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceHistorySnapshot {
    #[serde(serialize_with = "serialize_history_panes")]
    pub panes: HashMap<u32, PaneHistorySnapshot>,
}

/// JSON object fields follow pane ID order so the same captured history has
/// the same bytes, regardless of each `HashMap`'s randomized iteration order.
fn serialize_history_panes<S>(
    panes: &HashMap<u32, PaneHistorySnapshot>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;

    let mut entries: Vec<_> = panes.iter().collect();
    entries.sort_unstable_by_key(|entry| *entry.0);
    let mut map = serializer.serialize_map(Some(entries.len()))?;
    for (pane_id, pane) in entries {
        map.serialize_entry(pane_id, pane)?;
    }
    map.end()
}

/// One saved workspace. Its identity cwd is not saved: restore derives it from
/// the restored root pane's cwd.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    /// Canonical identity; restore assigns a fresh identity to duplicates.
    pub id: shepr_protocol::WorkspaceId,
    #[serde(deserialize_with = "required_nullable")]
    pub custom_name: Option<String>,
    /// Restore checks it against the panes' numbers.
    pub next_public_pane_number: shepr_protocol::PanePublicNumber,
    pub layout: LayoutSnapshot,
    pub panes: HashMap<u32, PaneSnapshot>,
    pub zoomed: bool,
    pub focused: u32,
    pub root_pane: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneSnapshot {
    #[serde(
        serialize_with = "path_bytes::serialize",
        deserialize_with = "path_bytes::deserialize"
    )]
    // Saved paths may disappear between capture and restore. Keep the path
    // observation; restore and the child's required chdir own admission.
    // Absolute-path validation at deserialization would reject the whole
    // strict session file for a value defect that must drop only this pane.
    pub cwd: PathBuf,
    /// Decoding refuses zero; restore refuses repeats within a workspace.
    pub public_number: shepr_protocol::PanePublicNumber,
    #[serde(deserialize_with = "required_nullable")]
    pub label: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_agent_session"
    )]
    pub agent_session: Option<PaneAgentSessionSnapshot>,
}

pub type PaneAgentSessionSnapshot = shepr_agent::resume::PersistedAgentSession;

// Agent labels and session formats can disappear between builds. A bad saved
// session must not discard the pane or unrelated workspaces.
fn deserialize_agent_session<'de, D>(
    deserializer: D,
) -> Result<Option<PaneAgentSessionSnapshot>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match serde_json::from_value(value) {
        Ok(session) => Some(session),
        Err(error) => {
            tracing::warn!(%error, "ignoring invalid saved agent session");
            None
        }
    }))
}

/// Saved screen history of one pane.
#[derive(Serialize, Deserialize)]
pub struct PaneHistorySnapshot {
    pub ansi: String,
}

/// Serializable BSP tree.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    variant_size_differences,
    reason = "a split is 23 bytes; boxing it would allocate per split to save that much per leaf"
)]
pub enum LayoutSnapshot {
    Pane(u32),
    Split {
        direction: DirectionSnapshot,
        ratio: shepr_core::layout::SplitRatio,
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub enum DirectionSnapshot {
    Horizontal,
    Vertical,
}

/// Where a pane sits in a snapshot: workspace index and saved pane ID.
pub type SavedPaneRef = (usize, u32);

/// The live cwd probe reads a capture left for whoever writes the snapshot.
/// Reading a cwd is a /proc access per pane, which the event loop should not
/// pay per save, so a capture records the best cwd it knows without one and
/// hands over a [`PaneCwdProbe`](crate::pane::PaneCwdProbe) per runtime; [`resolve`]
/// applies the same OSC 7 and /proc arbitration used by live panes.
///
/// [`resolve`]: Self::resolve
#[derive(Default)]
pub struct PendingCwds {
    probes: Vec<(SavedPaneRef, crate::pane::PaneCwdProbe)>,
}

impl PendingCwds {
    /// Reads every probe and stores the best result in `snapshot`.
    pub fn resolve(self, snapshot: &mut SessionSnapshot) {
        for ((workspace, pane), probe) in self.probes {
            let Some(cwd) = probe.read() else {
                continue;
            };
            if let Some(saved) = snapshot
                .workspaces
                .get_mut(workspace)
                .and_then(|workspace| workspace.panes.get_mut(&pane))
            {
                saved.cwd = cwd;
            }
        }
    }
}

/// Capture the current app state into a serializable snapshot, refreshing each
/// runtime's cwd now. A save uses [`capture_deferred`] instead.
pub fn capture(
    workspaces: &[Workspace],
    terminals: &std::collections::HashMap<
        shepr_protocol::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
    active: Option<usize>,
    host_theme: shepr_term::host::TerminalTheme,
) -> SessionSnapshot {
    let (mut snapshot, cwds, _) = capture_deferred(
        workspaces,
        terminals,
        terminal_runtimes,
        fallback_cwd,
        active,
        host_theme,
    );
    cwds.resolve(&mut snapshot);
    snapshot
}

/// Capture the current app state without reading any shell's /proc cwd: the
/// snapshot holds each pane's best known cwd, [`PendingCwds`] refreshes it where
/// the snapshot is written, and the map keys each saved pane to its terminal.
pub fn capture_deferred(
    workspaces: &[Workspace],
    terminals: &std::collections::HashMap<
        shepr_protocol::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
    active: Option<usize>,
    host_theme: shepr_term::host::TerminalTheme,
) -> (
    SessionSnapshot,
    PendingCwds,
    HashMap<SavedPaneRef, TerminalId>,
) {
    let mut cwds = PendingCwds::default();
    let mut terminal_ids = HashMap::new();
    let snapshot = SessionSnapshot {
        version: SNAPSHOT_VERSION,
        host_theme: host_theme.into(),
        workspaces: workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                capture_workspace(
                    index,
                    workspace,
                    terminals,
                    terminal_runtimes,
                    fallback_cwd,
                    &mut cwds,
                    &mut terminal_ids,
                )
            })
            .collect(),
        active,
    };
    (snapshot, cwds, terminal_ids)
}

fn capture_workspace(
    workspace_index: usize,
    ws: &Workspace,
    terminals: &std::collections::HashMap<
        shepr_protocol::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
    cwds: &mut PendingCwds,
    terminal_ids: &mut HashMap<SavedPaneRef, TerminalId>,
) -> WorkspaceSnapshot {
    let mut panes = HashMap::new();
    for (id, workspace_pane) in &ws.panes {
        let pane_ref: SavedPaneRef = (workspace_index, id.raw());
        let terminal_id = ws.terminal_id(*id);
        if let Some(terminal_id) = terminal_id {
            terminal_ids.insert(pane_ref, terminal_id.clone());
        }
        let terminal = terminal_id.and_then(|id| terminals.get(id));
        let runtime = terminal_id.and_then(|id| terminal_runtimes.get(id));
        let cwd =
            crate::workspace::terminal_cwd(runtime, terminal, crate::workspace::CwdPurpose::Save)
                .unwrap_or_else(|| fallback_cwd.to_path_buf());
        if let Some(runtime) = runtime {
            cwds.probes.push((pane_ref, runtime.cwd_probe()));
        }
        let label = terminal.and_then(|terminal| terminal.manual_label_value().cloned());
        let agent_session = terminal.and_then(|terminal| {
            terminal
                .ownership()
                .current_session_identity_for_persistence()
        });
        panes.insert(
            pane_ref.1,
            PaneSnapshot {
                cwd,
                public_number: workspace_pane.public_number,
                label,
                agent_session,
            },
        );
    }
    WorkspaceSnapshot {
        id: ws.id,
        custom_name: ws.custom_name.clone(),
        next_public_pane_number: ws.next_public_pane_number,
        layout: capture_node(ws.layout.root()),
        panes,
        zoomed: ws.zoomed,
        focused: ws.layout.focused().raw(),
        root_pane: ws.root_pane.raw(),
    }
}

// The layout's shape and pane IDs, which tell whether two layouts are the
// same for recovery-copy cadence (the writer's snapshot history). It does not
// pair a history file with a layout: pane IDs are process-local and restore
// reassigns them, so two layouts can share a fingerprint while their panes
// hold each other's scrollback. A layout names its history by the digest of
// the history bytes its own save serialized instead.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct LayoutFingerprint([u8; 32]);

impl LayoutFingerprint {
    pub(super) fn from_bytes(bytes: &[u8]) -> Self {
        Self(super::io::sha256_bytes(bytes))
    }
}

pub(super) fn layout_fingerprint(snapshot: &SessionSnapshot) -> Option<LayoutFingerprint> {
    // The encoding uses tagged tree nodes and fixed-width little-endian counts,
    // IDs, and ratios. Pane IDs are sorted, and other saved fields do not say
    // which recovery copy's screen history belongs to each pane.
    let mut encoding = Vec::new();
    append_fingerprint_count(snapshot.workspaces.len(), &mut encoding)?;
    for workspace in &snapshot.workspaces {
        append_layout_fingerprint(&workspace.layout, &mut encoding)?;
        let mut pane_ids: Vec<_> = workspace.panes.keys().copied().collect();
        pane_ids.sort_unstable();
        append_fingerprint_count(pane_ids.len(), &mut encoding)?;
        for pane_id in pane_ids {
            encoding.extend_from_slice(&pane_id.to_le_bytes());
        }
    }
    Some(LayoutFingerprint::from_bytes(&encoding))
}

fn append_fingerprint_count(count: usize, encoding: &mut Vec<u8>) -> Option<()> {
    encoding.extend_from_slice(&u64::try_from(count).ok()?.to_le_bytes());
    Some(())
}

fn append_layout_fingerprint(layout: &LayoutSnapshot, encoding: &mut Vec<u8>) -> Option<()> {
    match layout {
        LayoutSnapshot::Pane(pane_id) => {
            encoding.push(0);
            encoding.extend_from_slice(&pane_id.to_le_bytes());
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
            append_layout_fingerprint(first, encoding)?;
            append_layout_fingerprint(second, encoding)?;
        }
    }
    Some(())
}

/// One pane's history text as a save holds it: the pieces its history cache
/// keeps (or the one piece restored from the file), shared with the cache so
/// that holding the text for a save copies none of it. The text is the pieces
/// in order, each preceded by `\r\n` when its `break_before` says so.
#[derive(Clone, Debug)]
pub struct HistoryText {
    pub(super) pieces: Vec<HistoryPiece>,
}

impl HistoryText {
    pub(super) fn single(text: Arc<str>) -> Self {
        Self {
            pieces: vec![HistoryPiece {
                text,
                break_before: false,
            }],
        }
    }

    /// The text as one string. A save does not need it (it serializes the
    /// pieces), which is the point of keeping them apart.
    pub(super) fn assemble(&self) -> String {
        let mut text = String::with_capacity(
            self.pieces
                .iter()
                .map(|piece| piece.text.len() + 2)
                .sum::<usize>(),
        );
        for piece in &self.pieces {
            if piece.break_before {
                text.push_str("\r\n");
            }
            text.push_str(&piece.text);
        }
        text
    }
}

/// What a save writes as the history file, before serializing: the pane
/// histories of each workspace, sorted by pane number, as
/// [`HistoryText`]. The write-side twin of [`SessionHistorySnapshot`], which
/// is what reading the file gives; it serializes to the same JSON.
#[derive(Clone)]
pub struct SessionHistory {
    pub(super) version: SnapshotVersion,
    pub(super) workspaces: Vec<Vec<(u32, HistoryText)>>,
}

impl SessionHistory {
    /// The history with every pane's text assembled into one string.
    pub(super) fn into_snapshot(self) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: self.version,
            workspaces: self
                .workspaces
                .into_iter()
                .map(|panes| WorkspaceHistorySnapshot {
                    panes: panes
                        .into_iter()
                        .map(|(id, text)| {
                            (
                                id,
                                PaneHistorySnapshot {
                                    ansi: text.assemble(),
                                },
                            )
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// The history file's text for a pane that has not run yet; see
/// `HistoryCarry`.
struct RestoredEntry {
    ansi: Arc<str>,
    /// Names `ansi` the way a `PaneHistoryCache` revision names its text.
    revision: HistoryRevision,
}

/// Restored and live text have distinct identity spaces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryRevision {
    Live(u64),
    Restored(u64),
}

/// Names for restored text, independent of live cache revisions.
fn next_restored_revision() -> HistoryRevision {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    HistoryRevision::Restored(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// What a save's history holds for one pane, by content identity rather than
/// content: two saves with equal stamps hold equal text.
type PaneStamp = Option<HistoryRevision>;

/// What one save's history was made of: the content of every pane, under the
/// workspace position and pane number it is saved with, sorted by pane
/// number. Two equal stamps serialize to the same bytes: the history file
/// holds nothing but that mapping (and the format version), and one build's
/// serializer and cap are fixed.
#[derive(PartialEq, Eq)]
struct HistoryStamp {
    panes: Vec<Vec<(u32, PaneStamp)>>,
}

/// What resolving a save's history produced.
pub enum ResolvedHistory {
    /// The history file already holds exactly this history, whose digest is
    /// given: nothing was assembled and nothing needs writing. Only ever
    /// produced when the caller allowed it.
    Unchanged(super::io::HistoryDigest),
    Changed(SessionHistory),
}

/// Pane history kept from one save to the next. Restore creates it and hands
/// it to the session persister, which owns it from then on: every history
/// capture is resolved against it, on the persister's thread, in save order,
/// so nothing else touches it and it needs no lock.
///
/// It keeps, keyed by terminal ID:
///
/// - The history a pane's reader formatted before (`readers`, a
///   `PaneHistoryCache`), so a save formats only the lines that are new since
///   the last one. This is also the one copy of a pane's history that outlives
///   a save: a pane on the alternate screen (vim, an agent TUI) cannot have
///   its primary screen read, so saves use what the cache held at its last
///   successful read, and a pane that lost its runtime keeps the text it had.
///   A successful read of an empty screen leaves the cache with no text, which
///   saves as no history.
/// - What a pane's live screen cannot supply (`restored`): a restored pane
///   without a runtime (deferred agent resume, failed restore) keeps its
///   history from the loaded file until it runs. Capture reads live runtimes
///   only, so without this a save made before the pane runs would lose its
///   saved screen for good. The first save that sees a runtime for the pane
///   drops that entry: from then on the pane's own screen supersedes it, even
///   if its first read happens on the alternate screen.
/// - What the last successful save's history was made of (`saved`), so a save
///   with the same content is recognised without assembling, serializing or
///   hashing any text.
///
/// Each save drops the entries of panes no longer in its layout. That pruning
/// is why this belongs to one app's persister instead of being process-wide:
/// a save only knows its own layout, and would drop every other owner's
/// entries.
#[derive(Default)]
pub struct HistoryCarry {
    restored: HashMap<TerminalId, RestoredEntry>,
    readers: HashMap<TerminalId, crate::pane::PaneHistoryCache>,
    /// The stamp of the history the last successful save wrote, with the
    /// digest its layout names it by.
    saved: Option<(HistoryStamp, super::io::HistoryDigest)>,
    /// The stamp of the history resolved for the save in progress.
    resolved: Option<HistoryStamp>,
}

impl HistoryCarry {
    /// Keeps a restored pane's saved history for later saves until the pane
    /// has a runtime of its own.
    pub fn carry_restored(&mut self, terminal: &TerminalId, history: Option<&PaneHistorySnapshot>) {
        if let Some(history) = history {
            self.restored.insert(
                terminal.clone(),
                RestoredEntry {
                    ansi: Arc::from(history.ansi.as_str()),
                    revision: next_restored_revision(),
                },
            );
        }
    }

    /// Forgets every pane: a cleared session has none.
    pub(super) fn clear(&mut self) {
        self.restored.clear();
        self.readers.clear();
        self.forget_saved();
    }

    /// The save whose history was last resolved reached the disk, its layout
    /// naming that history by `digest`. A save that wrote no history
    /// (`None`) leaves nothing to skip against.
    pub(super) fn note_saved(&mut self, digest: Option<super::io::HistoryDigest>) {
        let resolved = self.resolved.take();
        self.saved = resolved.zip(digest);
    }

    /// The history file may not hold what the last resolution said; the next
    /// save writes its history in full.
    pub(super) fn forget_saved(&mut self) {
        self.saved = None;
        self.resolved = None;
    }

    /// Drops what belongs to panes outside `panes` (the ones a save saw).
    fn retain(&mut self, panes: &std::collections::HashSet<&TerminalId>) {
        self.restored.retain(|id, _| panes.contains(id));
        self.readers.retain(|id, _| panes.contains(id));
    }

    /// A pane without a runtime: what is carried for it (its restored
    /// history, else what its runtime's cache still holds), by content name.
    fn stamp_runtimeless(&self, terminal: &TerminalId) -> PaneStamp {
        if let Some(entry) = self.restored.get(terminal) {
            return Some(entry.revision);
        }
        self.readers
            .get(terminal)
            .filter(|cache| cache.has_text())
            .map(|cache| HistoryRevision::Live(cache.revision()))
    }

    /// A live pane: brings its cache up to date, or leaves it as it was while
    /// the alternate screen hides the primary screen, and names what it holds.
    fn stamp_live(
        &mut self,
        terminal: &TerminalId,
        source: &crate::pane::PaneHistorySource,
    ) -> PaneStamp {
        // The pane has a runtime of its own now: its own screen supersedes
        // the history restored for it, permanently, even while that screen is
        // on the alternate buffer and cannot be read.
        self.restored.remove(terminal);
        let cache = self.readers.entry(terminal.clone()).or_default();
        // A refresh that cannot complete leaves the cache as it was, and the
        // save carries that earlier read, whatever the reason.
        if let Err(reason) = source.refresh(cache) {
            tracing::debug!(?reason, "pane history refresh unavailable; keeping cache");
        }
        cache
            .has_text()
            .then(|| HistoryRevision::Live(cache.revision()))
    }

    /// The text a stamped pane saves, sharing the carried text.
    fn text(&self, terminal: &TerminalId) -> Option<HistoryText> {
        match self.restored.get(terminal) {
            Some(entry) => Some(HistoryText::single(Arc::clone(&entry.ansi))),
            None => self.readers.get(terminal).map(|cache| HistoryText {
                pieces: cache.pieces(),
            }),
        }
    }
}

enum PendingPaneHistory {
    /// A pane without a runtime: whatever history is carried for it.
    Runtimeless(TerminalId),
    /// A running pane, read where the history is resolved.
    Live(TerminalId, crate::pane::PaneHistorySource),
}

/// Pane history captured on the event loop: which pane each history belongs
/// to and a handle to read it through, nothing formatted. `resolve` turns it
/// into a `SessionHistorySnapshot` off the loop. The shape mirrors the
/// workspaces it was captured from.
pub struct PendingHistory {
    workspaces: Vec<Vec<(u32, PendingPaneHistory)>>,
}

impl PendingHistory {
    /// Formats every live pane's history, keyed as the layout it was captured
    /// alongside keys its panes. Saves go through `resolve_for_save`; this
    /// stays public for tests in this and the server crate, which have no
    /// other way to read a capture back. Meant for the persister's thread: it can
    /// take as long as formatting what is new in every pane's scrollback
    /// does, in bounded chunks per hold of each pane's terminal lock.
    pub fn resolve(self, carry: &mut HistoryCarry) -> SessionHistorySnapshot {
        match self.resolve_for_save(carry, false) {
            ResolvedHistory::Changed(history) => history.into_snapshot(),
            // Only produced when the caller allows it.
            ResolvedHistory::Unchanged(_) => SessionHistorySnapshot {
                version: SNAPSHOT_VERSION,
                workspaces: Vec::new(),
            },
        }
    }

    /// Like [`resolve`], for the persister: with `allow_unchanged` (the
    /// history file is known to hold what the last save wrote) a history with
    /// the same content as that save's is reported as `Unchanged` without
    /// assembling any text. The caller reports how the save went through
    /// `HistoryCarry::note_saved` or `forget_saved`.
    ///
    /// [`resolve`]: Self::resolve
    pub(super) fn resolve_for_save(
        self,
        carry: &mut HistoryCarry,
        allow_unchanged: bool,
    ) -> ResolvedHistory {
        carry.retain(
            &self
                .workspaces
                .iter()
                .flatten()
                .map(|(_, pending)| match pending {
                    PendingPaneHistory::Runtimeless(terminal)
                    | PendingPaneHistory::Live(terminal, _) => terminal,
                })
                .collect(),
        );
        // Bring every pane up to date first, naming what each one holds.
        let mut named: Vec<Vec<(u32, TerminalId, PaneStamp)>> =
            Vec::with_capacity(self.workspaces.len());
        for panes in self.workspaces {
            let mut named_panes: Vec<_> = panes
                .into_iter()
                .map(|(id, pending)| {
                    let (terminal, stamp) = match pending {
                        PendingPaneHistory::Runtimeless(terminal) => {
                            let stamp = carry.stamp_runtimeless(&terminal);
                            (terminal, stamp)
                        }
                        PendingPaneHistory::Live(terminal, source) => {
                            let stamp = carry.stamp_live(&terminal, &source);
                            (terminal, stamp)
                        }
                    };
                    (id, terminal, stamp)
                })
                .collect();
            named_panes.sort_unstable_by_key(|(id, _, _)| *id);
            named.push(named_panes);
        }

        // Keyed by the pane IDs of the layout this history is saved with:
        // live pane IDs are stable across saves, while restore allocates
        // fresh IDs and carries the saved history through the ID remap, so
        // the first save after a restore serializes the new mapping afresh.
        let stamp = HistoryStamp {
            panes: named
                .iter()
                .map(|panes| panes.iter().map(|(id, _, stamp)| (*id, *stamp)).collect())
                .collect(),
        };
        if allow_unchanged
            && let Some((saved, digest)) = &carry.saved
            && *saved == stamp
        {
            carry.resolved = Some(stamp);
            return ResolvedHistory::Unchanged(*digest);
        }
        carry.resolved = Some(stamp);
        ResolvedHistory::Changed(SessionHistory {
            version: SNAPSHOT_VERSION,
            workspaces: named
                .into_iter()
                .map(|panes| {
                    panes
                        .into_iter()
                        .filter_map(|(id, terminal, stamp)| {
                            let text = carry.text(&terminal).filter(|_| stamp.is_some())?;
                            Some((id, text))
                        })
                        .collect()
                })
                .collect(),
        })
    }
}

/// The event-loop half of a history capture; see `PendingHistory`. Takes no
/// terminal lock: a live pane contributes a handle to its terminal.
pub fn capture_pending_history(
    workspaces: &[Workspace],
    terminal_runtimes: &PaneRuntimeRegistry,
) -> PendingHistory {
    PendingHistory {
        workspaces: workspaces
            .iter()
            .map(|workspace| {
                workspace
                    .panes
                    .iter()
                    .map(|(id, pane)| {
                        let terminal = pane.attached_terminal_id.clone();
                        let runtime = terminal_runtimes.get(&terminal);
                        (id.raw(), pending_pane_history(terminal, runtime))
                    })
                    .collect()
            })
            .collect(),
    }
}

/// A pane's own screen supersedes its carried history only once its shell
/// launched. Until then (a chdir on a hung mount can last indefinitely) the
/// runtime holds a PTY and no shell, and a save must not trade the history
/// carried for it for that empty screen; if the launch then fails, the pane is
/// left with the carried history.
fn pending_pane_history(
    terminal: TerminalId,
    runtime: Option<&crate::pane::PaneRuntime>,
) -> PendingPaneHistory {
    match runtime.filter(|runtime| runtime.launched()) {
        Some(runtime) => PendingPaneHistory::Live(terminal, runtime.read().history_source()),
        None => PendingPaneHistory::Runtimeless(terminal),
    }
}

/// Captures fresh history handles for a previously captured session layout.
/// Panes removed since that layout was saved use the persister's carried
/// history, while panes that still have runtimes contribute their current
/// history. The terminal map must have been captured with `snapshot`.
pub fn capture_pending_history_for_snapshot(
    snapshot: &SessionSnapshot,
    terminal_ids: &HashMap<SavedPaneRef, TerminalId>,
    terminal_runtimes: &PaneRuntimeRegistry,
) -> Option<PendingHistory> {
    let mut workspaces = Vec::with_capacity(snapshot.workspaces.len());
    for (workspace_index, workspace) in snapshot.workspaces.iter().enumerate() {
        let mut pane_ids: Vec<_> = workspace.panes.keys().copied().collect();
        pane_ids.sort_unstable();
        let mut panes = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            let terminal = terminal_ids.get(&(workspace_index, pane_id))?.clone();
            let runtime = terminal_runtimes.get(&terminal);
            panes.push((pane_id, pending_pane_history(terminal, runtime)));
        }
        workspaces.push(panes);
    }
    Some(PendingHistory { workspaces })
}

/// Captures cwd probes for a previously captured session layout. A probe keeps
/// the best known cwd if its child has exited, and live panes keep their
/// checkpoint workspace and pane keys even if removals changed workspace indexes.
pub fn capture_pending_cwds_for_snapshot(
    snapshot: &SessionSnapshot,
    terminal_ids: &HashMap<SavedPaneRef, TerminalId>,
    terminal_runtimes: &PaneRuntimeRegistry,
) -> Option<PendingCwds> {
    let mut cwds = PendingCwds::default();
    for (workspace_index, workspace) in snapshot.workspaces.iter().enumerate() {
        for pane_id in workspace.panes.keys() {
            let terminal = terminal_ids.get(&(workspace_index, *pane_id))?;
            if let Some(runtime) = terminal_runtimes.get(terminal) {
                cwds.probes
                    .push(((workspace_index, *pane_id), runtime.cwd_probe()));
            }
        }
    }
    Some(cwds)
}

pub(super) fn capture_node(node: &Node) -> LayoutSnapshot {
    match node {
        Node::Pane(id) => LayoutSnapshot::Pane(id.raw()),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => LayoutSnapshot::Split {
            direction: match direction {
                Direction::Horizontal => DirectionSnapshot::Horizontal,
                Direction::Vertical => DirectionSnapshot::Vertical,
            },
            ratio: *ratio,
            first: Box::new(capture_node(first)),
            second: Box::new(capture_node(second)),
        },
    }
}

/// Parses one on-disk session file. The serde types are the whole schema, so
/// a missing key, a wrong type or an unknown field is a parse error here.
pub fn parse_session_file(
    content: &str,
) -> Result<SessionFile<SessionSnapshot>, serde_json::Error> {
    serde_json::from_str(content)
}

pub(super) fn parse_history_snapshot(
    content: &str,
) -> Result<SessionHistorySnapshot, serde_json::Error> {
    serde_json::from_str(content)
}

/// Both halves of a history capture in one call. Saves split them across the
/// event loop and the persister's thread instead.
#[cfg(test)]
pub fn capture_history(
    workspaces: &[Workspace],
    terminal_runtimes: &PaneRuntimeRegistry,
    carry: &mut HistoryCarry,
) -> SessionHistorySnapshot {
    capture_pending_history(workspaces, terminal_runtimes).resolve(carry)
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_and_restored_history_revisions_have_distinct_identities() {
        assert!(super::HistoryRevision::Live(0) != super::HistoryRevision::Restored(0));
        assert!(
            super::HistoryRevision::Live(u64::MAX) != super::HistoryRevision::Restored(u64::MAX)
        );
    }

    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use crate::pane::PaneRuntimeRegistry;
    use crate::terminal::TerminalState;
    use crate::workspace::Workspace;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Holder {
        #[serde(with = "super::path_bytes")]
        path: PathBuf,
    }

    #[test]
    fn snapshot_types_reject_wrong_version_during_deserialization() {
        for json in [
            r#"{"version":2,"workspaces":[],"active":null}"#,
            r#"{"version":2,"workspaces":[]}"#,
        ] {
            assert!(serde_json::from_str::<super::SessionSnapshot>(json).is_err());
            assert!(serde_json::from_str::<super::SessionHistorySnapshot>(json).is_err());
        }
    }

    #[test]
    fn snapshot_cwds_parse_relative_paths_for_restore_validation() {
        let relative = r#"{"cwd":"relative","public_number":1,"label":null}"#;
        assert!(serde_json::from_str::<super::PaneSnapshot>(relative).is_ok());
        // A missing saved path remains available for a later restore attempt.
        let missing = r#"{"cwd":"/shepr-missing-saved-directory","public_number":1,"label":null}"#;
        let pane: super::PaneSnapshot = serde_json::from_str(missing).expect("absolute saved cwd");
        assert_eq!(pane.cwd, PathBuf::from("/shepr-missing-saved-directory"));
    }

    /// Every key a save writes is required, a nullable one included, and a
    /// key no save writes is refused; a pane's agent session alone may be
    /// absent (a save leaves it out when there is none).
    #[test]
    fn every_saved_key_is_required_and_no_other_is_accepted() {
        let snapshot = super::SessionSnapshot {
            version: super::SNAPSHOT_VERSION,
            host_theme: super::SavedHostTheme::default(),
            workspaces: vec![super::WorkspaceSnapshot {
                id: "w1".parse().expect("id"),
                custom_name: None,
                next_public_pane_number: shepr_protocol::PanePublicNumber::new(2)
                    .expect("nonzero literal"),
                layout: super::LayoutSnapshot::Split {
                    direction: super::DirectionSnapshot::Horizontal,
                    ratio: shepr_core::layout::SplitRatio::new(0.5)
                        .expect("test split ratio is valid"),
                    first: Box::new(super::LayoutSnapshot::Pane(0)),
                    second: Box::new(super::LayoutSnapshot::Pane(1)),
                },
                panes: HashMap::from([(
                    0,
                    super::PaneSnapshot {
                        cwd: PathBuf::from("/"),
                        public_number: shepr_protocol::PanePublicNumber::new(1)
                            .expect("nonzero literal"),
                        label: None,
                        agent_session: None,
                    },
                )]),
                zoomed: false,
                focused: 0,
                root_pane: 0,
            }],
            active: None,
        };
        let saved = serde_json::to_value(super::SessionFile {
            snapshot: &snapshot,
            history_digest: None,
        })
        .expect("serialize");
        assert!(
            saved
                .pointer("/snapshot/workspaces/0/panes/0/agent_session")
                .is_none()
        );
        super::parse_session_file(&saved.to_string()).expect("a saved file parses");

        let workspace = "/snapshot/workspaces/0";
        let pane = "/snapshot/workspaces/0/panes/0";
        for (parent, key) in [
            ("", "history_digest"),
            ("/snapshot", "version"),
            ("/snapshot", "host_theme"),
            ("/snapshot", "workspaces"),
            ("/snapshot", "active"),
            ("/snapshot/host_theme", "foreground"),
            ("/snapshot/host_theme", "background"),
            ("/snapshot/host_theme", "palette"),
            (workspace, "id"),
            (workspace, "custom_name"),
            (workspace, "next_public_pane_number"),
            (workspace, "layout"),
            (workspace, "panes"),
            (workspace, "zoomed"),
            (workspace, "focused"),
            (workspace, "root_pane"),
            (pane, "cwd"),
            (pane, "public_number"),
            (pane, "label"),
        ] {
            let mut damaged = saved.clone();
            damaged
                .pointer_mut(parent)
                .and_then(serde_json::Value::as_object_mut)
                .and_then(|object| object.remove(key))
                .expect("test precondition");
            assert!(
                super::parse_session_file(&damaged.to_string()).is_err(),
                "{parent}/{key} missing"
            );
        }

        for (pointer, invalid) in [
            ("/snapshot/workspaces/0/id", serde_json::json!("ws_1")),
            ("/snapshot/workspaces/0/id", serde_json::json!(0)),
            (
                "/snapshot/workspaces/0/next_public_pane_number",
                serde_json::json!(0),
            ),
            (
                "/snapshot/workspaces/0/panes/0/public_number",
                serde_json::json!(0),
            ),
            (
                "/snapshot/workspaces/0/layout/Split/ratio",
                serde_json::json!(0.0),
            ),
            (
                "/snapshot/workspaces/0/layout/Split/ratio",
                serde_json::json!(1.0),
            ),
        ] {
            let mut damaged = saved.clone();
            *damaged.pointer_mut(pointer).expect("schema field") = invalid;
            assert!(
                super::parse_session_file(&damaged.to_string()).is_err(),
                "{pointer}"
            );
        }

        let mut unknown = saved.clone();
        unknown
            .pointer_mut(workspace)
            .and_then(serde_json::Value::as_object_mut)
            .expect("test precondition")
            .insert("identity_cwd".into(), "/".into());
        assert!(super::parse_session_file(&unknown.to_string()).is_err());

        let mut short_palette = saved;
        short_palette
            .pointer_mut("/snapshot/host_theme/palette")
            .and_then(serde_json::Value::as_array_mut)
            .expect("test precondition")
            .pop();
        assert!(super::parse_session_file(&short_palette.to_string()).is_err());
    }

    #[test]
    fn invalid_saved_agent_sessions_do_not_reject_the_pane() {
        for session in [
            serde_json::json!({"source": "shepr:codex", "agent": "removed-agent", "session_ref": {"id": "session"}}),
            serde_json::json!({"source": "invalid source", "agent": "codex", "session_ref": {"id": "session"}}),
            serde_json::json!(42),
        ] {
            let pane: super::PaneSnapshot = serde_json::from_value(serde_json::json!({
                "cwd": "/", "public_number": 1, "label": null, "agent_session": session,
            }))
            .expect("bad session stays local to this pane");
            assert!(pane.agent_session.is_none());
        }
    }

    #[test]
    fn history_panes_serialize_in_numeric_id_order() {
        let snapshot = super::WorkspaceHistorySnapshot {
            panes: HashMap::from([
                (
                    12,
                    super::PaneHistorySnapshot {
                        ansi: "twelve".into(),
                    },
                ),
                (2, super::PaneHistorySnapshot { ansi: "two".into() }),
                (
                    9,
                    super::PaneHistorySnapshot {
                        ansi: "nine".into(),
                    },
                ),
            ]),
        };

        assert_eq!(
            serde_json::to_string(&snapshot).expect("serialize"),
            r#"{"panes":{"2":{"ansi":"two"},"9":{"ansi":"nine"},"12":{"ansi":"twelve"}}}"#
        );
    }

    #[test]
    fn utf8_paths_stay_strings_and_other_paths_round_trip_as_bytes() {
        let readable = Holder {
            path: PathBuf::from("/home/user/project"),
        };
        let json = serde_json::to_string(&readable).expect("test precondition");
        assert_eq!(json, r#"{"path":"/home/user/project"}"#);
        assert_eq!(
            serde_json::from_str::<Holder>(&json).expect("utf-8 path parses"),
            readable
        );

        let raw = Holder {
            path: PathBuf::from(OsString::from_vec(b"/tmp/caf\xe9".to_vec())),
        };
        let json = serde_json::to_string(&raw).expect("non-UTF-8 path serializes");
        assert!(json.contains('['), "{json}");
        assert_eq!(
            serde_json::from_str::<Holder>(&json).expect("byte path parses"),
            raw
        );
    }

    #[tokio::test]
    async fn history_with_the_saved_content_is_recognised_without_assembling() {
        let workspaces = [Workspace::test_new("history-unchanged")];
        let pane_id = workspaces[0].root_pane();
        let terminal_id = workspaces[0]
            .terminal_id(pane_id)
            .expect("test terminal")
            .clone();
        let mut runtimes = PaneRuntimeRegistry::new();
        runtimes.insert(
            terminal_id.clone(),
            crate::pane::PaneRuntime::test_with_scrollback_bytes(20, 3, 4096, b"ONE\r\n"),
        );
        let mut carry = super::HistoryCarry::default();
        let first = super::super::io::history_digest(b"first");
        let second = super::super::io::history_digest(b"second");
        let third = super::super::io::history_digest(b"third");
        let resolve = |carry: &mut super::HistoryCarry, allow: bool| {
            super::capture_pending_history(&workspaces, &runtimes).resolve_for_save(carry, allow)
        };

        assert!(matches!(
            resolve(&mut carry, true),
            super::ResolvedHistory::Changed(_)
        ));
        carry.note_saved(Some(first));
        assert!(matches!(
            resolve(&mut carry, true),
            super::ResolvedHistory::Unchanged(digest) if digest == first
        ));
        // An unchanged save keeps what it can skip against.
        carry.note_saved(Some(first));
        assert!(
            matches!(
                resolve(&mut carry, false),
                super::ResolvedHistory::Changed(_)
            ),
            "a caller that cannot skip always gets the history"
        );
        carry.note_saved(Some(second));
        assert!(matches!(
            resolve(&mut carry, true),
            super::ResolvedHistory::Unchanged(digest) if digest == second
        ));
        carry.note_saved(None);
        assert!(
            matches!(
                resolve(&mut carry, true),
                super::ResolvedHistory::Changed(_)
            ),
            "a save that wrote no history leaves nothing to skip against"
        );
        carry.note_saved(Some(third));

        runtimes
            .get(&terminal_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"TWO\r\n");
        assert!(matches!(
            resolve(&mut carry, true),
            super::ResolvedHistory::Changed(_)
        ));
        carry.forget_saved();
        assert!(
            matches!(
                resolve(&mut carry, true),
                super::ResolvedHistory::Changed(_)
            ),
            "a save that failed leaves nothing to skip against"
        );
    }

    #[test]
    fn invalid_live_hook_authority_falls_back_to_persisted_agent_session() {
        use shepr_agent::resume::AgentSessionRef;
        for (source, label, live_ref) in [
            (
                "shepr:codex",
                "codex",
                AgentSessionRef::path("/codex-session").expect("test session ref"),
            ),
            (
                "shepr:claude",
                "claude",
                AgentSessionRef::path("/session.jsonl").expect("test session ref"),
            ),
        ] {
            let workspace = Workspace::test_new("snapshot-session-fallback");
            let pane_id = workspace.root_pane();
            let terminal_id = workspace
                .terminal_id(pane_id)
                .expect("test terminal")
                .clone();
            let saved_ref = shepr_agent::resume::AgentSessionRef::id("saved-session")
                .expect("test session ref");
            let mut terminal = TerminalState::new(terminal_id.clone(), PathBuf::from("/"));
            terminal.seed_hook_authority_for_test(Some(crate::terminal::state::HookAuthority {
                origin: shepr_agent::ReportOrigin::parse(source, label).expect("test origin"),
                state: shepr_agent::AgentState::Working,
                reported_at: std::time::Instant::now(),
                session_ref: Some(live_ref),
            }));
            let expected = shepr_agent::resume::PersistedAgentSession::new(
                shepr_agent::AgentSource::new(shepr_agent::IntegrationTarget::Claude),
                shepr_agent::Agent::Claude,
                saved_ref.clone(),
            )
            .expect("test session is valid");
            terminal
                .ownership_mut()
                .set_persisted_agent_session(expected.clone());
            let terminals = HashMap::from([(terminal_id, terminal)]);

            let snapshot = super::capture(
                &[workspace],
                &terminals,
                &PaneRuntimeRegistry::new(),
                PathBuf::from("/").as_path(),
                None,
                Default::default(),
            );

            let saved = snapshot.workspaces[0]
                .panes
                .values()
                .next()
                .expect("saved pane")
                .agent_session
                .as_ref();
            assert_eq!(saved, Some(&expected));
        }
    }
}
