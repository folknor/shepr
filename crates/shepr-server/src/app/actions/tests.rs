use super::*;
use crate::app::events::StateEvent;
use crate::test_support::*;
use shepr_agent::{Agent, AgentState};
use shepr_core::layout::Direction;
use shepr_mux::workspace::Workspace;
use std::time::Instant;

fn app_with_workspaces(names: &[&str]) -> AppState {
    let mut state = AppState::test_new();
    for name in names {
        let ws = Workspace::test_new(name);
        state.test_push_workspace(ws);
    }
    if !state.workspaces.is_empty() {
        state.seed_bookmark_index(Some(0));
    }
    state
}

fn report_hook_state(
    state: &mut AppState,
    pane_id: shepr_core::layout::PaneId,
    origin: shepr_agent::ReportOrigin,
    reported_state: AgentState,
    seq: Option<u64>,
    session_ref: Option<shepr_agent::resume::AgentSessionRef>,
) -> StateUpdate {
    let sample = shepr_detect::ownership::HookClockSample {
        monotonic: crate::app::tests::test_clock().now,
        wall: std::time::SystemTime::now(),
    };
    state.handle_state_event(crate::app::events::StateEvent::HookStateReported {
        pane_id,
        sample,
        origin,
        state: reported_state,
        seq,
        session_ref,
    })
}

fn app_from_state(state: AppState) -> crate::app::TestApp {
    let mut app = crate::app::App::new(&shepr_config::ServerConfig::default());
    app.state = state;
    app.state
        .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 20));
    app
}

fn send_endpoint_command(
    app: &mut crate::app::App,
    command: shepr_protocol::command::EndpointCommand,
) {
    let context = crate::app::EndpointContext {
        requester_geometry: None,
    };
    let outcome = app.handle_endpoint_command_with_render(command, &context);
    assert!(outcome.result.is_ok(), "endpoint command should succeed");
}

fn toggle_focused_zoom(state: &mut AppState) {
    let ws_idx = state.bookmark_index().expect("test precondition");
    let pane_id = state.ws(ws_idx).tree().focused();
    state.toggle_pane_zoom(pane_id).expect("test precondition");
}

#[test]
fn pane_removal_command_returns_the_removed_container_scope() {
    let mut state = app_with_workspaces(&["one"]);
    let second_pane = state.test_split_workspace(0, Direction::Horizontal);
    let first_pane = state.ws(0).tree().root();

    let outcome = state
        .remove_pane(first_pane)
        .expect("a pane of the workspace is removed");
    assert_eq!(outcome.scope, PaneRemovalScope::Pane);
    assert_eq!(outcome.pane_id, first_pane);
    assert_eq!(state.ws(0).tree().len(), 1);
    assert!(state.ws(0).tree().contains(second_pane));

    let outcome = state
        .remove_pane(second_pane)
        .expect("the last pane is removed");
    assert_eq!(outcome.scope, PaneRemovalScope::Workspace);
    assert!(state.workspaces.is_empty());
}

#[test]
fn pane_split_state_command_commits_prepared_geometry_and_terminal() {
    let mut state = app_with_workspaces(&["one"]);
    let root_pane = state.ws(0).tree().root();
    assert_eq!(state.ws(0).tree().len(), 1);
    let chrome = state.chrome_in(shepr_core::geometry::Rect::new(0, 0, 80, 24));
    let prepared = state
        .ws(0)
        .prepare_split(
            root_pane,
            Direction::Horizontal,
            &chrome,
            None,
            shepr_core::absolute_path::AbsolutePath::new("/shepr-test/cwd").expect("absolute"),
        )
        .expect("test precondition");
    let new_pane = prepared.pane_id();
    // The public ID a launch would export is the one the pane is registered
    // under.
    let reserved = prepared.public_id().number();
    let revision = state.shell_projection_revision();
    let outcome = state
        .commit_pane_split(prepared)
        .expect("prepared pane split commits");
    assert_ne!(state.shell_projection_revision(), revision);

    assert_eq!(
        state
            .ws(0)
            .tree()
            .pane(new_pane)
            .map(shepr_mux::workspace::PaneRecord::number),
        Some(reserved)
    );
    assert_eq!(
        state
            .terminal(new_pane)
            .map(|terminal| terminal.cwd().to_path_buf()),
        Some(std::path::PathBuf::from("/shepr-test/cwd"))
    );
    assert_eq!(outcome.pane_id, new_pane);
    assert_eq!(outcome.workspace_id, state.ws(0).id());
    assert_eq!(state.ws(0).tree().len(), 2);
    assert!(
        !state
            .ws(0)
            .tree()
            .pane(new_pane)
            .expect("committed pane is present")
            .right_click_passthrough()
    );
    assert_eq!(state.ws(0).tree().focused(), new_pane);
}

#[test]
fn workspace_creation_state_command_commits_spawned_values() {
    let mut state = AppState::test_new();
    let prepared = state
        .workspaces
        .prepare_workspace(
            &shepr_core::absolute_path::AbsolutePath::new("/shepr-test/cwd").expect("absolute"),
        )
        .expect("workspace ID available");
    let root_pane = prepared.root_pane();

    let geometry = shepr_mux::workspace::SpawnGeometry {
        area: shepr_core::geometry::Rect::new(0, 0, 90, 30),
        cell: None,
    };

    let revision = state.shell_projection_revision();
    let outcome = state
        .commit_workspace_creation(prepared, geometry)
        .expect("a fresh workspace commits");
    assert_ne!(state.shell_projection_revision(), revision);

    assert_eq!(outcome.workspace_id, state.ws(0).id());
    assert_eq!(outcome.root_pane, root_pane);
    assert_eq!(state.ws(0).spawn_geometry(), Some(geometry));
    // Creation moves nobody: navigation is per client, and the bookmark follows
    // only an active client's navigation.
    assert_eq!(state.bookmark_index(), None);
    assert_eq!(
        state
            .terminal(root_pane)
            .map(|terminal| terminal.cwd().to_path_buf()),
        Some(std::path::PathBuf::from("/shepr-test/cwd"))
    );
}

#[test]
fn apply_workspace_git_statuses_updates_matching_workspace() {
    let mut state = app_with_workspaces(&["one", "two"]);
    let first_id = state.ws(0).id();
    let first_cwd = state.ws(0).identity_cwd().to_path_buf();
    let second_id = state.ws(1).id();

    let revision = state.shell_projection_revision();
    let changed = state.apply_workspace_git_statuses(vec![(
        WorkspaceGitStatus {
            owner: first_id,
            status: shepr_mux::git::GitStatus {
                cwd: first_cwd.clone(),
                key: shepr_mux::git::GitStatusKey::Checkout(first_cwd.clone()),
                branch: shepr_mux::git::GitBranch::Named("main".into()),
                ahead_behind: Some(shepr_mux::git::AheadBehind {
                    ahead: 2,
                    behind: 1,
                }),
            },
        },
        Some(first_cwd),
    )]);

    assert_ne!(state.shell_projection_revision(), revision);
    assert!(changed);
    assert_eq!(state.ws(0).branch(), Some("main"));
    assert_eq!(
        state.ws(0).git_ahead_behind(),
        Some(shepr_mux::git::AheadBehind {
            ahead: 2,
            behind: 1
        })
    );
    assert_eq!(state.ws(1).id(), second_id);
    assert_eq!(state.ws(1).git_ahead_behind(), None);
}

#[test]
fn apply_workspace_git_statuses_ignores_stale_cwd() {
    let mut state = app_with_workspaces(&["one"]);
    let workspace_id = state.ws(0).id();
    let cwd = state.ws(0).identity_cwd().to_path_buf();
    state.ws_mut(0).apply_git_status(
        shepr_mux::git::GitStatus {
            cwd: cwd.clone(),
            key: shepr_mux::git::GitStatusKey::Checkout(cwd.clone()),
            branch: shepr_mux::git::GitBranch::Named("old".into()),
            ahead_behind: Some(shepr_mux::git::AheadBehind {
                ahead: 1,
                behind: 0,
            }),
        },
        Some(&cwd),
    );

    let current_cwd = state.ws(0).identity_cwd().to_path_buf();
    let revision = state.shell_projection_revision();
    let changed = state.apply_workspace_git_statuses(vec![(
        WorkspaceGitStatus {
            owner: workspace_id,
            status: shepr_mux::git::GitStatus {
                cwd: std::path::PathBuf::from("/definitely/not/current"),
                key: shepr_mux::git::GitStatusKey::Checkout(std::path::PathBuf::from(
                    "/definitely/not/current",
                )),
                branch: shepr_mux::git::GitBranch::Named("main".into()),
                ahead_behind: Some(shepr_mux::git::AheadBehind {
                    ahead: 0,
                    behind: 1,
                }),
            },
        },
        Some(current_cwd),
    )]);

    assert_eq!(state.shell_projection_revision(), revision);
    assert!(!changed);
    assert_eq!(state.ws(0).branch(), Some("old"));
    assert_eq!(
        state.ws(0).git_ahead_behind(),
        Some(shepr_mux::git::AheadBehind {
            ahead: 1,
            behind: 0
        })
    );
}

#[test]
fn apply_workspace_git_statuses_clears_missing_git_status() {
    let mut state = app_with_workspaces(&["one"]);
    let workspace_id = state.ws(0).id();
    let cwd = state.ws(0).identity_cwd().to_path_buf();
    state.ws_mut(0).apply_git_status(
        shepr_mux::git::GitStatus {
            cwd: cwd.clone(),
            key: shepr_mux::git::GitStatusKey::Checkout(cwd.clone()),
            branch: shepr_mux::git::GitBranch::Named("main".into()),
            ahead_behind: Some(shepr_mux::git::AheadBehind {
                ahead: 1,
                behind: 2,
            }),
        },
        Some(&cwd),
    );

    let changed = state.apply_workspace_git_statuses(vec![(
        WorkspaceGitStatus {
            owner: workspace_id,
            status: shepr_mux::git::GitStatus {
                cwd: cwd.clone(),
                key: shepr_mux::git::GitStatusKey::Outside(cwd.clone()),
                branch: shepr_mux::git::GitBranch::OutsideRepository,
                ahead_behind: None,
            },
        },
        Some(cwd),
    )]);

    assert!(changed);
    assert_eq!(state.ws(0).branch(), None);
    assert_eq!(state.ws(0).git_ahead_behind(), None);
}

#[test]
fn set_bookmark_moves_it_and_saves_only_when_it_moved() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.session_dirty = false;
    let third = state.ws(2).id();

    assert!(state.set_bookmark(&third));
    assert_eq!(state.bookmark_index(), Some(2));
    assert!(state.session_dirty);

    state.session_dirty = false;
    assert!(!state.set_bookmark(&third), "already bookmarked");
    assert!(!state.session_dirty);
}

#[test]
fn set_bookmark_of_a_workspace_that_is_gone_is_a_noop() {
    let mut state = app_with_workspaces(&["a"]);
    let gone = shepr_protocol::WorkspaceId::from_number(9_999).expect("nonzero number");

    assert!(!state.set_bookmark(&gone));
    assert_eq!(state.bookmark_index(), Some(0));
}

#[test]
fn move_workspace_reorders_and_the_bookmark_follows_its_workspace() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    let bookmarked_id = state.ws(1).id();
    state.seed_bookmark_index(Some(1));

    let first = state.ws(0).id();
    state.move_workspace(&bookmarked_id, Some(&first));

    let names: Vec<_> = state
        .workspaces
        .iter()
        .map(shepr_mux::workspace::Workspace::name)
        .collect();
    assert_eq!(names, vec!["b", "a", "c"]);
    assert_eq!(state.bookmark_index(), Some(0));
    assert_eq!(state.workspaces().bookmark().as_ref(), Some(&bookmarked_id));
}

#[test]
fn the_bookmark_survives_reorder_and_removal_of_other_workspaces() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.seed_bookmark_index(Some(1));
    let bookmarked_id = state.workspaces().bookmark();
    let (first, second) = (state.ws(0).id(), state.ws(1).id());

    assert!(state.move_workspace(&second, Some(&first)).changed());
    assert_eq!(state.workspaces().bookmark(), bookmarked_id);
    assert_eq!(state.bookmark_index(), Some(0));

    let background = state.ws(1).id();
    state
        .close_workspace(&background)
        .expect("background workspace");
    assert_eq!(state.workspaces().bookmark(), bookmarked_id);
    assert_eq!(state.bookmark_index(), Some(0));
}

#[test]
fn move_workspace_accepts_insert_at_end() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);

    let first = state.ws(0).id();
    state.move_workspace(&first, None);

    let names: Vec<_> = state
        .workspaces
        .iter()
        .map(shepr_mux::workspace::Workspace::name)
        .collect();
    assert_eq!(names, vec!["b", "c", "a"]);
}

#[test]
fn closing_the_bookmarked_workspace_moves_the_bookmark_to_the_one_now_at_its_index() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.seed_bookmark_index(Some(1));
    state.session_dirty = false;
    let closing = state.ws(1).id();

    state.close_workspace(&closing);

    assert_eq!(state.workspaces.len(), 2);
    assert_eq!(state.bookmark_index(), Some(1));
    assert_eq!(state.ws(1).name(), "c");
    assert!(state.session_dirty);
}

#[test]
fn closing_the_last_workspace_clears_the_bookmark() {
    let mut state = app_with_workspaces(&["only"]);
    let only = state.ws(0).id();
    state.close_workspace(&only);

    assert!(state.workspaces.is_empty());
    assert_eq!(state.bookmark_index(), None);
    assert_eq!(state.workspaces().bookmark(), None);
}

#[test]
fn closing_the_bookmarked_workspace_at_the_end_clamps_the_bookmark() {
    let mut state = app_with_workspaces(&["a", "b"]);
    state.seed_bookmark_index(Some(1));
    let last = state.ws(1).id();

    state.close_workspace(&last);

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.bookmark_index(), Some(0));
}

#[test]
fn closing_another_workspace_keeps_the_bookmark() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.seed_bookmark_index(Some(0));
    let other = state.ws(1).id();

    state.close_workspace(&other);

    assert_eq!(state.workspaces.len(), 2);
    assert_eq!(state.ws(0).name(), "a");
    assert_eq!(state.ws(1).name(), "c");
    assert_eq!(state.bookmark_index(), Some(0));
}

#[test]
fn state_changed_updates_pane() {
    let mut state = app_with_workspaces(&["test"]);
    let pane_id = state.ws(0).tree().root();

    state.handle_state_event(StateEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Pi),
        detection: shepr_detect::Detection::new(AgentState::Working, false),
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });

    let terminal = state.terminal(pane_id).expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Working);
    assert_eq!(terminal.ownership().detected_agent(), Some(Agent::Pi));
}

#[test]
fn state_changed_events_advance_the_agent_state_change_sequence() {
    let mut app = app_with_workspaces(&["active", "background"]);
    let pane_id = app.ws(1).tree().root();

    let mut sequence = shepr_agent::StateChangeSeq::NEVER;
    for state in [AgentState::Working, AgentState::Idle] {
        sequence.advance();
        app.handle_state_event(StateEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            detection: shepr_detect::Detection::new(state, false),
            process_exited: false,
            observed_at: Instant::now(),
        });
        assert_eq!(
            app.terminal(pane_id)
                .expect("terminal")
                .ownership()
                .last_agent_state_change_seq(),
            Some(sequence)
        );
    }
}

#[test]
fn agent_state_change_sequence_ignores_idle_unknown_presentation_changes() {
    let mut app = app_with_workspaces(&["active"]);
    let pane_id = app.ws(0).tree().root();
    let state_changed = |state| StateEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Pi),
        detection: shepr_detect::Detection::new(state, false),
        process_exited: false,
        observed_at: Instant::now(),
    };

    app.handle_state_event(state_changed(AgentState::Idle));
    assert_eq!(
        app.terminal(pane_id).expect("terminal").ownership().state(),
        AgentState::Idle
    );
    assert_eq!(
        app.terminal(pane_id)
            .expect("terminal")
            .ownership()
            .last_agent_state_change_seq(),
        None
    );

    app.handle_state_event(state_changed(AgentState::Unknown));
    assert_eq!(
        app.terminal(pane_id).expect("terminal").ownership().state(),
        AgentState::Unknown
    );
    assert_eq!(
        app.terminal(pane_id)
            .expect("terminal")
            .ownership()
            .last_agent_state_change_seq(),
        None
    );

    app.handle_state_event(state_changed(AgentState::Working));
    let mut first = shepr_agent::StateChangeSeq::NEVER;
    first.advance();
    assert_eq!(
        app.terminal(pane_id)
            .expect("terminal")
            .ownership()
            .last_agent_state_change_seq(),
        Some(first)
    );
}

#[test]
fn visible_blocker_overrides_hook_working() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.seed_bookmark_index(Some(0));
    let bg_pane_id = state.ws(1).tree().root();

    state.handle_state_event(StateEvent::StateChanged {
        pane_id: bg_pane_id,
        agent: Some(Agent::Codex),
        detection: shepr_detect::Detection::new(AgentState::Idle, false),
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });
    report_hook_state(
        &mut state,
        bg_pane_id,
        shepr_agent::ReportOrigin::parse("shepr:codex", "codex").expect("test origin"),
        AgentState::Working,
        Some(1),
        shepr_agent::resume::AgentSessionRef::id("codex-session"),
    );
    state.handle_state_event(StateEvent::StateChanged {
        pane_id: bg_pane_id,
        agent: Some(Agent::Codex),
        detection: shepr_detect::Detection::new(AgentState::Blocked, true),

        process_exited: false,
        observed_at: std::time::Instant::now(),
    });

    let terminal = state.terminal(bg_pane_id).expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Blocked);
}

#[test]
fn reserved_native_state_report_does_not_override_screen_state() {
    let mut state = app_with_workspaces(&["active"]);
    state.seed_bookmark_index(Some(0));
    let pane_id = state.ws(0).tree().root();

    state.handle_state_event(StateEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Claude),
        detection: shepr_detect::Detection::new(AgentState::Working, false),
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });
    report_hook_state(
        &mut state,
        pane_id,
        shepr_agent::ReportOrigin::parse("shepr:claude", "claude").expect("test origin"),
        AgentState::Blocked,
        Some(1),
        shepr_agent::resume::AgentSessionRef::id("claude-session"),
    );
    let terminal = state.terminal(pane_id).expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Working);
    assert!(terminal.ownership().hook_authority().is_none());
    assert!(terminal.ownership().persisted_agent_session().is_some());

    state.handle_state_event(StateEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Claude),
        detection: shepr_detect::Detection::new(AgentState::Idle, false),
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });

    let terminal = state.terminal(pane_id).expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Idle);
}

#[test]
fn devin_state_report_refreshes_session_without_overriding_screen_state() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = state.ws(0).tree().root();

    state.handle_state_event(StateEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Devin),
        detection: shepr_detect::Detection::new(AgentState::Idle, false),
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });
    report_hook_state(
        &mut state,
        pane_id,
        shepr_agent::ReportOrigin::parse("shepr:devin", "devin").expect("test origin"),
        AgentState::Working,
        Some(1),
        shepr_agent::resume::AgentSessionRef::id("devin-session"),
    );

    let terminal = state.terminal(pane_id).expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Idle);
    assert!(terminal.ownership().hook_authority().is_none());
    assert!(terminal.ownership().persisted_agent_session().is_some());
}

#[test]
fn session_ref_only_update_marks_session_dirty_without_visible_update() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = state.ws(0).tree().root();

    state.handle_state_event(StateEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Claude),
        detection: shepr_detect::Detection::new(AgentState::Working, false),
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });
    state.session_dirty = false;

    // A session-only integration's state report records its session and
    // leaves the screen-detected state as it was.
    let update = report_hook_state(
        &mut state,
        pane_id,
        shepr_agent::ReportOrigin::official(Agent::Claude).expect("Claude integration"),
        AgentState::Blocked,
        Some(20),
        shepr_agent::resume::AgentSessionRef::id("claude-session"),
    );

    assert_eq!(update, StateUpdate::Unchanged);
    assert!(state.session_dirty);
}

#[test]
fn terminal_cwd_report_updates_terminal_cwd_and_marks_session_dirty() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = state.ws(0).tree().root();
    let scratch = crate::test_support::ScratchDir::new("cwd-report");
    let cwd = scratch.to_path_buf();
    state.session_dirty = false;

    let update = state.handle_state_event(StateEvent::TerminalCwdReported {
        pane_id,
        cwd: shepr_mux::UsableCwd::new(cwd.clone()).expect("test cwd is usable"),
    });

    assert_eq!(update, StateUpdate::Unchanged);
    assert_eq!(
        state.terminal(pane_id).expect("test precondition").cwd(),
        &cwd
    );
    assert!(state.session_dirty);
}

#[test]
fn cwd_report_for_missing_pane_is_ignored() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = state.ws(0).tree().root();
    let before = state
        .terminal(pane_id)
        .expect("test precondition")
        .cwd()
        .to_path_buf();
    state.session_dirty = false;

    state.handle_state_event(StateEvent::TerminalCwdReported {
        pane_id: shepr_test_fixtures::fixed_pane_id(pane_id.raw().saturating_add(1_000_000)),
        cwd: shepr_mux::UsableCwd::new(std::path::PathBuf::from("/")).expect("root is usable"),
    });

    assert_eq!(
        state.terminal(pane_id).expect("test precondition").cwd(),
        &before
    );
    assert!(!state.session_dirty);
}

#[test]
fn toggle_zoom_works() {
    let mut state = app_with_workspaces(&["test"]);
    state.test_split_workspace(0, Direction::Horizontal);

    assert!(!state.ws(0).tree().zoomed());
    toggle_focused_zoom(&mut state);
    assert!(state.ws(0).tree().zoomed());
    toggle_focused_zoom(&mut state);
    assert!(!state.ws(0).tree().zoomed());
}

#[test]
fn toggle_zoom_single_pane_noop() {
    let mut state = app_with_workspaces(&["test"]);
    let pane_id = state.ws(0).tree().focused();
    state.session_dirty = false;

    let outcome = state
        .toggle_pane_zoom(pane_id)
        .expect("a lone pane is a valid target");
    assert!(!outcome.changed);
    let outcome = state
        .toggle_pane_zoom(pane_id)
        .expect("a lone pane is a valid target");
    assert!(!outcome.changed);

    assert!(!state.ws(0).tree().zoomed());
    assert!(!state.session_dirty);
}

#[test]
fn pane_focus_direction_changes_focus_while_zoomed_through_endpoint() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.ws(0).tree().root();
    let right = state.test_split_workspace(0, Direction::Horizontal);
    state.ws_mut(0).focus_pane(root);
    state.ws_mut(0).set_zoomed(true);
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

    let workspace = app.state.ws(0);
    assert!(workspace.tree().zoomed());
    assert_eq!(workspace.tree().focused(), right);
    let visible = app
        .state
        .chrome_in(shepr_core::geometry::Rect::new(0, 0, 100, 20))
        .visible_panes(workspace.tree().layout(), workspace.tree().zoomed());
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, right);
}

#[test]
fn pane_swap_direction_focuses_the_named_source_even_when_another_pane_had_focus() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.ws(0).tree().root();
    let right = state.test_split_workspace(0, Direction::Horizontal);
    state.ws_mut(0).focus_pane(right);
    let mut app = app_from_state(state);
    let pane_id = app.state.pane(root).expect("test precondition").public_id();

    send_endpoint_command(
        &mut app,
        shepr_protocol::command::EndpointCommand::PaneSwap(
            shepr_protocol::command::PaneSwapParams::Direction {
                pane_id,
                direction: shepr_protocol::command::PaneDirection::Right,
            },
        ),
    );

    assert_eq!(app.state.ws(0).tree().focused(), root);
    assert_eq!(
        app.state.ws(0).tree().layout().pane_ids(),
        vec![right, root]
    );
}

#[test]
fn pane_swap_direction_mutates_hidden_layout_while_zoomed_through_endpoint() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.ws(0).tree().root();
    let right = state.test_split_workspace(0, Direction::Horizontal);
    state.ws_mut(0).focus_pane(root);
    state.ws_mut(0).set_zoomed(true);
    let mut app = app_from_state(state);
    let pane_id = app.state.pane(root).expect("test precondition").public_id();

    send_endpoint_command(
        &mut app,
        shepr_protocol::command::EndpointCommand::PaneSwap(
            shepr_protocol::command::PaneSwapParams::Direction {
                pane_id,
                direction: shepr_protocol::command::PaneDirection::Right,
            },
        ),
    );

    assert!(app.state.ws(0).tree().zoomed());
    assert_eq!(app.state.ws(0).tree().focused(), root);
    assert_eq!(
        app.state.ws(0).tree().layout().pane_ids(),
        vec![right, root]
    );
}

#[test]
fn close_pane_removes_from_workspace() {
    let mut state = app_with_workspaces(&["test"]);
    let closed = state.test_split_workspace(0, Direction::Horizontal);
    assert_eq!(state.ws(0).tree().len(), 2);
    assert!(state.remove_pane(closed).is_some());
    assert_eq!(state.ws(0).tree().len(), 1);
}

#[test]
fn pane_process_exit_publish_marks_agent_idle_before_pane_removal() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.seed_bookmark_index(Some(1));
    let pane_id = state.ws(0).tree().root();
    state
        .terminal_mut(pane_id)
        .set_detected_state(Some(Agent::Pi), AgentState::Working);
    assert_eq!(
        state
            .terminal(pane_id)
            .expect("test precondition")
            .ownership()
            .state(),
        AgentState::Working
    );

    assert!(
        state.publish_pane_process_exit(
            pane_id,
            shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Exited),
            std::time::Instant::now()
        ),
        "the exit releases the agent"
    );

    assert_eq!(
        state
            .terminal(pane_id)
            .expect("test precondition")
            .ownership()
            .state(),
        AgentState::Idle
    );
}

#[test]
fn close_pane_takes_the_panes_terminal_state_with_it() {
    let mut state = app_with_workspaces(&["test"]);
    let pane_id = state.test_split_workspace(0, Direction::Horizontal);
    assert!(state.terminal(pane_id).is_some());

    let outcome = state.remove_pane(pane_id).expect("the pane is removed");

    assert!(state.terminal(pane_id).is_none());
    assert_eq!(outcome.removed, vec![pane_id]);
}

#[test]
fn close_workspace_keeps_the_bookmarked_workspace() {
    let mut state = app_with_workspaces(&["a", "b", "c", "d"]);
    let bookmarked_id = state.ws(3).id();
    state.seed_bookmark_index(Some(3));
    let first = state.ws(0).id();

    state.close_workspace(&first);

    assert_eq!(
        state.ws(state.bookmark_index().expect("bookmarked")).id(),
        bookmarked_id
    );
}

#[test]
fn close_workspace_of_an_unknown_id_is_a_noop() {
    let mut state = app_with_workspaces(&["a"]);
    state.session_dirty = false;
    let unknown = shepr_protocol::WorkspaceId::from_number(9_999).expect("nonzero number");

    assert!(state.close_workspace(&unknown).is_none());

    assert_eq!(state.workspaces.len(), 1);
    assert!(!state.session_dirty);
}

#[test]
fn close_workspace_takes_its_panes_terminal_states_with_it() {
    let mut state = app_with_workspaces(&["one", "two"]);
    let pane_id = state.ws(0).tree().root();
    assert!(state.terminal(pane_id).is_some());
    let closing = state.ws(0).id();
    let outcome = state
        .close_workspace(&closing)
        .expect("the workspace closes");

    assert!(state.terminal(pane_id).is_none());
    assert_eq!(outcome.removed, vec![pane_id]);
}

#[test]
fn close_pane_last_pane_closes_the_panes_own_workspace_not_the_bookmarked_one() {
    let mut state = app_with_workspaces(&["other", "closing"]);
    state.seed_bookmark_index(Some(0));

    let pane_id = state.ws(1).tree().root();
    let outcome = state.remove_pane(pane_id).expect("the pane is removed");

    assert_eq!(outcome.scope, PaneRemovalScope::Workspace);
    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.ws(0).name(), "other");
    assert!(state.terminal(pane_id).is_none());
}

#[test]
fn removing_a_pane_reports_whether_focus_moved() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.ws(0).tree().root();
    let second = state.test_split_workspace(0, Direction::Horizontal);
    assert_eq!(state.ws(0).tree().focused(), second);

    let outcome = state.remove_pane(root).expect("the root pane is removed");
    assert_eq!(outcome.scope, PaneRemovalScope::Pane);
    assert!(!outcome.focus_changed);

    let outcome = state.remove_pane(second);
    assert_eq!(
        outcome.map(|outcome| outcome.scope),
        Some(PaneRemovalScope::Workspace)
    );
}

// These call reducers directly: endpoint effects and the periodic /proc
// refresh cannot supply missing invalidation for them.
#[test]
fn projected_reducers_invalidate_without_an_endpoint_caller() {
    let mut state = app_with_workspaces(&["one", "two"]);
    let workspace = state.ws(0).id();
    let root = state.ws(0).tree().root();
    let split = state.test_split_workspace(0, Direction::Horizontal);

    fn advances(state: &mut AppState, apply: impl FnOnce(&mut AppState)) {
        let before = state.shell_projection_revision();
        apply(state);
        assert_ne!(state.shell_projection_revision(), before);
    }

    advances(&mut state, |state| {
        assert_eq!(state.focus_pane(root), ViewMutation::Focus);
    });
    advances(&mut state, |state| {
        assert_eq!(
            state.set_pane_input(root, true),
            Some(ViewMutation::Metadata)
        );
    });
    advances(&mut state, |state| {
        assert_eq!(
            state.rename_pane(
                root,
                Some(shepr_mux::terminal::Label::new("renamed").expect("label"))
            ),
            Some(ViewMutation::Metadata)
        );
    });
    advances(&mut state, |state| {
        assert_eq!(
            state.rename_workspace(
                &workspace,
                shepr_mux::terminal::Label::new("renamed").expect("label")
            ),
            Some(ViewMutation::Metadata)
        );
    });
    advances(&mut state, |state| {
        assert_eq!(
            state.move_workspace(&workspace, None),
            ViewMutation::WorkspaceOrder
        );
    });
    advances(&mut state, |state| {
        assert!(state.swap_panes(root, split).changed());
    });
    advances(&mut state, |state| {
        assert!(state.toggle_pane_zoom(root).expect("pane").changed);
    });
    advances(&mut state, |state| {
        assert!(state.remove_pane(split).is_some());
    });
    advances(&mut state, |state| {
        assert!(state.close_workspace(&workspace).is_some());
    });
}

#[test]
fn unchanged_projected_reducers_keep_the_revision() {
    let mut state = app_with_workspaces(&["one"]);
    let workspace = state.ws(0).id();
    let pane = state.ws(0).tree().root();
    let before = state.shell_projection_revision();
    assert_eq!(state.focus_pane(pane), ViewMutation::Unchanged);
    assert_eq!(
        state.set_pane_input(pane, false),
        Some(ViewMutation::Unchanged)
    );
    assert_eq!(state.rename_pane(pane, None), Some(ViewMutation::Unchanged));
    assert_eq!(
        state.rename_workspace(
            &workspace,
            shepr_mux::terminal::Label::new("one").expect("label")
        ),
        Some(ViewMutation::Unchanged)
    );
    assert_eq!(
        state.move_workspace(&workspace, None),
        ViewMutation::Unchanged
    );
    assert!(!state.toggle_pane_zoom(pane).expect("pane").changed);
    assert!(!state.apply_workspace_git_statuses(Vec::new()));
    assert_eq!(state.shell_projection_revision(), before);
}
