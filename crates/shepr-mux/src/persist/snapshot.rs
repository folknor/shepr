use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::pane::{HistoryPiece, PaneRuntimeRegistry};
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

/// Serializable snapshot of the entire shepr session.
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Format version - used to detect incompatible changes.
    pub version: SnapshotVersion,
    #[serde(default)]
    pub host_theme: SavedHostTheme,
    pub workspaces: Vec<WorkspaceSnapshot>,
    /// The workspace the session's bookmark names: where a client with no
    /// location of its own starts.
    pub active: Option<usize>,
}

/// Last observed physical terminal colours, retained for headless resumes.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SavedHostTheme {
    pub foreground: Option<shepr_termio::host_term::theme::RgbColor>,
    pub background: Option<shepr_termio::host_term::theme::RgbColor>,
    #[serde(default)]
    pub palette: Vec<Option<shepr_termio::host_term::theme::RgbColor>>,
}

impl From<shepr_termio::host_term::theme::TerminalTheme> for SavedHostTheme {
    fn from(theme: shepr_termio::host_term::theme::TerminalTheme) -> Self {
        Self {
            foreground: theme.foreground,
            background: theme.background,
            palette: theme.palette.into(),
        }
    }
}

impl SavedHostTheme {
    pub fn to_theme(&self) -> shepr_termio::host_term::theme::TerminalTheme {
        let mut theme = shepr_termio::host_term::theme::TerminalTheme {
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

#[derive(Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(
        serialize_with = "path_bytes::serialize",
        deserialize_with = "path_bytes::deserialize"
    )]
    pub identity_cwd: PathBuf,
    #[serde(default)]
    pub next_public_pane_number: usize,
    pub layout: LayoutSnapshot,
    pub panes: HashMap<u32, PaneSnapshot>,
    pub zoomed: bool,
    #[serde(default)]
    pub focused: Option<u32>,
    #[serde(default)]
    pub root_pane: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PaneSnapshot {
    #[serde(
        serialize_with = "path_bytes::serialize",
        deserialize_with = "path_bytes::deserialize"
    )]
    pub cwd: PathBuf,
    /// The pane's public number within its workspace. Restore gives a pane
    /// with none, or with zero (which no public ID can carry), a fresh free
    /// number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_number: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_agent_session"
    )]
    pub agent_session: Option<PaneAgentSessionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAgentSessionSnapshot {
    pub source: shepr_agent::agent::AgentSource,
    pub agent: shepr_agent::agent::Agent,
    pub session_ref: shepr_agent::agent::resume::AgentSessionRef,
}

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
#[expect(
    variant_size_differences,
    reason = "a split is 23 bytes; boxing it would allocate per split to save that much per leaf"
)]
pub enum LayoutSnapshot {
    Pane(u32),
    Split {
        direction: DirectionSnapshot,
        ratio: f32,
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub enum DirectionSnapshot {
    Horizontal,
    Vertical,
}

/// Where a pane sits in a snapshot: workspace index, pane number.
type PaneKey = (usize, u32);

/// The live cwd probe reads a capture left for whoever writes the snapshot.
/// Reading a cwd is a /proc access per pane, which the event loop should not
/// pay per save, so a capture records the best cwd it knows without one and
/// hands over a [`PaneCwdProbe`](crate::pane::PaneCwdProbe) per runtime; [`resolve`]
/// applies the same OSC 7 and /proc arbitration used by live panes.
///
/// [`resolve`]: Self::resolve
#[derive(Default)]
pub struct PendingCwds {
    probes: Vec<(PaneKey, crate::pane::PaneCwdProbe)>,
}

impl PendingCwds {
    /// Reads every probe and stores the best result in `snapshot`, then updates
    /// each affected workspace's identity cwd from its root pane.
    pub fn resolve(self, snapshot: &mut SessionSnapshot) {
        let mut touched = Vec::new();
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
                touched.push(workspace);
            }
        }
        for workspace in touched {
            if let Some(workspace) = snapshot.workspaces.get_mut(workspace)
                && let Some(cwd) = root_pane_cwd(workspace)
            {
                workspace.identity_cwd = cwd;
            }
        }
    }
}

/// The cwd of a workspace's root pane, which names the workspace.
fn root_pane_cwd(workspace: &WorkspaceSnapshot) -> Option<PathBuf> {
    workspace
        .root_pane
        .and_then(|id| workspace.panes.get(&id))
        .map(|pane| pane.cwd.clone())
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
    host_theme: shepr_termio::host_term::theme::TerminalTheme,
) -> SessionSnapshot {
    let (mut snapshot, cwds) = capture_deferred(
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
/// snapshot holds each pane's best known cwd, and the returned [`PendingCwds`]
/// refreshes it where the snapshot is written.
pub fn capture_deferred(
    workspaces: &[Workspace],
    terminals: &std::collections::HashMap<
        shepr_protocol::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
    active: Option<usize>,
    host_theme: shepr_termio::host_term::theme::TerminalTheme,
) -> (SessionSnapshot, PendingCwds) {
    let mut cwds = PendingCwds::default();
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
                )
            })
            .collect(),
        active,
    };
    (snapshot, cwds)
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
) -> WorkspaceSnapshot {
    let mut panes = HashMap::new();
    for (id, workspace_pane) in &ws.panes {
        let terminal_id = ws.terminal_id(*id);
        let terminal = terminal_id.and_then(|id| terminals.get(id));
        let runtime = terminal_id.and_then(|id| terminal_runtimes.get(id));
        let cwd = runtime
            .and_then(crate::pane::PaneRuntime::remembered_cwd)
            .or_else(|| terminal.map(|terminal| terminal.cwd().to_path_buf()))
            .unwrap_or_else(|| fallback_cwd.to_path_buf());
        if let Some(runtime) = runtime {
            cwds.probes
                .push(((workspace_index, id.raw()), runtime.cwd_probe()));
        }
        let label = terminal.and_then(|terminal| terminal.manual_label.clone());
        let agent_session = terminal
            .and_then(crate::terminal::TerminalState::current_session_identity_for_persistence)
            .map(|session| PaneAgentSessionSnapshot {
                source: session.source,
                agent: session.agent,
                session_ref: session.session_ref,
            });
        panes.insert(
            id.raw(),
            PaneSnapshot {
                cwd,
                public_number: Some(workspace_pane.public_number),
                label,
                agent_session,
            },
        );
    }
    let identity_cwd = panes
        .get(&ws.root_pane.raw())
        .map_or_else(|| ws.identity_cwd.clone(), |pane| pane.cwd.clone());
    WorkspaceSnapshot {
        id: Some(ws.id.to_string()),
        custom_name: ws.custom_name.clone(),
        identity_cwd,
        next_public_pane_number: ws.next_public_pane_number,
        layout: capture_node(ws.layout.root()),
        panes,
        zoomed: ws.zoomed,
        focused: Some(ws.layout.focused().raw()),
        root_pane: Some(ws.root_pane.raw()),
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
            if !ratio.is_finite() {
                return None;
            }
            encoding.push(1);
            encoding.push(match direction {
                DirectionSnapshot::Horizontal => 0,
                DirectionSnapshot::Vertical => 1,
            });
            encoding.extend_from_slice(&ratio.to_bits().to_le_bytes());
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
    revision: u64,
}

/// Names for restored text. A `PaneHistoryCache` numbers its text from a
/// counter of its own that never reaches the top bit, so a restored name never
/// equals a live one.
fn next_restored_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    (1 << 63) | NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// What a save's history holds for one pane, by content identity rather than
/// content: two saves with equal stamps hold equal text.
type PaneStamp = Option<u64>;

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
    Unchanged(String),
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
    pub(super) fn note_saved(&mut self, digest: Option<String>) {
        let resolved = self.resolved.take();
        self.saved =
            resolved.zip(digest.and_then(|digest| super::io::HistoryDigest::from_hex(&digest)));
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
            .map(crate::pane::PaneHistoryCache::revision)
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
        source.refresh(cache);
        cache.has_text().then(|| cache.revision())
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
            let digest = digest.to_hex();
            carry.resolved = Some(stamp);
            return ResolvedHistory::Unchanged(digest);
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
        Some(runtime) => PendingPaneHistory::Live(terminal, runtime.history_source()),
        None => PendingPaneHistory::Runtimeless(terminal),
    }
}

/// Captures fresh history handles for a previously captured session layout.
/// Panes removed since that layout was saved use the persister's carried
/// history, while panes that still have runtimes contribute their current
/// history. The terminal map must have been captured with `snapshot`.
pub fn capture_pending_history_for_snapshot(
    snapshot: &SessionSnapshot,
    terminal_ids: &HashMap<(usize, u32), TerminalId>,
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
    terminal_ids: &HashMap<(usize, u32), TerminalId>,
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
            ratio: ratio.get(),
            first: Box::new(capture_node(first)),
            second: Box::new(capture_node(second)),
        },
    }
}

/// Deserializes the saved shape only. Semantic checks stay in `restore`, so
/// one invalid workspace can be dropped while healthy ones survive and the caller
/// can back up the original session file before its next save.
pub fn parse_snapshot(content: &str) -> Result<SessionSnapshot, String> {
    serde_json::from_str(content).map_err(|e| e.to_string())
}

pub(super) fn parse_history_snapshot(content: &str) -> Result<SessionHistorySnapshot, String> {
    serde_json::from_str(content).map_err(|e| e.to_string())
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
        let relative = r#"{"cwd":"relative"}"#;
        assert!(serde_json::from_str::<super::PaneSnapshot>(relative).is_ok());
        let missing = r#"{"cwd":"/shepr-missing-saved-directory"}"#;
        // The rest of the pane fields default, so this also checks that a
        // missing saved path remains available for a later restore attempt.
        let pane: super::PaneSnapshot = serde_json::from_str(missing).expect("absolute saved cwd");
        assert_eq!(pane.cwd, PathBuf::from("/shepr-missing-saved-directory"));
    }

    #[test]
    fn invalid_saved_agent_sessions_do_not_reject_the_pane() {
        for session in [
            serde_json::json!({"source": "shepr:codex", "agent": "removed-agent", "session_ref": {"id": "session"}}),
            serde_json::json!({"source": "invalid source", "agent": "codex", "session_ref": {"id": "session"}}),
            serde_json::json!(42),
        ] {
            let pane: super::PaneSnapshot = serde_json::from_value(serde_json::json!({
                "cwd": "/", "agent_session": session,
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
        let first = super::super::io::history_digest(b"first").to_hex();
        let second = super::super::io::history_digest(b"second").to_hex();
        let third = super::super::io::history_digest(b"third").to_hex();
        let resolve = |carry: &mut super::HistoryCarry, allow: bool| {
            super::capture_pending_history(&workspaces, &runtimes).resolve_for_save(carry, allow)
        };

        assert!(matches!(
            resolve(&mut carry, true),
            super::ResolvedHistory::Changed(_)
        ));
        carry.note_saved(Some(first.clone()));
        assert!(matches!(
            resolve(&mut carry, true),
            super::ResolvedHistory::Unchanged(digest) if digest == first
        ));
        // An unchanged save keeps what it can skip against.
        carry.note_saved(Some(first.clone()));
        assert!(
            matches!(
                resolve(&mut carry, false),
                super::ResolvedHistory::Changed(_)
            ),
            "a caller that cannot skip always gets the history"
        );
        carry.note_saved(Some(second.clone()));
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
        use shepr_agent::agent::resume::AgentSessionRef;
        for (source, label, live_ref) in [
            (
                "unrecognised-source",
                "unrecognised-agent",
                AgentSessionRef::id("live-session").expect("test session ref"),
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
            let saved_ref = shepr_agent::agent::resume::AgentSessionRef::id("saved-session")
                .expect("test session ref");
            let mut terminal = TerminalState::new(terminal_id.clone(), PathBuf::from("/"));
            terminal.seed_hook_authority_for_test(Some(crate::terminal::state::HookAuthority {
                origin: shepr_agent::agent::ReportOrigin::parse(source, label)
                    .expect("test origin"),
                state: shepr_agent::detect::AgentState::Working,
                reported_at: std::time::Instant::now(),
                session_ref: Some(live_ref),
            }));
            terminal.set_persisted_agent_session(
                shepr_agent::agent::resume::PersistedAgentSession {
                    source: shepr_agent::agent::AgentSource::Official(
                        shepr_agent::agent::IntegrationTarget::Claude,
                    ),
                    agent: shepr_agent::agent::Agent::Claude,
                    session_ref: saved_ref.clone(),
                },
            );
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
            assert_eq!(
                saved,
                Some(&super::PaneAgentSessionSnapshot {
                    source: shepr_agent::agent::AgentSource::Official(
                        shepr_agent::agent::IntegrationTarget::Claude,
                    ),
                    agent: shepr_agent::agent::Agent::Claude,
                    session_ref: saved_ref,
                })
            );
        }
    }
}
