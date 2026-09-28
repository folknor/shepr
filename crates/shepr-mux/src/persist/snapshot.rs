use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::layout::Direction;
use serde::{Deserialize, Serialize};

use crate::pane::PaneRuntimeRegistry;
use crate::workspace::Workspace;
use shepr_core::layout::Node;
use shepr_protocol::TerminalId;

/// Current snapshot format version. Files with any other version are ignored.
pub const SNAPSHOT_VERSION: u32 = 1;

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
#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Format version - used to detect incompatible changes.
    pub version: u32,
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
        for (index, color) in self.palette.iter().take(256).enumerate() {
            theme.palette[index] = *color;
        }
        theme
    }
}

#[derive(Serialize, Deserialize)]
pub struct SessionHistorySnapshot {
    /// Format version follows the matching session snapshot version.
    pub version: u32,
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
    pub panes: HashMap<u32, PaneHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(with = "path_bytes")]
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
    #[serde(with = "path_bytes")]
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_agent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<PaneAgentSessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_argv: Option<Vec<String>>,
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
        active_tab: ws.active_tab,
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
            .or_else(|| terminal.map(|terminal| terminal.cwd.clone()))
            .unwrap_or_else(|| fallback_cwd.to_path_buf());
        let label = terminal.and_then(|terminal| terminal.manual_label.clone());
        let (agent_name, managed_agent_kind) = terminal
            .filter(|terminal| !terminal.managed_agent_launch_pending())
            .map_or_default(|terminal| {
                (
                    terminal.agent_name.clone(),
                    terminal
                        .managed_agent_kind()
                        .map(|agent| shepr_agent::detect::agent_label(agent).to_string()),
                )
            });
        let launch_argv = terminal.and_then(|terminal| terminal.launch_argv.clone());
        let agent_session = terminal.and_then(|terminal| {
            if let Some(authority) = terminal.hook_authority.as_ref()
                && let Some(session_ref) = authority.session_ref.as_ref()
            {
                return Some(PaneAgentSessionSnapshot {
                    source: shepr_agent::agent::AgentSource::from_pair(
                        &authority.source,
                        &authority.agent_label,
                    )?,
                    agent: shepr_agent::agent::Agent::parse_canonical_label(
                        &authority.agent_label,
                    )?,
                    session_ref: session_ref.clone(),
                });
            }
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| PaneAgentSessionSnapshot {
                    source: session.source.clone(),
                    agent: session.agent,
                    session_ref: session.session_ref.clone(),
                })
        });
        panes.insert(
            id.raw(),
            PaneSnapshot {
                cwd,
                label,
                agent_name,
                managed_agent_kind,
                agent_session,
                launch_argv,
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

type CarriedHistory = HashMap<TerminalId, CarriedEntry>;

/// Primary-screen history kept across saves, for two cases the live screen
/// cannot cover:
///
/// - A restored pane without a runtime (deferred agent resume, failed
///   restore) keeps its `Restored` history from the loaded file until it
///   runs. Capture reads live runtimes only, so without this a save made
///   before the pane runs would lose its saved screen for good. The first
///   capture that sees a runtime for the pane drops that entry: from then on
///   the pane's own screen supersedes it, even if its first read happens on
///   the alternate screen.
/// - A running pane on the alternate screen (vim, an agent TUI) cannot have
///   its primary screen read (`primary_history_ansi` returns `None`), so
///   saves fall back to its `Live` entry, the last primary history read
///   successfully. Every successful read replaces it and an empty read
///   removes it, so it never holds anything older than the pane's own last
///   primary screen.
///
/// Entries are keyed by terminal ID, one per pane at most, and each capture
/// drops those of panes no longer in the layout. That pruning is why the map
/// belongs to one app (restore creates it, the app hands it to every
/// capture) instead of being process-wide: a capture only knows its own
/// layout, and would drop every other owner's entries.
///
/// Capture (event loop) and resolve (save thread) both use it, hence the
/// shared lock. This lives here rather than on `TerminalState` so restore and
/// capture can share it without widening the terminal interface.
#[derive(Clone, Default)]
pub struct HistoryCarry(std::sync::Arc<std::sync::Mutex<CarriedHistory>>);

impl HistoryCarry {
    fn lock(&self) -> std::sync::MutexGuard<'_, CarriedHistory> {
        // The map holds plain strings with no invariant a panic could break.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keeps a restored pane's saved history for later saves until the pane
    /// has a runtime of its own.
    pub fn carry_restored(&self, terminal: &TerminalId, history: Option<&PaneHistorySnapshot>) {
        if let Some(history) = history {
            self.lock().insert(
                terminal.clone(),
                CarriedEntry {
                    ansi: history.ansi.clone(),
                    origin: CarriedOrigin::Restored,
                },
            );
        }
    }

    /// The save-thread half of a live pane's history: records a successful
    /// primary-screen read as the pane's fallback, or falls back to the last
    /// one while the alternate screen hides the primary screen.
    fn resolve_live(&self, terminal: &TerminalId, read: Option<String>) -> Option<String> {
        let mut carried = self.lock();
        match read {
            Some(ansi) if ansi.trim().is_empty() => {
                carried.remove(terminal);
                None
            }
            Some(ansi) => {
                carried.insert(
                    terminal.clone(),
                    CarriedEntry {
                        ansi: ansi.clone(),
                        origin: CarriedOrigin::Live,
                    },
                );
                Some(ansi)
            }
            None => carried
                .get(terminal)
                .filter(|entry| entry.origin == CarriedOrigin::Live)
                .map(|entry| entry.ansi.clone()),
        }
    }
}

/// Produces one live pane's saved-screen history. Runs on the session save
/// thread, not on the event loop.
type PaneHistoryRead = Box<dyn FnOnce() -> Option<String> + Send>;

enum PendingPaneHistory {
    /// Saved history carried for a pane without a runtime.
    Carried(String),
    Live(TerminalId, PaneHistoryRead),
}

/// Pane history captured on the event loop in the cheapest form available,
/// to be turned into a `SessionHistorySnapshot` off it (`resolve`). The shape
/// mirrors the workspaces and tabs it was captured from.
pub struct PendingHistory {
    workspaces: Vec<Vec<Vec<(u32, PendingPaneHistory)>>>,
    carry: HistoryCarry,
}

impl PendingHistory {
    /// Formats every live pane's history and pairs the result with the layout
    /// it was captured alongside. Meant for the save thread: it can take as
    /// long as formatting every pane's scrollback does.
    pub fn resolve(self, snapshot: &SessionSnapshot) -> SessionHistorySnapshot {
        let carry = self.carry;
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
                                        PendingPaneHistory::Carried(ansi) => Some(ansi),
                                        PendingPaneHistory::Live(terminal, read) => {
                                            carry.resolve_live(&terminal, read())
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

/// Both halves of a history capture in one call. Saves split them across the
/// event loop and the save thread instead.
#[cfg(test)]
pub fn capture_history(
    snapshot: &SessionSnapshot,
    workspaces: &[Workspace],
    terminal_runtimes: &PaneRuntimeRegistry,
    carry: &HistoryCarry,
) -> SessionHistorySnapshot {
    capture_pending_history(workspaces, terminal_runtimes, carry).resolve(snapshot)
}

/// The event-loop half of a history capture; see `PendingHistory`.
pub fn capture_pending_history(
    workspaces: &[Workspace],
    terminal_runtimes: &PaneRuntimeRegistry,
    carry: &HistoryCarry,
) -> PendingHistory {
    let mut carried = carry.lock();
    let workspaces_history = workspaces
        .iter()
        .map(|workspace| {
            workspace
                .tabs()
                .iter()
                .map(|tab| capture_tab_history(tab, terminal_runtimes, &mut carried))
                .collect()
        })
        .collect();
    let live_ids: std::collections::HashSet<_> = workspaces
        .iter()
        .flat_map(|workspace| workspace.tabs().iter())
        .flat_map(|tab| tab.panes.values())
        .map(|pane| &pane.attached_terminal_id)
        .collect();
    carried.retain(|id, _| live_ids.contains(id));
    drop(carried);
    PendingHistory {
        workspaces: workspaces_history,
        carry: carry.clone(),
    }
}

fn capture_tab_history(
    tab: &crate::workspace::Tab,
    terminal_runtimes: &PaneRuntimeRegistry,
    carried: &mut CarriedHistory,
) -> Vec<(u32, PendingPaneHistory)> {
    tab.panes
        .iter()
        .filter_map(|(id, pane)| {
            capture_pane_history(pane, terminal_runtimes, carried)
                .map(|history| (id.raw(), history))
        })
        .collect()
}

fn capture_pane_history(
    pane: &crate::pane::PaneState,
    terminal_runtimes: &PaneRuntimeRegistry,
    carried: &mut CarriedHistory,
) -> Option<PendingPaneHistory> {
    let terminal = &pane.attached_terminal_id;
    let Some(runtime) = terminal_runtimes.get(terminal) else {
        return carried
            .get(terminal)
            .map(|entry| PendingPaneHistory::Carried(entry.ansi.clone()));
    };
    // The pane has a runtime of its own now: its own screen supersedes the
    // history restored for it, permanently, even while that screen is on the
    // alternate buffer and cannot be read. Its own last primary read stays
    // as the alternate-screen fallback.
    if carried
        .get(terminal)
        .is_some_and(|entry| entry.origin == CarriedOrigin::Restored)
    {
        carried.remove(terminal);
    }
    Some(PendingPaneHistory::Live(
        terminal.clone(),
        live_history_read(runtime),
    ))
}

/// How a live pane's history gets read. The save path is built to run this
/// read on the save thread, but a `PaneRuntime` cannot leave the event
/// loop and the pane layer offers no `Send` handle to its terminal core yet,
/// so for now the whole scrollback is still formatted here, on the loop,
/// under one hold of the pane's terminal lock. Once the pane layer exposes
/// such a handle (ideally one that formats in bounded chunks under short lock
/// holds), returning a closure over it is the only change needed here.
fn live_history_read(runtime: &crate::pane::PaneRuntime) -> PaneHistoryRead {
    let ansi = runtime.snapshot_history();
    Box::new(move || ansi)
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

pub fn parse_snapshot(content: &str) -> Result<SessionSnapshot, String> {
    let snapshot = serde_json::from_str::<SessionSnapshot>(content).map_err(|e| e.to_string())?;
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(format!(
            "snapshot version {} is not supported (expected {SNAPSHOT_VERSION})",
            snapshot.version
        ));
    }
    Ok(snapshot)
}

pub(super) fn parse_history_snapshot(content: &str) -> Result<SessionHistorySnapshot, String> {
    let snapshot =
        serde_json::from_str::<SessionHistorySnapshot>(content).map_err(|e| e.to_string())?;
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(format!(
            "history snapshot version {} is not supported (expected {SNAPSHOT_VERSION})",
            snapshot.version
        ));
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Holder {
        #[serde(with = "super::path_bytes")]
        path: PathBuf,
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
}
