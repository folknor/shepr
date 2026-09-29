use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::pane::PaneRuntimeRegistry;
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

    pub(crate) fn deserialize_saved_cwd<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path = deserialize(deserializer)?;
        // A saved directory may have disappeared while shepr was stopped.
        // Restore retains it so a later restart can retry the original path.
        if !path.is_absolute() {
            return Err(serde::de::Error::custom("saved cwd must be absolute"));
        }
        Ok(path)
    }
}

/// Serializable snapshot of the entire shepr session.
#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Format version - used to detect incompatible changes.
    pub version: SnapshotVersion,
    #[serde(default)]
    pub host_theme: SavedHostTheme,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub active: Option<usize>,
    pub selected: usize,
}

/// Last observed physical terminal colours, retained for headless resumes.
#[derive(Default, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_fingerprint: Option<String>,
    pub workspaces: Vec<WorkspaceHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceHistorySnapshot {
    pub tabs: Vec<TabHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct TabHistorySnapshot {
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

#[derive(Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(
        serialize_with = "path_bytes::serialize",
        deserialize_with = "path_bytes::deserialize_saved_cwd"
    )]
    pub identity_cwd: PathBuf,
    /// Captured from the public numbers in each tab's pane records.
    #[serde(default)]
    pub public_pane_numbers: HashMap<u32, usize>,
    #[serde(default)]
    pub next_public_pane_number: usize,
    #[serde(default)]
    pub public_tab_numbers: Vec<usize>,
    #[serde(default)]
    pub next_public_tab_number: usize,
    pub tabs: Vec<TabSnapshot>,
    #[serde(default)]
    pub active_tab: usize,
}

#[derive(Serialize, Deserialize)]
pub struct TabSnapshot {
    #[serde(default)]
    pub custom_name: Option<String>,
    pub layout: LayoutSnapshot,
    pub panes: HashMap<u32, PaneSnapshot>,
    pub zoomed: bool,
    #[serde(default)]
    pub focused: Option<u32>,
    #[serde(default)]
    pub root_pane: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct PaneSnapshot {
    #[serde(
        serialize_with = "path_bytes::serialize",
        deserialize_with = "path_bytes::deserialize_saved_cwd"
    )]
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<PaneAgentSessionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAgentSessionSnapshot {
    pub source: shepr_agent::agent::AgentSource,
    pub agent: shepr_agent::agent::Agent,
    pub session_ref: shepr_agent::agent::resume::AgentSessionRef,
}

/// Saved screen history of one pane.
#[derive(Serialize, Deserialize)]
pub struct PaneHistorySnapshot {
    pub ansi: String,
}

/// Serializable BSP tree.
#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
pub enum DirectionSnapshot {
    Horizontal,
    Vertical,
}

/// Capture the current app state into a serializable snapshot.
pub fn capture(
    workspaces: &[Workspace],
    terminals: &std::collections::HashMap<
        shepr_protocol::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
    active: Option<usize>,
    selected: usize,
    host_theme: shepr_termio::host_term::theme::TerminalTheme,
) -> SessionSnapshot {
    SessionSnapshot {
        version: SNAPSHOT_VERSION,
        host_theme: host_theme.into(),
        workspaces: workspaces
            .iter()
            .map(|workspace| {
                capture_workspace(workspace, terminals, terminal_runtimes, fallback_cwd)
            })
            .collect(),
        active,
        selected,
    }
}

fn capture_workspace(
    ws: &Workspace,
    terminals: &std::collections::HashMap<
        shepr_protocol::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
) -> WorkspaceSnapshot {
    let tabs: Vec<_> = ws
        .tabs()
        .iter()
        .map(|tab| capture_tab(tab, terminals, terminal_runtimes, fallback_cwd))
        .collect();
    let identity_cwd = tabs
        .first()
        .and_then(|tab| tab.root_pane.and_then(|id| tab.panes.get(&id)))
        .map_or_else(|| ws.identity_cwd.clone(), |pane| pane.cwd.clone());
    WorkspaceSnapshot {
        id: Some(ws.id.to_string()),
        custom_name: ws.custom_name.clone(),
        identity_cwd,
        public_pane_numbers: ws
            .tabs()
            .iter()
            .flat_map(|tab| {
                tab.panes
                    .iter()
                    .map(|(pane_id, pane)| (pane_id.raw(), pane.public_number))
            })
            .collect(),
        next_public_pane_number: ws.next_public_pane_number,
        public_tab_numbers: ws.tabs().iter().map(|tab| tab.number).collect(),
        next_public_tab_number: ws.next_public_tab_number,
        tabs,
        active_tab: ws.active_tab_index(),
    }
}

fn capture_tab(
    tab: &crate::workspace::Tab,
    terminals: &std::collections::HashMap<
        shepr_protocol::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &std::path::Path,
) -> TabSnapshot {
    let mut panes = HashMap::new();
    for id in tab.panes.keys() {
        let terminal_id = tab.terminal_id(*id);
        let terminal = terminal_id.and_then(|id| terminals.get(id));
        let cwd = terminal_id
            .and_then(|id| terminal_runtimes.get(id))
            .and_then(crate::pane::PaneRuntime::cwd_for_persistence)
            .or_else(|| terminal.map(|terminal| terminal.cwd().to_path_buf()))
            .unwrap_or_else(|| fallback_cwd.to_path_buf());
        let label = terminal.and_then(|terminal| terminal.manual_label.clone());
        let agent_session = terminal.and_then(|terminal| {
            let hook_session = terminal.hook_authority.as_ref().and_then(|authority| {
                let session_ref = authority.session_ref.as_ref()?;
                Some(PaneAgentSessionSnapshot {
                    source: shepr_agent::agent::AgentSource::from_pair(
                        &authority.source,
                        &authority.agent_label,
                    )?,
                    agent: shepr_agent::agent::Agent::parse_canonical_label(
                        &authority.agent_label,
                    )?,
                    session_ref: session_ref.clone(),
                })
            });
            hook_session.or_else(|| {
                terminal
                    .persisted_agent_session
                    .as_ref()
                    .map(|session| PaneAgentSessionSnapshot {
                        source: session.source.clone(),
                        agent: session.agent,
                        session_ref: session.session_ref.clone(),
                    })
            })
        });
        panes.insert(
            id.raw(),
            PaneSnapshot {
                cwd,
                label,
                agent_session,
            },
        );
    }
    TabSnapshot {
        custom_name: tab.custom_name.clone(),
        layout: capture_node(tab.layout.root()),
        panes,
        zoomed: tab.zoomed,
        focused: Some(tab.layout.focused().raw()),
        root_pane: Some(tab.root_pane.raw()),
    }
}

pub(super) fn layout_fingerprint(snapshot: &SessionSnapshot) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    #[derive(Serialize)]
    struct WorkspaceLayout<'a> {
        tabs: Vec<TabLayout<'a>>,
    }

    #[derive(Serialize)]
    struct TabLayout<'a> {
        layout: &'a LayoutSnapshot,
        pane_ids: Vec<u32>,
    }

    let workspaces: Vec<_> = snapshot
        .workspaces
        .iter()
        .map(|workspace| WorkspaceLayout {
            tabs: workspace
                .tabs
                .iter()
                .map(|tab| {
                    let mut pane_ids: Vec<_> = tab.panes.keys().copied().collect();
                    pane_ids.sort_unstable();
                    TabLayout {
                        layout: &tab.layout,
                        pane_ids,
                    }
                })
                .collect(),
        })
        .collect();
    // This projection contains no maps, and pane IDs are sorted explicitly.
    // Cwd, names, agent state, theme, and current selections do not identify
    // which saved screen history belongs to each pane.
    let bytes = serde_json::to_vec(&workspaces).ok()?;
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(hex, "{byte:02x}").ok()?;
    }
    Some(hex)
}

/// One pane's history kept across saves; see `HistoryCarry`.
struct CarriedEntry {
    ansi: String,
    /// Where `ansi` came from. `Restored`: the history file loaded at
    /// startup, for a pane that has not run yet. `Live`: this pane's own
    /// runtime, on its last successful primary-screen read.
    origin: CarriedOrigin,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CarriedOrigin {
    Restored,
    Live,
}

/// Pane history kept from one save to the next. Restore creates it and hands
/// it to the session persister, which owns it from then on: every history
/// capture is resolved against it, on the persister's thread, in save order,
/// so nothing else touches it and it needs no lock.
///
/// It keeps two things per pane, keyed by terminal ID:
///
/// - What a pane's live screen cannot supply (`carried`):
///   - A restored pane without a runtime (deferred agent resume, failed
///     restore) keeps its `Restored` history from the loaded file until it
///     runs. Capture reads live runtimes only, so without this a save made
///     before the pane runs would lose its saved screen for good. The first
///     save that sees a runtime for the pane drops that entry: from then on
///     the pane's own screen supersedes it, even if its first read happens
///     on the alternate screen.
///   - A running pane on the alternate screen (vim, an agent TUI) cannot
///     have its primary screen read, so saves fall back to its `Live` entry,
///     the last primary history read successfully. Every successful read
///     replaces it and an empty read removes it, so it never holds anything
///     older than the pane's own last primary screen.
/// - The history a live pane's reader formatted before (`readers`), so a
///   save formats only the lines that are new since the last one
///   (`PaneHistoryCache`).
///
/// Each save drops the entries of panes no longer in its layout. That pruning
/// is why this belongs to one app's persister instead of being process-wide:
/// a save only knows its own layout, and would drop every other owner's
/// entries.
#[derive(Default)]
pub struct HistoryCarry {
    carried: HashMap<TerminalId, CarriedEntry>,
    readers: HashMap<TerminalId, crate::pane::PaneHistoryCache>,
}

impl HistoryCarry {
    /// Keeps a restored pane's saved history for later saves until the pane
    /// has a runtime of its own.
    pub fn carry_restored(&mut self, terminal: &TerminalId, history: Option<&PaneHistorySnapshot>) {
        if let Some(history) = history {
            self.carried.insert(
                terminal.clone(),
                CarriedEntry {
                    ansi: history.ansi.clone(),
                    origin: CarriedOrigin::Restored,
                },
            );
        }
    }

    /// Forgets every pane: a cleared session has none.
    pub(super) fn clear(&mut self) {
        self.carried.clear();
        self.readers.clear();
    }

    /// Drops what belongs to panes outside `panes` (the ones a save saw) and
    /// the readers of panes that no longer have a runtime.
    fn retain(&mut self, panes: &HashMap<TerminalId, bool>) {
        self.carried.retain(|id, _| panes.contains_key(id));
        self.readers
            .retain(|id, _| panes.get(id).copied().unwrap_or(false));
    }

    /// History carried for a pane without a runtime.
    fn carried(&self, terminal: &TerminalId) -> Option<String> {
        self.carried.get(terminal).map(|entry| entry.ansi.clone())
    }

    /// A live pane's history: reads it through its cache, records a
    /// successful primary-screen read as the pane's fallback, or falls back to
    /// the last one while the alternate screen hides the primary screen.
    fn resolve_live(
        &mut self,
        terminal: &TerminalId,
        source: &crate::pane::PaneHistorySource,
    ) -> Option<String> {
        // The pane has a runtime of its own now: its own screen supersedes
        // the history restored for it, permanently, even while that screen is
        // on the alternate buffer and cannot be read.
        if self
            .carried
            .get(terminal)
            .is_some_and(|entry| entry.origin == CarriedOrigin::Restored)
        {
            self.carried.remove(terminal);
        }
        let read = source.read(self.readers.entry(terminal.clone()).or_default());
        match read {
            Some(ansi) if ansi.trim().is_empty() => {
                self.carried.remove(terminal);
                None
            }
            Some(ansi) => {
                self.carried.insert(
                    terminal.clone(),
                    CarriedEntry {
                        ansi: ansi.clone(),
                        origin: CarriedOrigin::Live,
                    },
                );
                Some(ansi)
            }
            None => self
                .carried
                .get(terminal)
                .filter(|entry| entry.origin == CarriedOrigin::Live)
                .map(|entry| entry.ansi.clone()),
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
/// workspaces and tabs it was captured from.
pub struct PendingHistory {
    workspaces: Vec<Vec<Vec<(u32, PendingPaneHistory)>>>,
}

impl PendingHistory {
    /// Formats every live pane's history and pairs the result with the layout
    /// it was captured alongside. Meant for the persister's thread: it can
    /// take as long as formatting what is new in every pane's scrollback
    /// does, in bounded chunks per hold of each pane's terminal lock.
    pub fn resolve(
        self,
        snapshot: &SessionSnapshot,
        carry: &mut HistoryCarry,
    ) -> SessionHistorySnapshot {
        let panes: HashMap<TerminalId, bool> = self
            .workspaces
            .iter()
            .flatten()
            .flatten()
            .map(|(_, pending)| match pending {
                PendingPaneHistory::Runtimeless(terminal) => (terminal.clone(), false),
                PendingPaneHistory::Live(terminal, _) => (terminal.clone(), true),
            })
            .collect();
        carry.retain(&panes);
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            // Pair history to this saved layout here: live pane IDs are stable
            // across saves, while restore allocates fresh IDs and carries the
            // saved history through the ID remap.
            layout_fingerprint: layout_fingerprint(snapshot),
            workspaces: self
                .workspaces
                .into_iter()
                .map(|tabs| WorkspaceHistorySnapshot {
                    tabs: tabs
                        .into_iter()
                        .map(|panes| TabHistorySnapshot {
                            panes: panes
                                .into_iter()
                                .filter_map(|(id, pending)| {
                                    let ansi = match pending {
                                        PendingPaneHistory::Runtimeless(terminal) => {
                                            carry.carried(&terminal)
                                        }
                                        PendingPaneHistory::Live(terminal, source) => {
                                            carry.resolve_live(&terminal, &source)
                                        }
                                    }?;
                                    Some((id, PaneHistorySnapshot { ansi }))
                                })
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
        }
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
                    .tabs()
                    .iter()
                    .map(|tab| {
                        tab.panes
                            .iter()
                            .map(|(id, pane)| {
                                let terminal = pane.attached_terminal_id.clone();
                                let pending = match terminal_runtimes.get(&terminal) {
                                    Some(runtime) => {
                                        PendingPaneHistory::Live(terminal, runtime.history_source())
                                    }
                                    None => PendingPaneHistory::Runtimeless(terminal),
                                };
                                (id.raw(), pending)
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect(),
    }
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
/// one invalid tab can be dropped while healthy tabs survive and the caller
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
    snapshot: &SessionSnapshot,
    workspaces: &[Workspace],
    terminal_runtimes: &PaneRuntimeRegistry,
    carry: &mut HistoryCarry,
) -> SessionHistorySnapshot {
    capture_pending_history(workspaces, terminal_runtimes).resolve(snapshot, carry)
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
            r#"{"version":2,"workspaces":[],"active":null,"selected":0}"#,
            r#"{"version":2,"workspaces":[]}"#,
        ] {
            assert!(serde_json::from_str::<super::SessionSnapshot>(json).is_err());
            assert!(serde_json::from_str::<super::SessionHistorySnapshot>(json).is_err());
        }
    }

    #[test]
    fn snapshot_cwds_reject_relative_paths_but_retain_missing_absolute_paths() {
        let relative = r#"{"cwd":"relative"}"#;
        assert!(serde_json::from_str::<super::PaneSnapshot>(relative).is_err());
        let missing = r#"{"cwd":"/shepr-missing-saved-directory"}"#;
        // The rest of the pane fields default, so this also checks that a
        // missing saved path remains available for a later restore attempt.
        let pane: super::PaneSnapshot = serde_json::from_str(missing).expect("absolute saved cwd");
        assert_eq!(pane.cwd, PathBuf::from("/shepr-missing-saved-directory"));
    }

    #[test]
    fn history_panes_serialize_in_numeric_id_order() {
        let snapshot = super::TabHistorySnapshot {
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

    #[test]
    fn invalid_live_hook_authority_falls_back_to_persisted_agent_session() {
        let workspace = Workspace::test_new("snapshot-session-fallback");
        let tab = workspace.tabs().first().expect("test tab");
        let pane_id = *tab.panes.keys().next().expect("test pane");
        let terminal_id = tab.terminal_id(pane_id).expect("test terminal").clone();
        let saved_ref = shepr_agent::agent::resume::AgentSessionRef::id("saved-session")
            .expect("test session ref");
        let mut terminal = TerminalState::new(terminal_id.clone(), PathBuf::from("/"));
        terminal.hook_authority = Some(crate::terminal::state::HookAuthority {
            source: "unrecognised-source".into(),
            agent_label: "unrecognised-agent".into(),
            state: shepr_agent::detect::AgentState::Working,
            message: None,
            reported_at: std::time::Instant::now(),
            session_ref: Some(
                shepr_agent::agent::resume::AgentSessionRef::id("live-session")
                    .expect("test session ref"),
            ),
        });
        terminal.persisted_agent_session =
            Some(shepr_agent::agent::resume::PersistedAgentSession {
                source: shepr_agent::agent::AgentSource::Official(
                    shepr_agent::agent::Agent::Claude,
                ),
                agent: shepr_agent::agent::Agent::Claude,
                session_ref: saved_ref.clone(),
            });
        let terminals = HashMap::from([(terminal_id, terminal)]);

        let snapshot = super::capture(
            &[workspace],
            &terminals,
            &PaneRuntimeRegistry::new(),
            PathBuf::from("/").as_path(),
            None,
            0,
            Default::default(),
        );

        let saved = snapshot.workspaces[0].tabs[0]
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
                    shepr_agent::agent::Agent::Claude,
                ),
                agent: shepr_agent::agent::Agent::Claude,
                session_ref: saved_ref,
            })
        );
    }
}
