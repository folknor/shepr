use crate::test_support::*;
use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::layout::Rect;

use super::AppState;
use shepr_core::layout::{Direction, NavDirection};
use shepr_mux::pane::PaneRuntimeRegistry;
use shepr_mux::persist::snapshot::*;
use shepr_mux::terminal::TerminalState;
use shepr_mux::workspace::Workspace;

fn state_with_workspaces(names: &[&str]) -> AppState {
    let mut state = AppState::test_new();
    state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
    state.ensure_test_terminals();
    if !state.workspaces.is_empty() {
        state.set_bookmark_index(Some(0));
    }
    state
}

fn refresh_test_view(state: &mut AppState, area: Rect) {
    state.test_record_all_workspace_areas(area);
}

fn capture_from_state(state: &AppState) -> SessionSnapshot {
    let terminal_runtimes = PaneRuntimeRegistry::new();
    capture_from_state_with_runtimes(state, &terminal_runtimes)
}

fn capture_from_state_with_runtimes(
    state: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
) -> SessionSnapshot {
    capture(
        &state.workspaces,
        &state.terminals,
        terminal_runtimes,
        std::path::Path::new("/"),
        state.bookmark_index(),
        state.host_terminal_theme,
    )
}

fn capture_history_from_state_with_runtimes(
    state: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
) -> SessionHistorySnapshot {
    capture_history_with_carry(state, terminal_runtimes, &mut HistoryCarry::default())
}

/// Both halves of a history capture in one call: the event loop's capture,
/// then the persister's resolve against `carry`.
fn capture_history_with_carry(
    state: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    carry: &mut HistoryCarry,
) -> SessionHistorySnapshot {
    let snapshot = capture_from_state_with_runtimes(state, terminal_runtimes);
    shepr_mux::persist::capture_pending_history(&state.workspaces, terminal_runtimes)
        .resolve(&snapshot, carry)
}

fn root_split_ratio(workspace: &WorkspaceSnapshot) -> Option<f32> {
    match &workspace.layout {
        LayoutSnapshot::Split { ratio, .. } => Some(*ratio),
        LayoutSnapshot::Pane(_) => None,
    }
}

#[test]
fn round_trip_empty_session() {
    let snap = SessionSnapshot {
        version: SNAPSHOT_VERSION,
        host_theme: Default::default(),
        workspaces: vec![],
        active: None,
    };
    let json = serde_json::to_string(&snap).expect("test precondition");
    let restored = parse_snapshot(&json).expect("test precondition");
    assert!(restored.workspaces.is_empty());
    assert_eq!(restored.active, None);
}

#[test]
fn saved_host_theme_round_trips_and_old_snapshots_default_to_empty() {
    let color = shepr_termio::host_term::theme::RgbColor {
        r: 12,
        g: 34,
        b: 56,
    };
    let mut theme = shepr_termio::host_term::theme::TerminalTheme {
        background: Some(color),
        ..Default::default()
    };
    theme.palette[240] = Some(color);
    let saved = SavedHostTheme::from(theme);
    let json = serde_json::to_string(&saved).expect("test precondition");
    let loaded: SavedHostTheme = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(loaded.to_theme(), theme);

    let old = r#"{"version":1,"workspaces":[],"active":null}"#;
    let loaded = parse_snapshot(old).expect("old snapshot remains readable");
    assert!(loaded.host_theme.to_theme().is_empty());
}

#[test]
fn capture_keeps_the_theme_for_a_headless_resume() {
    let mut state = AppState::test_new();
    let color = shepr_termio::host_term::theme::RgbColor { r: 2, g: 4, b: 8 };
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
            public_number: Some(1),
            label: None,
            agent_session: None,
        },
    );
    panes.insert(
        1,
        PaneSnapshot {
            cwd: PathBuf::from("/home/can/Projects/website"),
            public_number: Some(2),
            label: Some("website".into()),
            agent_session: None,
        },
    );

    let snap = SessionSnapshot {
        host_theme: Default::default(),
        workspaces: vec![WorkspaceSnapshot {
            id: Some("wproj".to_string()),
            custom_name: Some("pi-mono".to_string()),
            identity_cwd: PathBuf::from("/home/can/Projects/shepr"),
            next_public_pane_number: 3,
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
        active: Some(0),
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
    assert_eq!(restored.workspaces[0].panes.len(), 2);
    assert_eq!(
        restored.workspaces[0].panes[&0].cwd,
        PathBuf::from("/home/can/Projects/shepr")
    );
    assert_eq!(
        restored.workspaces[0].panes[&1].label.as_deref(),
        Some("website")
    );
}

#[test]
fn capture_contract_tracks_workspace_order_and_the_bookmark() {
    let mut state = state_with_workspaces(&["a", "b", "c"]);
    state.set_bookmark_index(Some(1));

    state.move_workspace(1, 0);

    let snapshot = capture_from_state(&state);
    let ids: Vec<_> = state.workspaces.iter().map(|ws| ws.id.clone()).collect();
    let captured_ids: Vec<_> = snapshot
        .workspaces
        .iter()
        .map(|ws| ws.id.clone().expect("test precondition"))
        .collect();
    assert_eq!(captured_ids, ids);
    assert_eq!(snapshot.active, state.bookmark_index());
}

#[test]
fn capture_contract_tracks_workspace_names() {
    let mut state = state_with_workspaces(&["one"]);
    state.workspaces[0].set_custom_name("renamed-workspace".into());

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    assert_eq!(workspace.custom_name.as_deref(), Some("renamed-workspace"));
}

#[test]
fn capture_contract_tracks_workspace_closure() {
    let mut state = state_with_workspaces(&["one", "two"]);
    state.set_bookmark_index(Some(1));

    state.close_workspace_at(1);

    let snapshot = capture_from_state(&state);
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].custom_name.as_deref(), Some("one"));
    assert_eq!(
        snapshot.active,
        Some(0),
        "the bookmark on the closed workspace moves to the one now at its index"
    );
}

#[test]
fn capture_contract_tracks_layout_focus_zoom_and_root_pane() {
    let mut state = state_with_workspaces(&["one"]);
    let root = state.workspaces[0].root_pane();
    let second = state.workspaces[0].test_split(Direction::Horizontal);
    state.workspaces[0].focus_pane(second);
    state
        .toggle_pane_zoom(0, second)
        .expect("test precondition");

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    assert!(matches!(workspace.layout, LayoutSnapshot::Split { .. }));
    assert_eq!(workspace.focused, Some(second.raw()));
    assert_eq!(workspace.root_pane, Some(root.raw()));
    assert!(workspace.zoomed);
    assert_eq!(workspace.panes.len(), 2);
}

#[test]
fn capture_contract_tracks_focus_navigation() {
    let mut state = state_with_workspaces(&["one"]);
    let root = state.workspaces[0].root_pane();
    let second = state.workspaces[0].test_split(Direction::Horizontal);
    refresh_test_view(&mut state, Rect::new(0, 0, 106, 20));

    state.navigate_pane(0, NavDirection::Right);

    let snapshot = capture_from_state(&state);
    assert_eq!(snapshot.workspaces[0].focused, Some(second.raw()));
    assert_ne!(snapshot.workspaces[0].focused, Some(root.raw()));
}

#[test]
fn capture_contract_tracks_resize_ratio_changes() {
    let mut state = state_with_workspaces(&["one"]);
    let root = state.workspaces[0].root_pane();
    state.workspaces[0].test_split(Direction::Horizontal);
    state.workspaces[0].focus_pane(root);
    refresh_test_view(&mut state, Rect::new(0, 0, 106, 20));
    let before = capture_from_state(&state);

    state.resize_pane(0, NavDirection::Right);

    let after = capture_from_state(&state);
    let before_ratio = root_split_ratio(&before.workspaces[0]).expect("test precondition");
    let after_ratio = root_split_ratio(&after.workspaces[0]).expect("test precondition");
    assert_ne!(before_ratio, after_ratio);
}

#[test]
fn capture_contract_tracks_pane_closure() {
    let mut state = state_with_workspaces(&["one"]);
    state.workspaces[0].test_split(Direction::Horizontal);

    let focused = state.workspaces[0].focused_pane_id();
    assert!(matches!(
        state.remove_pane(0, focused),
        crate::app::actions::PaneRemovalCommit::Removed(_)
    ));

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    assert_eq!(workspace.panes.len(), 1);
    assert!(matches!(workspace.layout, LayoutSnapshot::Pane(_)));
    assert!(!workspace.zoomed);
}

#[test]
fn capture_contract_tracks_public_id_counters() {
    let mut state = state_with_workspaces(&["one"]);
    let second = state.workspaces[0].test_split(Direction::Horizontal);
    let third = state.workspaces[0].test_split(Direction::Vertical);
    let fourth = state.workspaces[0].test_split(Direction::Horizontal);

    state.workspaces[0]
        .close_pane(second)
        .expect("the split pane closes");

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    let numbers: HashMap<u32, Option<usize>> = workspace
        .panes
        .iter()
        .map(|(id, pane)| (*id, pane.public_number))
        .collect();
    assert_eq!(
        numbers,
        HashMap::from([
            (state.workspaces[0].root_pane().raw(), Some(1)),
            (third.raw(), Some(3)),
            (fourth.raw(), Some(4)),
        ])
    );
    assert_eq!(workspace.next_public_pane_number, 5);
}

#[tokio::test]
async fn capture_prefers_live_shell_cwd_and_keeps_it_after_exit() {
    let old_scratch = crate::test_support::ScratchDir::new("persist-cwd-old");
    let old = std::fs::canonicalize(old_scratch.path()).expect("test precondition");
    let scratch = crate::test_support::ScratchDir::new("persist-cwd");
    let new = std::fs::canonicalize(scratch.path()).expect("test precondition");
    let mut state = AppState::test_new();
    state.workspaces = vec![Workspace::test_new("cwd-source")];
    state.workspaces[0].identity_cwd = old.clone();
    state.set_bookmark_index(Some(0));
    state.ensure_test_terminals();
    let pane_id = state.workspaces[0].root_pane();
    let terminal_id = state.workspaces[0]
        .terminal_id(pane_id)
        .expect("test precondition")
        .clone();
    let (events, _rx) = tokio::sync::mpsc::channel(32);
    // A stand-in pane shell that moves to `new` while reporting `old` over
    // OSC 7, then stays alive.
    let shell_dir = crate::test_support::ScratchDir::new("persist-cwd-shell");
    let shell = shepr_test_support::fixture::stand_in(
        &shell_dir,
        "sh",
        &[
            shepr_test_support::fixture::Step::Cd(new.clone()),
            shepr_test_support::fixture::Step::Print(format!(
                "\x1b]7;file://{}\x07",
                old.display()
            )),
            shepr_test_support::fixture::Step::Sleep(std::time::Duration::from_secs(30)),
        ],
    );
    let runtime = shepr_mux::pane::PaneRuntime::spawn(
        pane_id,
        shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
        &old,
        0,
        Default::default(),
        None,
        shepr_mux::pane::PaneShellConfig::new(shell.to_str().expect("test precondition"), false),
        &shepr_mux::pane::PaneLaunchEnv::from_extra(
            Vec::new(),
            "/run/user/1000/shepr-test.sock".into(),
        ),
        &events,
        &std::sync::Arc::new(tokio::sync::Notify::new()),
        &std::sync::Arc::new(shepr_mux::render_signal::RenderSignal::new()),
        &std::sync::Arc::default(),
    )
    .expect("test precondition");
    let pid = runtime.child_pid().expect("test precondition");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while (shepr_agent::detect::process_cwd(pid).as_ref() != Some(&new)
        || runtime.cwd().as_ref() != Some(&old))
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(shepr_agent::detect::process_cwd(pid), Some(new.clone()));
    assert_eq!(
        runtime.cwd(),
        Some(old.clone()),
        "existing reported-cwd accessor is unchanged"
    );
    let mut runtimes = PaneRuntimeRegistry::new();
    runtimes.insert(terminal_id, runtime);
    let before = capture_from_state_with_runtimes(&state, &runtimes);
    assert_eq!(
        before.workspaces[0]
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
    assert!(
        shepr_platform::ProcessHandle::open(pid)
            .expect("the pane child is running")
            .signal(shepr_platform::Signal::Kill)
    );
    let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while shepr_agent::detect::process_cwd(pid).is_some()
        && std::time::Instant::now() < exit_deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(shepr_agent::detect::process_cwd(pid).is_none());
    let after = capture_from_state_with_runtimes(&state, &runtimes);
    assert_eq!(
        after.workspaces[0]
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
        drop(runtime);
    }
}

#[test]
fn capture_contract_tracks_workspace_identity_and_pane_cwds() {
    let mut state = state_with_workspaces(&["one"]);
    let pion_cwd = ScratchDir::new("snapshot-pion-cwd").to_path_buf();
    let shepr_cwd = ScratchDir::new("snapshot-shepr-cwd").to_path_buf();
    let root = state.workspaces[0].root_pane();
    state.workspaces[0].identity_cwd = pion_cwd.clone();
    let second = state.workspaces[0].test_split(Direction::Horizontal);
    state.ensure_test_terminals();
    let root_terminal_id = state.workspaces[0].panes()[&root]
        .attached_terminal_id
        .clone();
    state.terminals.insert(
        root_terminal_id.clone(),
        TerminalState::new(root_terminal_id.clone(), pion_cwd.clone()),
    );
    let second_terminal_id = state.workspaces[0].panes()[&second]
        .attached_terminal_id
        .clone();
    state.terminals.insert(
        second_terminal_id.clone(),
        TerminalState::new(second_terminal_id.clone(), shepr_cwd.clone()),
    );

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    assert_eq!(workspace.identity_cwd, pion_cwd);
    assert_eq!(workspace.panes[&root.raw()].cwd, pion_cwd);
    assert_eq!(workspace.panes[&second.raw()].cwd, shepr_cwd);
}

#[tokio::test]
async fn capture_contract_tracks_pane_history_from_runtime() {
    let state = state_with_workspaces(&["one"]);
    let root = state.workspaces[0].root_pane();
    let terminal_id = state.workspaces[0].panes()[&root]
        .attached_terminal_id
        .clone();
    let mut terminal_runtimes = PaneRuntimeRegistry::new();
    terminal_runtimes.insert(
        terminal_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(
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
    let history = &history_snapshot.workspaces[0].panes[&root.raw()];

    assert!(history.ansi.contains("alpha"));
    assert!(history.ansi.contains("gamma"));
}

#[tokio::test]
async fn capture_contract_tracks_history_for_each_pane() {
    let mut state = state_with_workspaces(&["one"]);
    let first = state.workspaces[0].root_pane();
    let second = state.workspaces[0].test_split(Direction::Horizontal);
    let first_terminal_id = state.workspaces[0].panes()[&first]
        .attached_terminal_id
        .clone();
    let second_terminal_id = state.workspaces[0].panes()[&second]
        .attached_terminal_id
        .clone();
    let mut terminal_runtimes = PaneRuntimeRegistry::new();
    terminal_runtimes.insert(
        first_terminal_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(
            20,
            3,
            4096,
            b"first-pane-history\r\n",
        ),
    );
    terminal_runtimes.insert(
        second_terminal_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(
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
    let workspace = &history_snapshot.workspaces[0];
    let first_history = &workspace.panes[&first.raw()];
    let second_history = &workspace.panes[&second.raw()];

    assert!(first_history.ansi.contains("first-pane-history"));
    assert!(second_history.ansi.contains("second-pane-history"));
}

fn root_history(
    history: &SessionHistorySnapshot,
    root: shepr_core::layout::PaneId,
) -> Option<&str> {
    history.workspaces[0]
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
    let root = state.workspaces[0].root_pane();
    let terminal_id = state.workspaces[0].panes()[&root]
        .attached_terminal_id
        .clone();
    let mut terminal_runtimes = PaneRuntimeRegistry::new();
    terminal_runtimes.insert(
        terminal_id.clone(),
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 3, 4096, b"PRIMARY_ONE\r\n"),
    );
    let runtime = |runtimes: &PaneRuntimeRegistry, bytes: &[u8]| {
        runtimes
            .get(&terminal_id)
            .expect("test precondition")
            .test_process_pty_bytes(bytes);
    };
    let mut carry = HistoryCarry::default();

    let saved = capture_history_with_carry(&state, &terminal_runtimes, &mut carry);
    assert!(root_history(&saved, root).is_some_and(|ansi| ansi.contains("PRIMARY_ONE")));

    runtime(&terminal_runtimes, b"\x1b[?1049hALT_FRAME");
    for _ in 0..2 {
        let saved = capture_history_with_carry(&state, &terminal_runtimes, &mut carry);
        let ansi = root_history(&saved, root).expect("alternate screen keeps the history");
        assert!(ansi.contains("PRIMARY_ONE"));
        assert!(!ansi.contains("ALT_FRAME"));
    }

    runtime(&terminal_runtimes, b"\x1b[?1049lPRIMARY_TWO\r\n");
    let saved = capture_history_with_carry(&state, &terminal_runtimes, &mut carry);
    assert!(root_history(&saved, root).is_some_and(|ansi| ansi.contains("PRIMARY_TWO")));
    runtime(&terminal_runtimes, b"\x1b[?1049hALT_AGAIN");
    let saved = capture_history_with_carry(&state, &terminal_runtimes, &mut carry);
    let ansi = root_history(&saved, root).expect("alternate screen keeps the history");
    assert!(ansi.contains("PRIMARY_TWO"));
    assert!(!ansi.contains("ALT_AGAIN"));

    // Closing the pane drops its fallback.
    let other = state_with_workspaces(&["other"]);
    let saved = capture_history_with_carry(&other, &PaneRuntimeRegistry::new(), &mut carry);
    assert_eq!(root_history(&saved, other.workspaces[0].root_pane()), None);
    for (_, runtime) in terminal_runtimes.drain() {
        drop(runtime);
    }
}

/// A restored pane without a runtime keeps its saved history in every
/// save until it runs. From then on only its own screen counts: the
/// restored copy is gone even while the pane is on the alternate screen,
/// and even if the pane later loses its runtime again.
#[tokio::test]
async fn restored_history_is_carried_until_the_pane_runs_then_superseded() {
    let state = state_with_workspaces(&["one"]);
    let root = state.workspaces[0].root_pane();
    let terminal_id = state.workspaces[0].panes()[&root]
        .attached_terminal_id
        .clone();
    let mut carry = HistoryCarry::default();
    carry.carry_restored(
        &terminal_id,
        Some(&PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
        }),
    );
    let mut terminal_runtimes = PaneRuntimeRegistry::new();
    for _ in 0..2 {
        let saved = capture_history_with_carry(&state, &terminal_runtimes, &mut carry);
        assert_eq!(root_history(&saved, root), Some("RESTORED_HISTORY\r\n"));
    }

    // The pane starts straight into an alternate-screen program, as a
    // resumed agent does: no primary history of its own yet.
    terminal_runtimes.insert(
        terminal_id.clone(),
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(
            20,
            3,
            4096,
            b"\x1b[?1049hAGENT_TUI",
        ),
    );
    let saved = capture_history_with_carry(&state, &terminal_runtimes, &mut carry);
    assert_eq!(root_history(&saved, root), None);

    terminal_runtimes
        .get(&terminal_id)
        .expect("test precondition")
        .test_process_pty_bytes(b"\x1b[?1049lLIVE_SCREEN\r\n");
    let saved = capture_history_with_carry(&state, &terminal_runtimes, &mut carry);
    let ansi = root_history(&saved, root).expect("live history is saved");
    assert!(ansi.contains("LIVE_SCREEN"));
    assert!(!ansi.contains("RESTORED_HISTORY"));

    if let Some(runtime) = terminal_runtimes.remove(&terminal_id) {
        drop(runtime);
    }
    let saved = capture_history_with_carry(&state, &terminal_runtimes, &mut carry);
    let ansi = root_history(&saved, root).expect("last live history is kept");
    assert!(ansi.contains("LIVE_SCREEN"));
    assert!(!ansi.contains("RESTORED_HISTORY"));
}

#[test]
fn capture_contract_tracks_hook_authority_agent_session() {
    let mut state = state_with_workspaces(&["one"]);
    let session_dir = ScratchDir::new("pi-session");
    let session_path = session_dir.join("pi-session.jsonl").display().to_string();
    let root = state.workspaces[0].root_pane();
    state.ensure_test_terminals();
    let terminal_id = state.workspaces[0].panes()[&root]
        .attached_terminal_id
        .clone();
    let terminal = state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition");
    terminal.set_detected_state(
        Some(shepr_agent::detect::Agent::Pi),
        shepr_agent::detect::AgentState::Idle,
    );
    terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
        source: "shepr:pi".into(),
        agent: shepr_agent::agent::Agent::Pi,
        session_ref: shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone())
            .expect("test precondition"),
    });
    terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        shepr_agent::detect::AgentState::Working,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(20),
        shepr_mux::terminal::state::HookClockSample {
            monotonic: std::time::Instant::now(),
            wall: std::time::SystemTime::now(),
        },
    );

    let snapshot = capture_from_state(&state);
    let agent_session = snapshot.workspaces[0].panes[&root.raw()]
        .agent_session
        .as_ref()
        .expect("agent session should be captured");

    assert_eq!(agent_session.source, "shepr:pi");
    assert_eq!(agent_session.agent, "pi");
    assert_eq!(
        agent_session.session_ref.kind(),
        shepr_agent::agent::resume::AgentSessionRefKind::Path
    );
    assert_eq!(agent_session.session_ref.value_str(), session_path);
}

#[test]
fn capture_contract_preserves_restored_agent_session() {
    let mut state = state_with_workspaces(&["one"]);
    let root = state.workspaces[0].root_pane();
    state.ensure_test_terminals();
    let terminal_id = state.workspaces[0].panes()[&root]
        .attached_terminal_id
        .clone();
    state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
            source: "shepr:opencode".into(),
            agent: shepr_agent::agent::Agent::OpenCode,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::id("opencode-session")
                .expect("test precondition"),
        });

    let snapshot = capture_from_state(&state);
    let agent_session = snapshot.workspaces[0].panes[&root.raw()]
        .agent_session
        .as_ref()
        .expect("persisted agent session should be captured");

    assert_eq!(agent_session.source, "shepr:opencode");
    assert_eq!(agent_session.agent, "opencode");
    assert_eq!(
        agent_session.session_ref.kind(),
        shepr_agent::agent::resume::AgentSessionRefKind::Id
    );
    assert_eq!(agent_session.session_ref.value_str(), "opencode-session");
}

#[test]
fn other_or_missing_version_is_rejected() {
    let json = r#"{"workspaces":[],"active":null}"#;
    assert!(parse_snapshot(json).is_err());
    let json = r#"{"version":999,"workspaces":[],"active":null}"#;
    assert!(parse_snapshot(json).is_err());
}

#[test]
fn snapshot_parsing_preserves_missing_cwd() {
    let scratch = ScratchDir::new("snapshot-cwd");
    let missing_cwd = scratch.join("missing-cwd");
    let existing_cwd = scratch.to_path_buf();
    assert_eq!(
        std::fs::symlink_metadata(&missing_cwd)
            .expect_err("test precondition")
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(
        std::fs::metadata(&existing_cwd)
            .expect("test precondition")
            .is_dir()
    );

    let mut panes = HashMap::new();
    panes.insert(
        0,
        PaneSnapshot {
            cwd: missing_cwd.clone(),
            public_number: None,
            label: None,
            agent_session: None,
        },
    );
    panes.insert(
        1,
        PaneSnapshot {
            cwd: existing_cwd.clone(),
            public_number: None,
            label: None,
            agent_session: None,
        },
    );

    let snap = SessionSnapshot {
        version: SNAPSHOT_VERSION,
        host_theme: Default::default(),
        workspaces: vec![WorkspaceSnapshot {
            id: Some("test-ws".to_string()),
            custom_name: Some("fallback test".to_string()),
            identity_cwd: existing_cwd,
            next_public_pane_number: 0,
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
        active: Some(0),
    };

    let json = serde_json::to_string(&snap).expect("test precondition");
    let restored = parse_snapshot(&json).expect("test precondition");
    assert_eq!(restored.workspaces.len(), 1);
    assert_eq!(restored.workspaces[0].panes[&0].cwd, missing_cwd);
}
