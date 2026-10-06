use crate::test_support::*;
use std::path::PathBuf;

use ratatui::layout::Rect;

use super::AppState;
use shepr_core::layout::Direction;
use shepr_mux::pane::PaneRuntimeRegistry;
use shepr_mux::persist::capture;
use shepr_mux::persist::schema::*;
use shepr_mux::workspace::Workspace;
use shepr_protocol::PanePublicNumber;

fn split_ratio(value: f32) -> shepr_core::layout::SplitRatio {
    shepr_core::layout::SplitRatio::new(value).expect("test split ratio is valid")
}

fn number(value: usize) -> PanePublicNumber {
    PanePublicNumber::new(value).expect("nonzero literal")
}

/// The public number `pane` carries in `state`.
fn pane_number(state: &AppState, pane: shepr_core::layout::PaneId) -> PanePublicNumber {
    state
        .pane(pane)
        .expect("the pane is in the state")
        .record()
        .number()
}

fn saved_pane(public_number: usize, cwd: &str) -> PaneSnapshot {
    PaneSnapshot {
        cwd: shepr_core::absolute_path::AbsolutePath::new(cwd).expect("test cwd is absolute"),
        public_number: number(public_number),
        label: None,
        agent_session: None,
        unusable_agent_session: None,
    }
}

/// The pane `workspace` saved under `public_number`.
fn pane_snapshot(workspace: &WorkspaceSnapshot, public_number: usize) -> &PaneSnapshot {
    workspace
        .layout
        .panes()
        .into_iter()
        .find(|pane| pane.public_number == number(public_number))
        .expect("the pane is saved")
}

/// The only pane (or the first, in layout order) `workspace` saved.
fn first_pane_snapshot(workspace: &WorkspaceSnapshot) -> &PaneSnapshot {
    workspace.layout.panes()[0]
}

fn state_with_workspaces(names: &[&str]) -> AppState {
    let mut state = AppState::test_new();
    state.test_set_workspaces(names.iter().map(|name| Workspace::test_new(name)).collect());
    state
}

fn app_from_state(state: AppState) -> crate::app::TestApp {
    let mut app = crate::app::App::new(&shepr_config::ServerConfig::default());
    app.state = state;
    app.state
        .test_record_all_workspace_areas(Rect::new(0, 0, 106, 20));
    app
}

fn send_endpoint_command(
    app: &mut crate::app::App,
    command: shepr_protocol::command::EndpointCommand,
) {
    let context = crate::app::EndpointContext {
        requester_geometry: None,
    };
    let outcome = app.handle_endpoint_command(command, &context);
    assert!(outcome.result.is_ok(), "endpoint command should succeed");
}

fn refresh_test_view(state: &mut AppState, area: Rect) {
    state.test_record_all_workspace_areas(area);
}

fn capture_from_state(state: &AppState) -> SessionSnapshot {
    let terminal_runtimes = PaneRuntimeRegistry::default();
    capture_from_state_with_runtimes(state, &terminal_runtimes)
}

fn capture_from_state_with_runtimes(
    state: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
) -> SessionSnapshot {
    capture(
        state.workspaces(),
        terminal_runtimes,
        &shepr_core::absolute_path::AbsolutePath::root(),
        state.host_terminal_theme(),
    )
    .expect("fixture workspace trees capture consistently")
}

fn root_split_ratio(workspace: &WorkspaceSnapshot) -> Option<f32> {
    match &workspace.layout {
        LayoutSnapshot::Split { ratio, .. } => Some(ratio.get()),
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
    let restored: SessionSnapshot = serde_json::from_str(&json).expect("test precondition");
    assert!(restored.workspaces.is_empty());
    assert_eq!(restored.active, None);
}

#[test]
fn saved_host_theme_round_trips() {
    let color = shepr_term::host::RgbColor {
        r: 12,
        g: 34,
        b: 56,
    };
    let mut theme = shepr_term::host::TerminalTheme {
        background: Some(color),
        ..Default::default()
    };
    theme.palette[240] = Some(color);
    let saved = SavedHostTheme::from(theme);
    let json = serde_json::to_string(&saved).expect("test precondition");
    let loaded: SavedHostTheme = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(loaded.to_theme(), theme);
}

#[test]
fn capture_keeps_the_theme_for_a_headless_resume() {
    let mut state = AppState::test_new();
    let color = shepr_term::host::RgbColor { r: 2, g: 4, b: 8 };
    state.record_host_theme(shepr_term::host::TerminalTheme {
        background: Some(color),
        ..state.host_terminal_theme()
    });
    let snapshot = capture_from_state(&state);
    assert_eq!(snapshot.host_theme.to_theme().background, Some(color));
}

#[test]
fn round_trip_layout_snapshot() {
    let layout = LayoutSnapshot::Split {
        direction: DirectionSnapshot::Horizontal,
        ratio: split_ratio(0.6),
        first: Box::new(LayoutSnapshot::Pane(saved_pane(1, "/"))),
        second: Box::new(LayoutSnapshot::Split {
            direction: DirectionSnapshot::Vertical,
            ratio: split_ratio(0.5),
            first: Box::new(LayoutSnapshot::Pane(saved_pane(2, "/"))),
            second: Box::new(LayoutSnapshot::Pane(saved_pane(3, "/"))),
        }),
    };
    let json = serde_json::to_string(&layout).expect("test precondition");
    let restored: LayoutSnapshot = serde_json::from_str(&json).expect("test precondition");

    match restored {
        LayoutSnapshot::Split { ratio, .. } => {
            assert!((ratio.get() - 0.6).abs() < 0.01);
        }
        _ => panic!("expected split"),
    }
}

#[test]
fn round_trip_full_workspace_snapshot() {
    let mut website = saved_pane(2, "/nonexistent/website");
    website.label = Some(shepr_mux::Label::new("website").expect("test label"));

    let snap = SessionSnapshot {
        host_theme: Default::default(),
        workspaces: vec![WorkspaceSnapshot {
            id: "w1".parse().expect("id"),
            name: shepr_mux::Label::new("pi-mono").expect("test name"),
            next_public_pane_number: number(3),
            layout: LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: split_ratio(0.5),
                first: Box::new(LayoutSnapshot::Pane(saved_pane(1, "/nonexistent/shepr"))),
                second: Box::new(LayoutSnapshot::Pane(website)),
            },
            zoomed: false,
            focused: number(1),
            root_pane: number(1),
        }],
        active: Some(0),
        version: SNAPSHOT_VERSION,
    };

    let json = serde_json::to_string_pretty(&snap).expect("test precondition");
    let restored: SessionSnapshot = serde_json::from_str(&json).expect("test precondition");

    assert_eq!(restored.workspaces.len(), 1);
    assert_eq!(restored.workspaces[0].id, "w1".parse().expect("id"));
    assert_eq!(restored.workspaces[0].name.as_str(), "pi-mono");
    assert_eq!(restored.workspaces[0].layout.panes().len(), 2);
    assert_eq!(
        pane_snapshot(&restored.workspaces[0], 1).cwd,
        PathBuf::from("/nonexistent/shepr")
    );
    assert_eq!(
        pane_snapshot(&restored.workspaces[0], 2)
            .label
            .as_ref()
            .map(shepr_mux::Label::as_str),
        Some("website")
    );
}

#[test]
fn capture_contract_tracks_workspace_order_and_the_bookmark() {
    let mut state = state_with_workspaces(&["a", "b", "c"]);
    state.seed_bookmark_index(Some(1));

    let before: Vec<_> = state.workspaces().iter().map(Workspace::id).collect();
    state.move_workspace(&before[1], Some(&before[0]));

    let snapshot = capture_from_state(&state);
    let ids: Vec<_> = state.workspaces().iter().map(Workspace::id).collect();
    let captured_ids: Vec<_> = snapshot.workspaces.iter().map(|ws| ws.id).collect();
    assert_eq!(captured_ids, ids);
    assert_eq!(snapshot.active, state.bookmark_index());
}

#[test]
fn capture_contract_tracks_workspace_names() {
    let mut state = state_with_workspaces(&["one"]);
    state
        .ws_mut(0)
        .set_name(shepr_mux::Label::new("renamed-workspace").expect("test name"));

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    assert_eq!(workspace.name.as_str(), "renamed-workspace");
}

#[test]
fn capture_contract_tracks_workspace_closure() {
    let mut state = state_with_workspaces(&["one", "two"]);
    state.seed_bookmark_index(Some(1));

    let closing = state.ws(1).id();
    state.close_workspace(&closing);

    let snapshot = capture_from_state(&state);
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].name.as_str(), "one");
    assert_eq!(
        snapshot.active,
        Some(0),
        "the bookmark on the closed workspace moves to the one now at its index"
    );
}

#[test]
fn capture_contract_tracks_layout_focus_zoom_and_root_pane() {
    let mut state = state_with_workspaces(&["one"]);
    let root = state.ws(0).tree().root();
    let second = state.test_split_workspace(0, Direction::Horizontal);
    state.ws_mut(0).focus_pane(second);
    state.toggle_pane_zoom(second).expect("test precondition");

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    assert!(matches!(workspace.layout, LayoutSnapshot::Split { .. }));
    assert_eq!(workspace.focused, pane_number(&state, second));
    assert_eq!(workspace.root_pane, pane_number(&state, root));
    assert!(workspace.zoomed);
    assert_eq!(workspace.layout.panes().len(), 2);
}

#[test]
fn capture_contract_tracks_focus_navigation() {
    let mut state = state_with_workspaces(&["one"]);
    let root = state.ws(0).tree().root();
    let second = state.test_split_workspace(0, Direction::Horizontal);
    refresh_test_view(&mut state, Rect::new(0, 0, 106, 20));
    let mut app = app_from_state(state);
    let pane_id = app.state.pane(root).expect("test precondition").public_id();

    send_endpoint_command(
        &mut app,
        shepr_protocol::command::EndpointCommand::PaneFocusDirection(
            shepr_protocol::command::PaneFocusDirectionParams {
                pane_id,
                direction: shepr_protocol::command::PaneDirection::Right,
            },
        ),
    );

    let snapshot = capture_from_state(&app.state);
    assert_eq!(
        snapshot.workspaces[0].focused,
        pane_number(&app.state, second)
    );
    assert_ne!(
        snapshot.workspaces[0].focused,
        pane_number(&app.state, root)
    );
}

#[test]
fn capture_contract_tracks_resize_ratio_changes() {
    let mut state = state_with_workspaces(&["one"]);
    let root = state.ws(0).tree().root();
    let right = state.test_split_workspace(0, Direction::Horizontal);
    state.ws_mut(0).focus_pane(right);
    refresh_test_view(&mut state, Rect::new(0, 0, 106, 20));
    let mut app = app_from_state(state);
    let pane_id = app.state.pane(root).expect("test precondition").public_id();
    let before = capture_from_state(&app.state);

    send_endpoint_command(
        &mut app,
        shepr_protocol::command::EndpointCommand::PaneResize(
            shepr_protocol::command::PaneResizeParams {
                pane_id,
                direction: shepr_protocol::command::PaneDirection::Right,
            },
        ),
    );

    let after = capture_from_state(&app.state);
    let before_ratio = root_split_ratio(&before.workspaces[0]).expect("test precondition");
    let after_ratio = root_split_ratio(&after.workspaces[0]).expect("test precondition");
    assert_ne!(before_ratio, after_ratio);
    // Resizing moves the split, not the focus.
    assert_eq!(after.workspaces[0].focused, pane_number(&app.state, right));
}

#[test]
fn capture_contract_tracks_pane_closure() {
    let mut state = state_with_workspaces(&["one"]);
    let second = state.test_split_workspace(0, Direction::Horizontal);
    let mut app = app_from_state(state);
    let pane_id = app
        .state
        .pane(second)
        .expect("test precondition")
        .public_id();

    send_endpoint_command(
        &mut app,
        shepr_protocol::command::EndpointCommand::PaneClose(shepr_protocol::command::PaneTarget {
            pane_id,
        }),
    );

    let snapshot = capture_from_state(&app.state);
    let workspace = &snapshot.workspaces[0];
    assert_eq!(workspace.layout.panes().len(), 1);
    assert!(matches!(workspace.layout, LayoutSnapshot::Pane(_)));
    assert!(!workspace.zoomed);
}

#[test]
fn capture_contract_tracks_public_id_counters() {
    let mut state = state_with_workspaces(&["one"]);
    let second = state.test_split_workspace(0, Direction::Horizontal);
    state.test_split_workspace(0, Direction::Vertical);
    state.test_split_workspace(0, Direction::Horizontal);

    state
        .ws_mut(0)
        .close_pane(second)
        .expect("the split pane closes");

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    let mut numbers: Vec<usize> = workspace
        .layout
        .panes()
        .iter()
        .map(|pane| pane.public_number.get())
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, vec![1, 3, 4]);
    assert_eq!(workspace.next_public_pane_number.get(), 5);
}

#[tokio::test]
async fn capture_follows_live_cwd_arbitration_and_keeps_it_after_exit() {
    let old_scratch = crate::test_support::ScratchDir::new("persist-cwd-old");
    let old = std::fs::canonicalize(old_scratch.path()).expect("test precondition");
    let scratch = crate::test_support::ScratchDir::new("persist-cwd");
    let new = std::fs::canonicalize(scratch.path()).expect("test precondition");
    let mut state = AppState::test_new();
    state.test_set_workspaces(vec![Workspace::test_at(Some("cwd-source"), &old)]);
    let pane_id = state.ws(0).tree().root();
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
    let launcher = shepr_mux::pane::PaneLauncher::new(
        shepr_mux::pane::PaneSpawnHandles {
            events,
            render_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            render_dirty: std::sync::Arc::new(shepr_mux::render_signal::RenderSignal::new()),
            pane_teardowns: std::sync::Arc::default(),
            socket_path: "/run/user/1000/shepr-test.sock".into(),
        },
        shepr_mux::pane::PaneShellConfig::new(
            &shepr_test_support::fixture::resolved_shell(&shell),
            false,
        ),
        shepr_core::scrollback::ScrollbackBudget::DISABLED,
        None,
    );
    let runtime = launcher
        .launch(shepr_mux::pane::PaneLaunchRequest {
            pane_id,
            public_id: shepr_protocol::PublicPaneId::new(
                &state.ws(0).id(),
                shepr_protocol::PanePublicNumber::new(1).expect("nonzero literal"),
            ),
            geometry: shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            cwd: &shepr_core::absolute_path::AbsolutePath::new(old.clone())
                .expect("a scratch path is absolute"),
            kind: shepr_mux::pane::LaunchKind::Fresh,
            presentation: shepr_mux::pane::LaunchPresentation::Live {
                theme: Default::default(),
                appearance: None,
            },
        })
        .expect("test precondition");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    // The child is observable once its shell launched.
    let pid = loop {
        if let Some(pid) = runtime.child_pid() {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "the shell launches");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    while (shepr_platform::process_cwd(pid).as_ref() != Some(&new)
        || runtime.cwd().as_ref() != Some(&old))
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(shepr_platform::process_cwd(pid,), Some(new.clone()));
    assert_eq!(
        runtime.cwd(),
        Some(old.clone()),
        "existing reported-cwd accessor is unchanged"
    );
    let mut runtimes = PaneRuntimeRegistry::default();
    runtimes.insert(pane_id, runtime);
    let before = capture_from_state_with_runtimes(&state, &runtimes);
    assert_eq!(
        first_pane_snapshot(&before.workspaces[0]).cwd,
        old,
        "the report arrived after the shell moved, so it wins, as for the live cwd"
    );
    assert_eq!(
        runtimes.values().next().expect("test precondition").cwd(),
        Some(old.clone())
    );
    assert!(
        shepr_platform::ProcessHandle::open(pid,)
            .expect("the pane child is running")
            .signal(shepr_platform::Signal::Kill)
    );
    let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while shepr_platform::process_cwd(pid).is_some() && std::time::Instant::now() < exit_deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(shepr_platform::process_cwd(pid,).is_none());
    let after = capture_from_state_with_runtimes(&state, &runtimes);
    assert_eq!(first_pane_snapshot(&after.workspaces[0]).cwd, old);
    assert_eq!(
        runtimes.values().next().expect("test precondition").cwd(),
        Some(old)
    );
    for (_, runtime) in runtimes.drain() {
        drop(runtime);
    }
}

#[test]
fn capture_contract_tracks_pane_cwds() {
    let mut state = state_with_workspaces(&["one"]);
    let pion_cwd = ScratchDir::new("snapshot-pion-cwd").to_path_buf();
    let shepr_cwd = ScratchDir::new("snapshot-shepr-cwd").to_path_buf();
    let root = state.ws(0).tree().root();
    let second = state.test_split_workspace(0, Direction::Horizontal);
    state
        .terminal_mut(root)
        .set_cwd(shepr_mux::UsableCwd::new(pion_cwd.clone()).expect("test cwd is usable"));
    state
        .terminal_mut(second)
        .set_cwd(shepr_mux::UsableCwd::new(shepr_cwd.clone()).expect("test cwd is usable"));

    let snapshot = capture_from_state(&state);
    let workspace = &snapshot.workspaces[0];
    assert_eq!(pane_snapshot(workspace, 1).cwd, pion_cwd);
    assert_eq!(pane_snapshot(workspace, 2).cwd, shepr_cwd);
}

#[test]
fn capture_contract_tracks_hook_authority_agent_session() {
    let mut state = state_with_workspaces(&["one"]);
    let session_dir = ScratchDir::new("pi-session");
    let session_path = session_dir.join("pi-session.jsonl").display().to_string();
    let root = state.ws(0).tree().root();
    let terminal = state.terminal_mut(root);
    terminal.set_detected_state(Some(shepr_agent::Agent::Pi), shepr_agent::AgentState::Idle);
    terminal.ownership_mut().set_persisted_agent_session(
        shepr_agent::resume::PersistedAgentSession::new(
            shepr_agent::AgentSource::parse("shepr:pi").expect("bundled source"),
            shepr_agent::resume::AgentSessionRef::path(session_path.clone())
                .expect("test precondition"),
        )
        .expect("test session is valid"),
    );
    terminal.ownership_mut().set_hook_report_at(
        shepr_agent::ReportOrigin::parse("shepr:pi").expect("test origin"),
        shepr_agent::AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(20),
        shepr_detect::ownership::HookClockSample {
            monotonic: std::time::Instant::now(),
            wall: std::time::SystemTime::now(),
        },
    );

    let snapshot = capture_from_state(&state);
    let agent_session = first_pane_snapshot(&snapshot.workspaces[0])
        .agent_session
        .as_ref()
        .expect("agent session should be captured");

    assert_eq!(agent_session.source().as_str(), "shepr:pi");
    assert_eq!(agent_session.agent().label(), "pi");
    assert_eq!(
        agent_session.session_ref().kind(),
        shepr_agent::resume::AgentSessionRefKind::Path
    );
    assert_eq!(agent_session.session_ref().value_str(), session_path);
}

#[test]
fn capture_contract_preserves_restored_agent_session() {
    let mut state = state_with_workspaces(&["one"]);
    let root = state.ws(0).tree().root();
    state
        .terminal_mut(root)
        .ownership_mut()
        .set_persisted_agent_session(
            shepr_agent::resume::PersistedAgentSession::new(
                shepr_agent::AgentSource::parse("shepr:opencode").expect("bundled source"),
                shepr_agent::resume::AgentSessionRef::id("opencode-session")
                    .expect("test precondition"),
            )
            .expect("test session is valid"),
        );

    let snapshot = capture_from_state(&state);
    let agent_session = first_pane_snapshot(&snapshot.workspaces[0])
        .agent_session
        .as_ref()
        .expect("persisted agent session should be captured");

    assert_eq!(agent_session.source().as_str(), "shepr:opencode");
    assert_eq!(agent_session.agent().label(), "opencode");
    assert_eq!(
        agent_session.session_ref().kind(),
        shepr_agent::resume::AgentSessionRefKind::Id
    );
    assert_eq!(agent_session.session_ref().value_str(), "opencode-session");
}

#[test]
fn other_or_missing_version_is_rejected() {
    let theme = serde_json::to_value(SavedHostTheme::default()).expect("test precondition");
    for version in [None, Some(999)] {
        let mut json = serde_json::json!({
            "host_theme": theme,
            "workspaces": [],
            "active": null,
        });
        if let Some(version) = version {
            json["version"] = version.into();
        }
        assert!(serde_json::from_value::<SessionSnapshot>(json).is_err());
    }
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

    let snap = SessionSnapshot {
        version: SNAPSHOT_VERSION,
        host_theme: Default::default(),
        workspaces: vec![WorkspaceSnapshot {
            id: "w1".parse().expect("id"),
            name: shepr_mux::Label::new("fallback test").expect("test name"),
            next_public_pane_number: number(3),
            layout: LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: split_ratio(0.5),
                first: Box::new(LayoutSnapshot::Pane(PaneSnapshot {
                    cwd: shepr_core::absolute_path::AbsolutePath::new(missing_cwd.clone())
                        .expect("a scratch path is absolute"),
                    ..saved_pane(1, "/")
                })),
                second: Box::new(LayoutSnapshot::Pane(PaneSnapshot {
                    cwd: shepr_core::absolute_path::AbsolutePath::new(existing_cwd.clone())
                        .expect("a scratch path is absolute"),
                    ..saved_pane(2, "/")
                })),
            },
            zoomed: false,
            focused: number(1),
            root_pane: number(1),
        }],
        active: Some(0),
    };

    let json = serde_json::to_string(&snap).expect("test precondition");
    let restored: SessionSnapshot = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored.workspaces.len(), 1);
    assert_eq!(pane_snapshot(&restored.workspaces[0], 1).cwd, missing_cwd);
    assert_eq!(pane_snapshot(&restored.workspaces[0], 2).cwd, existing_cwd);
}
