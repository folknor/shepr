use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::layout::Direction;
use serde::{Deserialize, Serialize};

use crate::layout::Node;
use crate::terminal::{TerminalId, TerminalRuntimeRegistry};
use crate::workspace::Workspace;

/// Current snapshot format version. Files with any other version are ignored.
pub(super) const SNAPSHOT_VERSION: u32 = 1;

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
    pub foreground: Option<crate::terminal_theme::RgbColor>,
    pub background: Option<crate::terminal_theme::RgbColor>,
    #[serde(default)]
    pub palette: Vec<Option<crate::terminal_theme::RgbColor>>,
}

impl From<crate::terminal_theme::TerminalTheme> for SavedHostTheme {
    fn from(theme: crate::terminal_theme::TerminalTheme) -> Self {
        Self {
            foreground: theme.foreground,
            background: theme.background,
            palette: theme.palette.into(),
        }
    }
}

impl SavedHostTheme {
    pub fn to_theme(&self) -> crate::terminal_theme::TerminalTheme {
        let mut theme = crate::terminal_theme::TerminalTheme {
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
    pub identity_cwd: PathBuf,
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
    pub source: String,
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
}

/// Saved screen history of one pane. Files written by older builds also carry
/// a `lines` count; nothing read it, and serde skips it on load.
#[derive(Serialize, Deserialize)]
pub struct PaneHistorySnapshot {
    pub ansi: String,
}

/// Serializable BSP tree.
#[derive(Serialize, Deserialize)]
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
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
    fallback_cwd: &std::path::Path,
    active: Option<usize>,
    selected: usize,
    host_theme: crate::terminal_theme::TerminalTheme,
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
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
    fallback_cwd: &std::path::Path,
) -> WorkspaceSnapshot {
    let tabs: Vec<_> = ws
        .tabs
        .iter()
        .map(|tab| capture_tab(tab, terminals, terminal_runtimes, fallback_cwd))
        .collect();
    let identity_cwd = tabs
        .first()
        .and_then(|tab| tab.root_pane.and_then(|id| tab.panes.get(&id)))
        .map(|pane| pane.cwd.clone())
        .unwrap_or_else(|| ws.identity_cwd.clone());
    WorkspaceSnapshot {
        id: Some(ws.id.clone()),
        custom_name: ws.custom_name.clone(),
        identity_cwd,
        public_pane_numbers: ws
            .public_pane_numbers
            .iter()
            .map(|(pane_id, number)| (pane_id.raw(), *number))
            .collect(),
        next_public_pane_number: ws.next_public_pane_number,
        public_tab_numbers: ws.tabs.iter().map(|tab| tab.number).collect(),
        next_public_tab_number: ws.next_public_tab_number,
        tabs,
        active_tab: ws.active_tab,
    }
}

fn capture_tab(
    tab: &crate::workspace::Tab,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
    fallback_cwd: &std::path::Path,
) -> TabSnapshot {
    let mut panes = HashMap::new();
    for id in tab.panes.keys() {
        let terminal_id = tab.terminal_id(*id);
        let terminal = terminal_id.and_then(|id| terminals.get(id));
        let cwd = terminal_id
            .and_then(|id| terminal_runtimes.get(id))
            .and_then(crate::terminal::TerminalRuntime::cwd_for_persistence)
            .or_else(|| terminal.map(|terminal| terminal.cwd.clone()))
            .unwrap_or_else(|| fallback_cwd.to_path_buf());
        let label = terminal.and_then(|terminal| terminal.manual_label.clone());
        let (agent_name, managed_agent_kind) = terminal
            .filter(|terminal| !terminal.managed_agent_launch_pending())
            .map(|terminal| {
                (
                    terminal.agent_name.clone(),
                    terminal
                        .managed_agent_kind()
                        .map(|agent| crate::detect::agent_label(agent).to_string()),
                )
            })
            .unwrap_or_default();
        let launch_argv = terminal.and_then(|terminal| terminal.launch_argv.clone());
        let agent_session = terminal.and_then(|terminal| {
            if let Some(authority) = terminal.hook_authority.as_ref()
                && let Some(session_ref) = authority.session_ref.as_ref()
            {
                return Some(PaneAgentSessionSnapshot {
                    source: authority.source.clone(),
                    agent: authority.agent_label.clone(),
                    kind: session_ref.kind,
                    value: session_ref.value.clone(),
                });
            }
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| PaneAgentSessionSnapshot {
                    source: session.source.clone(),
                    agent: session.agent.clone(),
                    kind: session.session_ref.kind,
                    value: session.session_ref.value.clone(),
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

    // Round-trip through `Value` so JSON object keys serialize in sorted order.
    let value = serde_json::to_value(snapshot).ok()?;
    let bytes = serde_json::to_vec(&value).ok()?;
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
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
    pub(super) fn carry_restored(
        &self,
        terminal: &TerminalId,
        history: Option<&PaneHistorySnapshot>,
    ) {
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
pub(crate) struct PendingHistory {
    workspaces: Vec<Vec<Vec<(u32, PendingPaneHistory)>>>,
    carry: HistoryCarry,
}

impl PendingHistory {
    /// Formats every live pane's history and pairs the result with the layout
    /// it was captured alongside. Meant for the save thread: it can take as
    /// long as formatting every pane's scrollback does.
    pub(crate) fn resolve(self, snapshot: &SessionSnapshot) -> SessionHistorySnapshot {
        let carry = self.carry;
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
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
    terminal_runtimes: &TerminalRuntimeRegistry,
    carry: &HistoryCarry,
) -> SessionHistorySnapshot {
    capture_pending_history(workspaces, terminal_runtimes, carry).resolve(snapshot)
}

/// The event-loop half of a history capture; see `PendingHistory`.
pub(crate) fn capture_pending_history(
    workspaces: &[Workspace],
    terminal_runtimes: &TerminalRuntimeRegistry,
    carry: &HistoryCarry,
) -> PendingHistory {
    let mut carried = carry.lock();
    let workspaces_history = workspaces
        .iter()
        .map(|workspace| {
            workspace
                .tabs
                .iter()
                .map(|tab| capture_tab_history(tab, terminal_runtimes, &mut carried))
                .collect()
        })
        .collect();
    let live_ids: std::collections::HashSet<_> = workspaces
        .iter()
        .flat_map(|workspace| workspace.tabs.iter())
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
    terminal_runtimes: &TerminalRuntimeRegistry,
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
    terminal_runtimes: &TerminalRuntimeRegistry,
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
/// read on the save thread, but a `TerminalRuntime` cannot leave the event
/// loop and the pane layer offers no `Send` handle to its terminal core yet,
/// so for now the whole scrollback is still formatted here, on the loop,
/// under one hold of the pane's terminal lock. Once the pane layer exposes
/// such a handle (ideally one that formats in bounded chunks under short lock
/// holds), returning a closure over it is the only change needed here.
fn live_history_read(runtime: &crate::terminal::TerminalRuntime) -> PaneHistoryRead {
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
            ratio: *ratio,
            first: Box::new(capture_node(first)),
            second: Box::new(capture_node(second)),
        },
    }
}

pub(super) fn parse_snapshot(content: &str) -> Result<SessionSnapshot, String> {
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
    use std::collections::HashMap;
    use std::path::PathBuf;

    use ratatui::layout::{Direction, Rect};

    use super::*;
    use crate::app::{AppState, Mode};
    use crate::layout::NavDirection;
    use crate::workspace::Workspace;

    fn test_session_path(name: &str) -> String {
        std::env::current_dir()
            .expect("test precondition")
            .join(name)
            .display()
            .to_string()
    }

    fn state_with_workspaces(names: &[&str]) -> AppState {
        let mut state = AppState::test_new();
        state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        state.ensure_test_terminals();
        if !state.workspaces.is_empty() {
            state.active = Some(0);
            state.selected = 0;
            state.mode = Mode::Terminal;
        }
        state
    }

    fn capture_from_state(state: &AppState) -> SessionSnapshot {
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        capture_from_state_with_runtimes(state, &terminal_runtimes)
    }

    fn capture_from_state_with_runtimes(
        state: &AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> SessionSnapshot {
        capture(
            &state.workspaces,
            &state.terminals,
            terminal_runtimes,
            std::path::Path::new("/"),
            state.active,
            state.selected,
            state.host_terminal_theme,
        )
    }

    fn capture_history_from_state_with_runtimes(
        state: &AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> SessionHistorySnapshot {
        capture_history_with_carry(state, terminal_runtimes, &HistoryCarry::default())
    }

    fn capture_history_with_carry(
        state: &AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
        carry: &HistoryCarry,
    ) -> SessionHistorySnapshot {
        let snapshot = capture_from_state_with_runtimes(state, terminal_runtimes);
        capture_history(&snapshot, &state.workspaces, terminal_runtimes, carry)
    }

    fn root_split_ratio(tab: &TabSnapshot) -> Option<f32> {
        match &tab.layout {
            LayoutSnapshot::Split { ratio, .. } => Some(*ratio),
            LayoutSnapshot::Pane(_) => None,
        }
    }

    #[test]
    fn managed_agent_snapshot_omits_pending_and_persists_active_ownership() {
        let mut state = state_with_workspaces(&["managed-snapshot"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let now = std::time::Instant::now();
        state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .begin_managed_agent(
                "reviewer".into(),
                crate::detect::Agent::Pi,
                now,
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(1),
            );

        let pending = capture_from_state(&state);
        let pending_pane = &pending.workspaces[0].tabs[0].panes[&root.raw()];
        assert_eq!(pending_pane.agent_name, None);
        assert_eq!(pending_pane.managed_agent_kind, None);

        let terminal = state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Idle,
        );
        assert!(terminal.reconcile_managed_agent_at(now, false));
        let active = capture_from_state(&state);
        let active_pane = &active.workspaces[0].tabs[0].panes[&root.raw()];
        assert_eq!(active_pane.agent_name.as_deref(), Some("reviewer"));
        assert_eq!(active_pane.managed_agent_kind.as_deref(), Some("pi"));
    }

    #[test]
    fn round_trip_empty_session() {
        let snap = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![],
            active: None,
            selected: 0,
        };
        let json = serde_json::to_string(&snap).expect("test precondition");
        let restored = parse_snapshot(&json).expect("test precondition");
        assert!(restored.workspaces.is_empty());
        assert_eq!(restored.active, None);
    }

    #[test]
    fn saved_host_theme_round_trips_and_old_snapshots_default_to_empty() {
        let color = crate::terminal_theme::RgbColor {
            r: 12,
            g: 34,
            b: 56,
        };
        let mut theme = crate::terminal_theme::TerminalTheme {
            background: Some(color),
            ..Default::default()
        };
        theme.palette[240] = Some(color);
        let saved = SavedHostTheme::from(theme);
        let json = serde_json::to_string(&saved).expect("test precondition");
        let loaded: SavedHostTheme = serde_json::from_str(&json).expect("test precondition");
        assert_eq!(loaded.to_theme(), theme);

        let old = r#"{"version":1,"workspaces":[],"active":null,"selected":0}"#;
        let loaded = parse_snapshot(old).expect("old snapshot remains readable");
        assert!(loaded.host_theme.to_theme().is_empty());
    }

    #[test]
    fn capture_keeps_the_theme_for_a_headless_resume() {
        let mut state = AppState::test_new();
        let color = crate::terminal_theme::RgbColor { r: 2, g: 4, b: 8 };
        state.host_terminal_theme.background = Some(color);
        let snapshot = capture_from_state(&state);
        assert_eq!(snapshot.host_theme.to_theme().background, Some(color));
    }

    #[test]
    fn round_trip_layout_snapshot() {
        let layout = LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio: 0.6,
            first: Box::new(LayoutSnapshot::Pane(0)),
            second: Box::new(LayoutSnapshot::Split {
                direction: DirectionSnapshot::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(1)),
                second: Box::new(LayoutSnapshot::Pane(2)),
            }),
        };
        let json = serde_json::to_string(&layout).expect("test precondition");
        let restored: LayoutSnapshot = serde_json::from_str(&json).expect("test precondition");

        match restored {
            LayoutSnapshot::Split { ratio, .. } => assert!((ratio - 0.6).abs() < 0.01),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn round_trip_full_workspace_snapshot() {
        let mut panes = HashMap::new();
        panes.insert(
            0,
            PaneSnapshot {
                cwd: PathBuf::from("/home/can/Projects/shepr"),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );
        panes.insert(
            1,
            PaneSnapshot {
                cwd: PathBuf::from("/home/can/Projects/website"),
                label: Some("website".into()),
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );

        let snap = SessionSnapshot {
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("wproj".to_string()),
                custom_name: Some("pi-mono".to_string()),
                identity_cwd: PathBuf::from("/home/can/Projects/shepr"),
                public_pane_numbers: HashMap::from([(0, 1), (1, 2)]),
                next_public_pane_number: 3,
                public_tab_numbers: vec![1],
                next_public_tab_number: 2,
                tabs: vec![TabSnapshot {
                    custom_name: Some("api".to_string()),
                    layout: LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(0)),
                        second: Box::new(LayoutSnapshot::Pane(1)),
                    },
                    panes,
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            version: SNAPSHOT_VERSION,
        };

        let json = serde_json::to_string_pretty(&snap).expect("test precondition");
        let restored = parse_snapshot(&json).expect("test precondition");

        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].id.as_deref(), Some("wproj"));
        assert_eq!(
            restored.workspaces[0].custom_name.as_deref(),
            Some("pi-mono")
        );
        assert_eq!(restored.workspaces[0].tabs.len(), 1);
        assert_eq!(restored.workspaces[0].tabs[0].panes.len(), 2);
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&0].cwd,
            PathBuf::from("/home/can/Projects/shepr")
        );
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&1].label.as_deref(),
            Some("website")
        );
    }

    #[test]
    fn capture_contract_tracks_workspace_order_active_and_selected() {
        let mut state = state_with_workspaces(&["a", "b", "c"]);
        state.active = Some(1);
        state.selected = 2;

        state.move_workspace(1, 0);

        let snapshot = capture_from_state(&state);
        let ids: Vec<_> = state.workspaces.iter().map(|ws| ws.id.clone()).collect();
        let captured_ids: Vec<_> = snapshot
            .workspaces
            .iter()
            .map(|ws| ws.id.clone().expect("test precondition"))
            .collect();
        assert_eq!(captured_ids, ids);
        assert_eq!(snapshot.active, state.active);
        assert_eq!(snapshot.selected, state.selected);
    }

    #[test]
    fn capture_contract_tracks_workspace_and_tab_names_and_active_tab() {
        let mut state = state_with_workspaces(&["one"]);
        state.workspaces[0].set_custom_name("renamed-workspace".into());
        let second_tab = state.workspaces[0].test_add_tab(Some("logs"));
        state.workspaces[0].switch_tab(second_tab);
        state.workspaces[0].tabs[0].set_custom_name("main".into());

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.custom_name.as_deref(), Some("renamed-workspace"));
        assert_eq!(workspace.active_tab, second_tab);
        assert_eq!(workspace.tabs[0].custom_name.as_deref(), Some("main"));
        assert_eq!(workspace.tabs[1].custom_name.as_deref(), Some("logs"));
    }

    #[test]
    fn capture_contract_tracks_workspace_closure() {
        let mut state = state_with_workspaces(&["one", "two"]);
        state.selected = 1;
        state.active = Some(1);

        state.close_selected_workspace();

        let snapshot = capture_from_state(&state);
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].custom_name.as_deref(), Some("one"));
        assert_eq!(snapshot.active, Some(0));
        assert_eq!(snapshot.selected, 0);
    }

    #[test]
    fn capture_contract_tracks_layout_focus_zoom_and_root_pane() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].tabs[0].layout.focus_pane(second);
        state.toggle_zoom();

        let snapshot = capture_from_state(&state);
        let tab = &snapshot.workspaces[0].tabs[0];
        assert!(matches!(tab.layout, LayoutSnapshot::Split { .. }));
        assert_eq!(tab.focused, Some(second.raw()));
        assert_eq!(tab.root_pane, Some(root.raw()));
        assert!(tab.zoomed);
        assert_eq!(tab.panes.len(), 2);
    }

    #[test]
    fn capture_contract_tracks_focus_navigation() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 106, 20),
        );

        state.navigate_pane(NavDirection::Right);

        let snapshot = capture_from_state(&state);
        assert_eq!(snapshot.workspaces[0].tabs[0].focused, Some(second.raw()));
        assert_ne!(snapshot.workspaces[0].tabs[0].focused, Some(root.raw()));
    }

    #[test]
    fn capture_contract_tracks_resize_ratio_changes() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].layout.focus_pane(root);
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 106, 20),
        );
        let before = capture_from_state(&state);

        state.resize_pane(NavDirection::Right);

        let after = capture_from_state(&state);
        let before_ratio =
            root_split_ratio(&before.workspaces[0].tabs[0]).expect("test precondition");
        let after_ratio =
            root_split_ratio(&after.workspaces[0].tabs[0]).expect("test precondition");
        assert_ne!(before_ratio, after_ratio);
    }

    #[test]
    fn capture_contract_tracks_tab_closure() {
        let mut state = state_with_workspaces(&["one"]);
        let second_tab = state.workspaces[0].test_add_tab(Some("logs"));
        state.switch_tab(second_tab);

        state.close_tab();

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.active_tab, 0);
        assert!(workspace.tabs[0].custom_name.is_none());
    }

    #[test]
    fn capture_contract_tracks_pane_closure() {
        let mut state = state_with_workspaces(&["one"]);
        state.workspaces[0].test_split(Direction::Horizontal);

        state.close_pane();

        let snapshot = capture_from_state(&state);
        let tab = &snapshot.workspaces[0].tabs[0];
        assert_eq!(tab.panes.len(), 1);
        assert!(matches!(tab.layout, LayoutSnapshot::Pane(_)));
        assert!(!tab.zoomed);
    }

    #[test]
    fn capture_contract_tracks_public_id_counters() {
        let mut state = state_with_workspaces(&["one"]);
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        let third = state.workspaces[0].test_split(Direction::Vertical);
        let second_tab = state.workspaces[0].test_add_tab(None);

        state.workspaces[0].close_pane(second);

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(
            workspace.public_pane_numbers,
            HashMap::from([
                (state.workspaces[0].tabs[0].root_pane.raw(), 1),
                (third.raw(), 3),
                (state.workspaces[0].tabs[second_tab].root_pane.raw(), 4),
            ])
        );
        assert_eq!(workspace.next_public_pane_number, 5);
        assert_eq!(workspace.public_tab_numbers, vec![1, 2]);
        assert_eq!(workspace.next_public_tab_number, 3);
    }

    #[tokio::test]
    async fn capture_prefers_live_shell_cwd_and_keeps_it_after_exit() {
        let old = std::env::current_dir().expect("test precondition");
        let scratch = crate::test_support::ScratchDir::new("persist-cwd");
        let new = std::fs::canonicalize(scratch.path()).expect("test precondition");
        let mut state = AppState::test_new();
        state.workspaces = vec![Workspace::test_new("cwd-source")];
        state.workspaces[0].identity_cwd = old.clone();
        state.active = Some(0);
        state.ensure_test_terminals();
        let pane_id = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0]
            .terminal_id(pane_id)
            .expect("test precondition")
            .clone();
        let (events, _rx) = tokio::sync::mpsc::channel(32);
        let runtime = crate::terminal::TerminalRuntime::spawn(
            pane_id,
            24,
            80,
            &old,
            0,
            Default::default(),
            None,
            crate::pane::PaneShellConfig::new("/bin/sh", false),
            &crate::pane::PaneLaunchEnv::default(),
            &events,
            &std::sync::Arc::new(tokio::sync::Notify::new()),
            &std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        )
        .expect("test precondition");
        let pid = runtime.child_pid().expect("test precondition");
        runtime
            .try_send_bytes(bytes::Bytes::from(format!(
                "cd '{}'; printf '\\033]7;file://{}\\007'; exec sleep 30\n",
                new.display(),
                old.display()
            )))
            .expect("test precondition");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while (crate::platform::process_cwd(pid).as_ref() != Some(&new)
            || runtime.cwd().as_ref() != Some(&old))
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(crate::platform::process_cwd(pid), Some(new.clone()));
        assert_eq!(
            runtime.cwd(),
            Some(old.clone()),
            "existing reported-cwd accessor is unchanged"
        );
        let mut runtimes = TerminalRuntimeRegistry::new();
        runtimes.insert(terminal_id, runtime);
        let before = capture_from_state_with_runtimes(&state, &runtimes);
        assert_eq!(
            before.workspaces[0].tabs[0]
                .panes
                .values()
                .next()
                .expect("test precondition")
                .cwd,
            new
        );
        assert_eq!(before.workspaces[0].identity_cwd, new);
        assert_eq!(
            runtimes.values().next().expect("test precondition").cwd(),
            Some(old.clone())
        );
        crate::platform::signal_processes(&[pid], crate::platform::Signal::Kill);
        let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while crate::platform::process_cwd(pid).is_some()
            && std::time::Instant::now() < exit_deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(crate::platform::process_cwd(pid).is_none());
        let after = capture_from_state_with_runtimes(&state, &runtimes);
        assert_eq!(
            after.workspaces[0].tabs[0]
                .panes
                .values()
                .next()
                .expect("test precondition")
                .cwd,
            new
        );
        assert_eq!(after.workspaces[0].identity_cwd, new);
        assert_eq!(
            runtimes.values().next().expect("test precondition").cwd(),
            Some(old)
        );
        for (_, runtime) in runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[test]
    fn capture_contract_tracks_workspace_identity_and_pane_cwds() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        state.workspaces[0].identity_cwd = PathBuf::from("/tmp/pion");
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        state.ensure_test_terminals();
        let root_terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        state
            .terminals
            .get_mut(&root_terminal_id)
            .expect("test precondition")
            .cwd = PathBuf::from("/tmp/pion");
        let second_terminal_id = state.workspaces[0].tabs[0].panes[&second]
            .attached_terminal_id
            .clone();
        state
            .terminals
            .get_mut(&second_terminal_id)
            .expect("test precondition")
            .cwd = PathBuf::from("/tmp/shepr");

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        let tab = &workspace.tabs[0];
        assert_eq!(workspace.identity_cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(tab.panes[&root.raw()].cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(tab.panes[&second.raw()].cwd, PathBuf::from("/tmp/shepr"));
    }

    #[tokio::test]
    async fn capture_contract_tracks_pane_history_from_runtime() {
        let state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let mut terminal_runtimes = TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"alpha\r\nbeta\r\ngamma\r\n",
            ),
        );

        let snapshot = capture_from_state_with_runtimes(&state, &terminal_runtimes);
        let encoded = serde_json::to_string(&snapshot).expect("test precondition");
        assert!(!encoded.contains("alpha"));
        assert!(!encoded.contains("\"history\""));

        let history_snapshot = capture_history_from_state_with_runtimes(&state, &terminal_runtimes);
        let history = &history_snapshot.workspaces[0].tabs[0].panes[&root.raw()];

        assert!(history.ansi.contains("alpha"));
        assert!(history.ansi.contains("gamma"));
    }

    #[tokio::test]
    async fn capture_contract_tracks_history_for_each_pane() {
        let mut state = state_with_workspaces(&["one"]);
        let first = state.workspaces[0].tabs[0].root_pane;
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        let first_terminal_id = state.workspaces[0].tabs[0].panes[&first]
            .attached_terminal_id
            .clone();
        let second_terminal_id = state.workspaces[0].tabs[0].panes[&second]
            .attached_terminal_id
            .clone();
        let mut terminal_runtimes = TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(
            first_terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"first-pane-history\r\n",
            ),
        );
        terminal_runtimes.insert(
            second_terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"second-pane-history\r\n",
            ),
        );

        let snapshot = capture_from_state_with_runtimes(&state, &terminal_runtimes);
        let encoded = serde_json::to_string(&snapshot).expect("test precondition");
        assert!(!encoded.contains("first-pane-history"));
        assert!(!encoded.contains("second-pane-history"));

        let history_snapshot = capture_history_from_state_with_runtimes(&state, &terminal_runtimes);
        let tab = &history_snapshot.workspaces[0].tabs[0];
        let first_history = &tab.panes[&first.raw()];
        let second_history = &tab.panes[&second.raw()];

        assert!(first_history.ansi.contains("first-pane-history"));
        assert!(second_history.ansi.contains("second-pane-history"));
    }

    fn root_history(history: &SessionHistorySnapshot, root: crate::layout::PaneId) -> Option<&str> {
        history.workspaces[0].tabs[0]
            .panes
            .get(&root.raw())
            .map(|pane| pane.ansi.as_str())
    }

    /// The alternate screen hides the primary one from saves; a save made
    /// meanwhile keeps the pane's last primary history instead of dropping it
    /// or writing the alternate frame, and the fallback follows every fresh
    /// primary read.
    #[tokio::test]
    async fn running_pane_saved_on_alternate_screen_keeps_last_primary_history() {
        let state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let mut terminal_runtimes = TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"PRIMARY_ONE\r\n",
            ),
        );
        let runtime = |runtimes: &TerminalRuntimeRegistry, bytes: &[u8]| {
            runtimes
                .get(&terminal_id)
                .expect("test precondition")
                .test_process_pty_bytes(bytes);
        };
        let carry = HistoryCarry::default();

        let saved = capture_history_with_carry(&state, &terminal_runtimes, &carry);
        assert!(root_history(&saved, root).is_some_and(|ansi| ansi.contains("PRIMARY_ONE")));

        runtime(&terminal_runtimes, b"\x1b[?1049hALT_FRAME");
        for _ in 0..2 {
            let saved = capture_history_with_carry(&state, &terminal_runtimes, &carry);
            let ansi = root_history(&saved, root).expect("alternate screen keeps the history");
            assert!(ansi.contains("PRIMARY_ONE"));
            assert!(!ansi.contains("ALT_FRAME"));
        }

        runtime(&terminal_runtimes, b"\x1b[?1049lPRIMARY_TWO\r\n");
        let saved = capture_history_with_carry(&state, &terminal_runtimes, &carry);
        assert!(root_history(&saved, root).is_some_and(|ansi| ansi.contains("PRIMARY_TWO")));
        runtime(&terminal_runtimes, b"\x1b[?1049hALT_AGAIN");
        let saved = capture_history_with_carry(&state, &terminal_runtimes, &carry);
        let ansi = root_history(&saved, root).expect("alternate screen keeps the history");
        assert!(ansi.contains("PRIMARY_TWO"));
        assert!(!ansi.contains("ALT_AGAIN"));

        // Closing the pane drops its fallback.
        let other = state_with_workspaces(&["other"]);
        capture_history_with_carry(&other, &TerminalRuntimeRegistry::new(), &carry);
        assert!(carry.lock().is_empty());
        for (_, runtime) in terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    /// A restored pane without a runtime keeps its saved history in every
    /// save until it runs. From then on only its own screen counts: the
    /// restored copy is gone even while the pane is on the alternate screen,
    /// and even if the pane later loses its runtime again.
    #[tokio::test]
    async fn restored_history_is_carried_until_the_pane_runs_then_superseded() {
        let state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let carry = HistoryCarry::default();
        carry.carry_restored(
            &terminal_id,
            Some(&PaneHistorySnapshot {
                ansi: "RESTORED_HISTORY\r\n".into(),
            }),
        );
        let mut terminal_runtimes = TerminalRuntimeRegistry::new();
        for _ in 0..2 {
            let saved = capture_history_with_carry(&state, &terminal_runtimes, &carry);
            assert_eq!(root_history(&saved, root), Some("RESTORED_HISTORY\r\n"));
        }

        // The pane starts straight into an alternate-screen program, as a
        // resumed agent does: no primary history of its own yet.
        terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"\x1b[?1049hAGENT_TUI",
            ),
        );
        let saved = capture_history_with_carry(&state, &terminal_runtimes, &carry);
        assert_eq!(root_history(&saved, root), None);

        terminal_runtimes
            .get(&terminal_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"\x1b[?1049lLIVE_SCREEN\r\n");
        let saved = capture_history_with_carry(&state, &terminal_runtimes, &carry);
        let ansi = root_history(&saved, root).expect("live history is saved");
        assert!(ansi.contains("LIVE_SCREEN"));
        assert!(!ansi.contains("RESTORED_HISTORY"));

        if let Some(runtime) = terminal_runtimes.remove(&terminal_id) {
            runtime.shutdown();
        }
        let saved = capture_history_with_carry(&state, &terminal_runtimes, &carry);
        let ansi = root_history(&saved, root).expect("last live history is kept");
        assert!(ansi.contains("LIVE_SCREEN"));
        assert!(!ansi.contains("RESTORED_HISTORY"));
    }

    #[test]
    fn capture_contract_tracks_hook_authority_agent_session() {
        let mut state = state_with_workspaces(&["one"]);
        let session_path = test_session_path("pi-session.jsonl");
        let root = state.workspaces[0].tabs[0].root_pane;
        state.ensure_test_terminals();
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let terminal = state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Idle,
        );
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "shepr:pi".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef::path(session_path.clone())
                .expect("test precondition"),
        });
        terminal.set_hook_authority_with_session_ref(
            "shepr:pi".into(),
            "pi".into(),
            crate::detect::AgentState::Working,
            None,
            crate::agent_resume::AgentSessionRef::path(session_path.clone()),
            Some(20),
        );

        let snapshot = capture_from_state(&state);
        let agent_session = snapshot.workspaces[0].tabs[0].panes[&root.raw()]
            .agent_session
            .as_ref()
            .expect("agent session should be captured");

        assert_eq!(agent_session.source, "shepr:pi");
        assert_eq!(agent_session.agent, "pi");
        assert_eq!(
            agent_session.kind,
            crate::agent_resume::AgentSessionRefKind::Path
        );
        assert_eq!(agent_session.value, session_path);
    }

    #[test]
    fn capture_contract_preserves_restored_agent_session() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        state.ensure_test_terminals();
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "shepr:opencode".into(),
                agent: "opencode".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("opencode-session")
                    .expect("test precondition"),
            });

        let snapshot = capture_from_state(&state);
        let agent_session = snapshot.workspaces[0].tabs[0].panes[&root.raw()]
            .agent_session
            .as_ref()
            .expect("persisted agent session should be captured");

        assert_eq!(agent_session.source, "shepr:opencode");
        assert_eq!(agent_session.agent, "opencode");
        assert_eq!(
            agent_session.kind,
            crate::agent_resume::AgentSessionRefKind::Id
        );
        assert_eq!(agent_session.value, "opencode-session");
    }

    #[test]
    fn other_or_missing_version_is_rejected() {
        let json = r#"{"workspaces":[],"active":null,"selected":0}"#;
        assert!(parse_snapshot(json).is_err());
        let json = r#"{"version":999,"workspaces":[],"active":null,"selected":0}"#;
        assert!(parse_snapshot(json).is_err());
    }

    #[test]
    fn active_tab_default_is_zero() {
        let json = r#"{"custom_name":"test","identity_cwd":"/tmp","tabs":[]}"#;
        let ws: WorkspaceSnapshot = serde_json::from_str(json).expect("test precondition");
        assert_eq!(ws.active_tab, 0);
    }

    #[test]
    fn snapshot_parsing_preserves_missing_cwd() {
        let mut panes = HashMap::new();
        panes.insert(
            0,
            PaneSnapshot {
                cwd: PathBuf::from("/tmp/this-directory-does-not-exist-for-shepr-test"),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );
        panes.insert(
            1,
            PaneSnapshot {
                cwd: std::env::var("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("/tmp")),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );

        let snap = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("test-ws".to_string()),
                custom_name: Some("fallback test".to_string()),
                identity_cwd: PathBuf::from("/tmp"),
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(0)),
                        second: Box::new(LayoutSnapshot::Pane(1)),
                    },
                    panes,
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
        };

        let json = serde_json::to_string(&snap).expect("test precondition");
        let restored = parse_snapshot(&json).expect("test precondition");
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&0].cwd,
            PathBuf::from("/tmp/this-directory-does-not-exist-for-shepr-test")
        );
    }
}
