use super::*;
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
    state.ensure_test_terminals();
    if !state.workspaces.is_empty() {
        state.set_bookmark_index(Some(0));
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
        monotonic: state.clock_now,
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

fn app_from_state(state: AppState) -> crate::app::App {
    let mut app = crate::app::App::new(
        &shepr_config::ServerConfig::default(),
        crate::app::AppPolicy::Suspended,
    );
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
    let pane_id = state.workspaces[ws_idx].focused_pane_id();
    state
        .toggle_pane_zoom(ws_idx, pane_id)
        .expect("test precondition");
}

#[test]
fn pane_removal_command_returns_the_removed_container_scope() {
    let mut state = app_with_workspaces(&["one"]);
    let second_pane = state.test_split_workspace(0, Direction::Horizontal);
    state.ensure_test_terminals();
    let first_pane = state.workspaces[0].root_pane();

    let plan = state
        .prepare_pane_removal(0, first_pane)
        .expect("test precondition");
    let PaneRemovalCommit::Removed(outcome) = state.commit_pane_removal(&plan) else {
        panic!("prepared pane removal must commit");
    };
    assert_eq!(outcome.removal.scope, PaneRemovalScope::Pane);
    assert_eq!(state.workspaces[0].pane_count(), 1);
    assert!(state.workspaces[0].contains_pane(second_pane));

    let plan = state
        .prepare_pane_removal(0, second_pane)
        .expect("test precondition");
    let PaneRemovalCommit::Removed(outcome) = state.commit_pane_removal(&plan) else {
        panic!("final pane removal must commit");
    };
    assert_eq!(outcome.removal.scope, PaneRemovalScope::Workspace);
    assert!(state.workspaces.is_empty());
    state.assert_invariants_for_test();
}

#[test]
fn pane_split_state_command_commits_prepared_geometry_and_terminal() {
    let mut state = app_with_workspaces(&["one"]);
    let root_pane = state.workspaces[0].root_pane();
    assert_eq!(state.workspaces[0].pane_count(), 1);
    let chrome = state.pane_geometry_in(ratatui::layout::Rect::new(0, 0, 80, 24));
    let prepared = state.workspaces[0]
        .prepare_split(
            root_pane,
            Direction::Horizontal,
            &chrome,
            None,
            std::path::PathBuf::from("/shepr-test/cwd"),
            true,
        )
        .expect("test precondition");
    let new_pane = prepared.pane_id();
    let terminal_id = prepared.terminal().id.clone();
    // The public ID a launch would export is the one the pane is registered
    // under.
    let reserved = prepared.public_id().number();
    let outcome = state
        .commit_pane_split(0, prepared)
        .expect("prepared pane split commits");

    assert_eq!(
        state.workspaces[0].public_pane_number(new_pane),
        Some(reserved)
    );
    assert_eq!(
        state
            .terminals
            .get(&terminal_id)
            .map(|terminal| terminal.cwd().to_path_buf()),
        Some(std::path::PathBuf::from("/shepr-test/cwd"))
    );
    assert_eq!(outcome.pane_id, new_pane);
    assert_eq!(outcome.terminal_id, terminal_id);
    assert_eq!(state.workspaces[0].pane_count(), 2);
    assert!(
        !state.workspaces[0]
            .pane_state(new_pane)
            .expect("committed pane is present")
            .right_click_passthrough
    );
    assert!(state.terminals.contains_key(&terminal_id));
    assert_eq!(state.workspaces[0].focused_pane_id(), new_pane);
    state.assert_invariants_for_test();
}

#[test]
fn workspace_creation_state_command_commits_spawned_values() {
    let mut state = AppState::test_new();
    let workspace = Workspace::test_new("created");
    let root_pane = workspace.root_pane();
    let terminal_id = workspace
        .terminal_id(root_pane)
        .expect("test precondition")
        .clone();
    let terminal = shepr_mux::terminal::TerminalState::new(
        terminal_id.clone(),
        std::path::PathBuf::from("/shepr-test/cwd"),
    );

    let outcome = state.commit_workspace_creation(workspace, terminal);

    assert_eq!(outcome.workspace_index, 0);
    assert_eq!(outcome.root_pane, root_pane);
    // Creation moves nobody: navigation is per client, and the bookmark follows
    // only an active client's navigation.
    assert_eq!(state.bookmark_index(), None);
    assert!(state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn apply_workspace_git_statuses_updates_matching_workspace() {
    let mut state = app_with_workspaces(&["one", "two"]);
    let first_id = state.workspaces[0].id;
    let first_cwd = state.workspaces[0]
        .resolved_identity_cwd()
        .expect("test precondition");
    let second_id = state.workspaces[1].id;

    let changed = state.apply_workspace_git_statuses(vec![(
        WorkspaceGitStatus {
            workspace_id: first_id,
            resolved_identity_cwd: first_cwd.clone(),
            status_cache_key: shepr_mux::git::GitStatusKey::Checkout(first_cwd.clone()),
            auto_label: "one".into(),
            branch: shepr_mux::git::WorkspaceBranch::Named("main".into()),
            ahead_behind: Some(shepr_mux::git::AheadBehind {
                ahead: 2,
                behind: 1,
            }),
        },
        Some(first_cwd),
    )]);

    assert!(changed);
    assert_eq!(state.workspaces[0].branch().as_deref(), Some("main"));
    assert_eq!(
        state.workspaces[0].git_ahead_behind(),
        Some(shepr_mux::git::AheadBehind {
            ahead: 2,
            behind: 1
        })
    );
    assert_eq!(state.workspaces[1].id, second_id);
    assert_eq!(state.workspaces[1].git_ahead_behind(), None);
}

#[test]
fn apply_workspace_git_statuses_ignores_stale_cwd() {
    let mut state = app_with_workspaces(&["one"]);
    let workspace_id = state.workspaces[0].id;
    let cwd = state.workspaces[0].identity_cwd.clone();
    state.workspaces[0].admit_git_status(
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: shepr_mux::git::GitStatusKey::Checkout(cwd.clone()),
            auto_label: "one".into(),
            branch: shepr_mux::git::WorkspaceBranch::Named("old".into()),
            ahead_behind: Some(shepr_mux::git::AheadBehind {
                ahead: 1,
                behind: 0,
            }),
        },
        Some(&cwd),
    );

    let current_cwd = state.workspaces[0]
        .resolved_identity_cwd()
        .expect("test precondition");
    let changed = state.apply_workspace_git_statuses(vec![(
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: std::path::PathBuf::from("/definitely/not/current"),
            status_cache_key: shepr_mux::git::GitStatusKey::Checkout(std::path::PathBuf::from(
                "/definitely/not/current",
            )),
            auto_label: "stale".into(),
            branch: shepr_mux::git::WorkspaceBranch::Named("main".into()),
            ahead_behind: Some(shepr_mux::git::AheadBehind {
                ahead: 0,
                behind: 1,
            }),
        },
        Some(current_cwd),
    )]);

    assert!(!changed);
    assert_eq!(state.workspaces[0].branch().as_deref(), Some("old"));
    assert_eq!(
        state.workspaces[0].git_ahead_behind(),
        Some(shepr_mux::git::AheadBehind {
            ahead: 1,
            behind: 0
        })
    );
}

#[test]
fn apply_workspace_git_statuses_clears_missing_git_status() {
    let mut state = app_with_workspaces(&["one"]);
    let workspace_id = state.workspaces[0].id;
    let cwd = state.workspaces[0]
        .resolved_identity_cwd()
        .expect("test precondition");
    state.workspaces[0].admit_git_status(
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: shepr_mux::git::GitStatusKey::Checkout(cwd.clone()),
            auto_label: "one".into(),
            branch: shepr_mux::git::WorkspaceBranch::Named("main".into()),
            ahead_behind: Some(shepr_mux::git::AheadBehind {
                ahead: 1,
                behind: 2,
            }),
        },
        Some(&cwd),
    );

    let changed = state.apply_workspace_git_statuses(vec![(
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: shepr_mux::git::GitStatusKey::Outside(cwd.clone()),
            auto_label: "one".into(),
            branch: shepr_mux::git::WorkspaceBranch::OutsideRepository,
            ahead_behind: None,
        },
        Some(cwd),
    )]);

    assert!(changed);
    assert_eq!(state.workspaces[0].branch(), None);
    assert_eq!(state.workspaces[0].git_ahead_behind(), None);
}

#[test]
fn set_bookmark_moves_it_and_saves_only_when_it_moved() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.session_dirty = false;
    let third = state.workspaces[2].id;

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
    let bookmarked_id = state.workspaces[1].id;
    state.set_bookmark_index(Some(1));

    state.move_workspace(1, 0);

    let names: Vec<_> = state
        .workspaces
        .iter()
        .map(shepr_mux::workspace::Workspace::display_name)
        .collect();
    assert_eq!(names, vec!["b", "a", "c"]);
    assert_eq!(state.bookmark_index(), Some(0));
    assert_eq!(state.bookmark.as_ref(), Some(&bookmarked_id));
    state.assert_invariants_for_test();
}

#[test]
fn the_bookmark_survives_reorder_and_removal_of_other_workspaces() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.set_bookmark_index(Some(1));
    let bookmarked_id = state.bookmark;

    assert!(state.move_workspace(1, 0));
    assert_eq!(state.bookmark, bookmarked_id);
    assert_eq!(state.bookmark_index(), Some(0));

    state.close_workspace_at(1).expect("background workspace");
    assert_eq!(state.bookmark, bookmarked_id);
    assert_eq!(state.bookmark_index(), Some(0));
    state.assert_invariants_for_test();
}

#[test]
fn move_workspace_accepts_insert_at_end() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);

    state.move_workspace(0, state.workspaces.len());

    let names: Vec<_> = state
        .workspaces
        .iter()
        .map(shepr_mux::workspace::Workspace::display_name)
        .collect();
    assert_eq!(names, vec!["b", "c", "a"]);
}

#[test]
fn closing_the_bookmarked_workspace_moves_the_bookmark_to_the_one_now_at_its_index() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.set_bookmark_index(Some(1));
    state.session_dirty = false;

    state.close_workspace_at(1);

    assert_eq!(state.workspaces.len(), 2);
    assert_eq!(state.bookmark_index(), Some(1));
    assert_eq!(state.workspaces[1].custom_name.as_deref(), Some("c"));
    assert!(state.session_dirty);
}

#[test]
fn closing_the_last_workspace_clears_the_bookmark() {
    let mut state = app_with_workspaces(&["only"]);
    state.close_workspace_at(0);

    assert!(state.workspaces.is_empty());
    assert_eq!(state.bookmark_index(), None);
    assert_eq!(state.bookmark, None);
}

#[test]
fn closing_the_bookmarked_workspace_at_the_end_clamps_the_bookmark() {
    let mut state = app_with_workspaces(&["a", "b"]);
    state.set_bookmark_index(Some(1));

    state.close_workspace_at(1);

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.bookmark_index(), Some(0));
}

#[test]
fn closing_another_workspace_keeps_the_bookmark() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.set_bookmark_index(Some(0));

    state.close_workspace_at(1);

    assert_eq!(state.workspaces.len(), 2);
    assert_eq!(state.workspaces[0].display_name(), "a");
    assert_eq!(state.workspaces[1].display_name(), "c");
    assert_eq!(state.bookmark_index(), Some(0));
    state.assert_invariants_for_test();
}

#[test]
fn state_changed_updates_pane() {
    let mut state = app_with_workspaces(&["test"]);
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");

    state.handle_app_event(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Pi),
        detection: shepr_detect::Detection::new(AgentState::Working, false),
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });

    let terminal_id = state.workspaces[0]
        .panes()
        .get(&pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    let terminal = state
        .terminals
        .get(&terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Working);
    assert_eq!(terminal.ownership().detected_agent(), Some(Agent::Pi));
}

#[test]
fn state_changed_events_advance_the_agent_state_change_sequence() {
    let mut app = app_with_workspaces(&["active", "background"]);
    let pane_id = app.workspaces[1].root_pane();
    let terminal_id = app.workspaces[1].panes()[&pane_id]
        .attached_terminal_id
        .clone();

    for (sequence, state) in [(1, AgentState::Working), (2, AgentState::Idle)] {
        app.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            detection: shepr_detect::Detection::new(state, false),
            process_exited: false,
            observed_at: Instant::now(),
        });
        assert_eq!(
            app.terminals[&terminal_id]
                .ownership()
                .last_agent_state_change_seq(),
            Some(sequence)
        );
    }
    app.assert_invariants_for_test();
}

#[test]
fn agent_state_change_sequence_ignores_idle_unknown_presentation_changes() {
    let mut app = app_with_workspaces(&["active"]);
    let pane_id = app.workspaces[0].root_pane();
    let terminal_id = app.workspaces[0].panes()[&pane_id]
        .attached_terminal_id
        .clone();
    let state_changed = |state| AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Pi),
        detection: shepr_detect::Detection::new(state, false),
        process_exited: false,
        observed_at: Instant::now(),
    };

    app.handle_app_event(state_changed(AgentState::Idle));
    assert_eq!(
        app.terminals[&terminal_id].ownership().state(),
        AgentState::Idle
    );
    assert_eq!(
        app.terminals[&terminal_id]
            .ownership()
            .last_agent_state_change_seq(),
        None
    );

    app.handle_app_event(state_changed(AgentState::Unknown));
    assert_eq!(
        app.terminals[&terminal_id].ownership().state(),
        AgentState::Unknown
    );
    assert_eq!(
        app.terminals[&terminal_id]
            .ownership()
            .last_agent_state_change_seq(),
        None
    );

    app.handle_app_event(state_changed(AgentState::Working));
    assert_eq!(
        app.terminals[&terminal_id]
            .ownership()
            .last_agent_state_change_seq(),
        Some(1)
    );
}

#[test]
fn visible_blocker_overrides_hook_working() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.set_bookmark_index(Some(0));
    let bg_pane_id = *state.workspaces[1]
        .panes()
        .keys()
        .next()
        .expect("test precondition");
    let bg_terminal_id = state.workspaces[1]
        .panes()
        .get(&bg_pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();

    state.handle_app_event(AppEvent::StateChanged {
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
    state.handle_app_event(AppEvent::StateChanged {
        pane_id: bg_pane_id,
        agent: Some(Agent::Codex),
        detection: shepr_detect::Detection::new(AgentState::Blocked, true),

        process_exited: false,
        observed_at: std::time::Instant::now(),
    });

    let terminal = state
        .terminals
        .get(&bg_terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Blocked);
}

#[test]
fn reserved_native_state_report_does_not_override_screen_state() {
    let mut state = app_with_workspaces(&["active"]);
    state.set_bookmark_index(Some(0));
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .panes()
        .get(&pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();

    state.handle_app_event(AppEvent::StateChanged {
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
    let terminal = state
        .terminals
        .get(&terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Working);
    assert!(terminal.ownership().hook_authority().is_none());
    assert!(terminal.ownership().persisted_agent_session().is_some());

    state.handle_app_event(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Claude),
        detection: shepr_detect::Detection::new(AgentState::Idle, false),
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });

    let terminal = state
        .terminals
        .get(&terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Idle);
}

#[test]
fn devin_state_report_refreshes_session_without_overriding_screen_state() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .panes()
        .get(&pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();

    state.handle_app_event(AppEvent::StateChanged {
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

    let terminal = state
        .terminals
        .get(&terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.ownership().state(), AgentState::Idle);
    assert!(terminal.ownership().hook_authority().is_none());
    assert!(terminal.ownership().persisted_agent_session().is_some());
}

#[test]
fn session_ref_only_update_marks_session_dirty_without_visible_update() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");

    state.handle_app_event(AppEvent::StateChanged {
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
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .pane_state(pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    let scratch = crate::test_support::ScratchDir::new("cwd-report");
    let cwd = scratch.to_path_buf();
    state.session_dirty = false;

    let update = state.handle_app_event(AppEvent::TerminalCwdReported {
        pane_id,
        cwd: shepr_mux::UsableCwd::new(cwd.clone()).expect("test cwd is usable"),
    });

    assert_eq!(update, StateUpdate::Unchanged);
    assert_eq!(
        state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .cwd(),
        cwd
    );
    assert!(state.session_dirty);
}

#[test]
fn cwd_report_for_missing_pane_is_ignored() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .pane_state(pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    let before = state.terminals[&terminal_id].cwd().to_path_buf();
    state.session_dirty = false;

    state.handle_app_event(AppEvent::TerminalCwdReported {
        pane_id: shepr_test_fixtures::fixed_pane_id(pane_id.raw().saturating_add(1_000_000)),
        cwd: shepr_mux::UsableCwd::new(std::path::PathBuf::from("/")).expect("root is usable"),
    });

    assert_eq!(state.terminals[&terminal_id].cwd(), before);
    assert!(!state.session_dirty);
}

#[test]
fn toggle_zoom_works() {
    let mut state = app_with_workspaces(&["test"]);
    state.test_split_workspace(0, Direction::Horizontal);

    assert!(!state.workspaces[0].zoomed());
    toggle_focused_zoom(&mut state);
    assert!(state.workspaces[0].zoomed());
    toggle_focused_zoom(&mut state);
    assert!(!state.workspaces[0].zoomed());
}

#[test]
fn toggle_zoom_single_pane_noop() {
    let mut state = app_with_workspaces(&["test"]);
    let pane_id = state.workspaces[0].focused_pane_id();
    state.session_dirty = false;

    let outcome = state
        .toggle_pane_zoom(0, pane_id)
        .expect("a lone pane is a valid target");
    assert!(!outcome.changed);
    let outcome = state
        .toggle_pane_zoom(0, pane_id)
        .expect("a lone pane is a valid target");
    assert!(!outcome.changed);

    assert!(!state.workspaces[0].zoomed());
    assert!(!state.session_dirty);
}

#[test]
fn pane_focus_direction_changes_focus_while_zoomed_through_endpoint() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].root_pane();
    let right = state.test_split_workspace(0, Direction::Horizontal);
    state.workspaces[0].focus_pane(root);
    state.workspaces[0].set_zoomed(true);
    let mut app = app_from_state(state);
    let pane_id = app.public_pane_id(0, root).expect("test precondition");

    send_endpoint_command(
        &mut app,
        shepr_protocol::command::EndpointCommand::PaneFocusDirection(
            shepr_protocol::command::PaneFocusDirectionParams {
                pane_id,
                direction: shepr_protocol::command::PaneDirection::Right,
            },
        ),
    );

    let workspace = &app.state.workspaces[0];
    assert!(workspace.zoomed());
    assert_eq!(workspace.focused_pane_id(), right);
    let visible = app
        .state
        .pane_geometry_in(ratatui::layout::Rect::new(0, 0, 100, 20))
        .visible_panes(workspace.layout(), workspace.zoomed());
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, right);
}

#[test]
fn pane_swap_direction_focuses_the_named_source_even_when_another_pane_had_focus() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].root_pane();
    let right = state.test_split_workspace(0, Direction::Horizontal);
    state.workspaces[0].focus_pane(right);
    let mut app = app_from_state(state);
    let pane_id = app.public_pane_id(0, root).expect("test precondition");

    send_endpoint_command(
        &mut app,
        shepr_protocol::command::EndpointCommand::PaneSwap(
            shepr_protocol::command::PaneSwapParams::Direction {
                pane_id,
                direction: shepr_protocol::command::PaneDirection::Right,
            },
        ),
    );

    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);
    assert_eq!(
        app.state.workspaces[0].layout().pane_ids(),
        vec![right, root]
    );
}

#[test]
fn pane_swap_direction_mutates_hidden_layout_while_zoomed_through_endpoint() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].root_pane();
    let right = state.test_split_workspace(0, Direction::Horizontal);
    state.workspaces[0].focus_pane(root);
    state.workspaces[0].set_zoomed(true);
    let mut app = app_from_state(state);
    let pane_id = app.public_pane_id(0, root).expect("test precondition");

    send_endpoint_command(
        &mut app,
        shepr_protocol::command::EndpointCommand::PaneSwap(
            shepr_protocol::command::PaneSwapParams::Direction {
                pane_id,
                direction: shepr_protocol::command::PaneDirection::Right,
            },
        ),
    );

    assert!(app.state.workspaces[0].zoomed());
    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);
    assert_eq!(
        app.state.workspaces[0].layout().pane_ids(),
        vec![right, root]
    );
}

#[test]
fn close_pane_removes_from_workspace() {
    let mut state = app_with_workspaces(&["test"]);
    let closed = state.test_split_workspace(0, Direction::Horizontal);
    state.ensure_test_terminals();
    assert_eq!(state.workspaces[0].panes().len(), 2);
    let plan = state
        .prepare_pane_removal(0, closed)
        .expect("test precondition");
    assert!(matches!(
        state.commit_pane_removal(&plan),
        PaneRemovalCommit::Removed(_)
    ));
    assert_eq!(state.workspaces[0].panes().len(), 1);
    state.assert_invariants_for_test();
}

#[test]
fn pane_process_exit_publish_marks_agent_idle_before_pane_removal() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.set_bookmark_index(Some(1));
    state.ensure_test_terminals();
    let pane_id = state.workspaces[0].root_pane();
    let terminal_id = state
        .terminal_id_for_pane(0, pane_id)
        .expect("test precondition");
    state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .set_detected_state(Some(Agent::Pi), AgentState::Working);
    assert_eq!(
        state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .ownership()
            .state(),
        AgentState::Working
    );

    assert!(
        state.publish_pane_process_exit(
            pane_id,
            shepr_platform::ChildExitReason::Exited,
            std::time::Instant::now()
        ),
        "the exit releases the agent"
    );

    assert_eq!(
        state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .ownership()
            .state(),
        AgentState::Idle
    );
}

#[test]
fn close_pane_removes_unattached_terminal_state() {
    let mut state = app_with_workspaces(&["test"]);
    let pane_id = state.test_split_workspace(0, Direction::Horizontal);
    state.ensure_test_terminals();
    let terminal_id = state
        .terminal_id_for_pane(0, pane_id)
        .expect("test precondition");

    let plan = state
        .prepare_pane_removal(0, pane_id)
        .expect("test precondition");
    assert!(matches!(
        state.commit_pane_removal(&plan),
        PaneRemovalCommit::Removed(_)
    ));

    assert!(!state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn close_workspace_at_keeps_the_bookmarked_workspace() {
    let mut state = app_with_workspaces(&["a", "b", "c", "d"]);
    let bookmarked_id = state.workspaces[3].id;
    state.set_bookmark_index(Some(3));

    state.close_workspace_at(0);

    assert_eq!(
        state.workspaces[state.bookmark_index().expect("bookmarked")].id,
        bookmarked_id
    );
    state.assert_invariants_for_test();
}

#[test]
fn close_workspace_at_out_of_range_is_a_noop() {
    let mut state = app_with_workspaces(&["a"]);
    state.session_dirty = false;

    state.close_workspace_at(3);

    assert_eq!(state.workspaces.len(), 1);
    assert!(!state.session_dirty);
}

#[test]
fn close_workspace_removes_unattached_terminal_states() {
    let mut state = app_with_workspaces(&["one", "two"]);
    let pane_id = state.workspaces[0].root_pane();
    let terminal_id = state
        .terminal_id_for_pane(0, pane_id)
        .expect("test precondition");
    let _ = pane_id;
    state.close_workspace_at(0);

    assert!(!state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn close_pane_last_pane_closes_the_panes_own_workspace_not_the_bookmarked_one() {
    let mut state = app_with_workspaces(&["other", "closing"]);
    let active_terminal_id = state
        .terminal_id_for_pane(1, state.workspaces[1].root_pane())
        .expect("test precondition");
    state.set_bookmark_index(Some(0));

    let pane_id = state.workspaces[1].root_pane();
    let plan = state
        .prepare_pane_removal(1, pane_id)
        .expect("test precondition");
    assert!(matches!(
        state.commit_pane_removal(&plan),
        PaneRemovalCommit::Removed(_)
    ));

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].display_name(), "other");
    assert!(!state.terminals.contains_key(&active_terminal_id));
    state.assert_invariants_for_test();
}
