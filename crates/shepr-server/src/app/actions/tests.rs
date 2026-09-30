use super::*;
use crate::test_support::*;
use ratatui::layout::Rect;
use shepr_agent::detect::{Agent, AgentState};
use shepr_core::layout::Direction;
use shepr_mux::workspace::Workspace;
use std::time::Instant;

fn app_with_workspaces(names: &[&str]) -> AppState {
    let mut state = AppState::test_new();
    for name in names {
        let ws = Workspace::test_new(name);
        state.workspaces.push(ws);
    }
    state.ensure_test_terminals();
    if !state.workspaces.is_empty() {
        state.set_active_index(Some(0));
        state.mode = Mode::Terminal;
    }
    state
}

/// Records `area` as every workspace's layout area and returns the focused
/// workspace's visible panes laid out in it.
fn test_view(state: &mut AppState, area: Rect) -> Vec<shepr_mux::workspace::PaneChromeInfo> {
    state.test_record_all_workspace_areas(area);
    state
        .active_index()
        .and_then(|ws_idx| state.workspaces.get(ws_idx))
        .map_or_default(|workspace| {
            state
                .pane_geometry_in(area)
                .visible_panes(workspace.layout(), workspace.zoomed())
        })
        .into_iter()
        .map(|mut pane| {
            pane.inner_rect = shepr_mux::workspace::pane_inner_rect(pane.rect, pane.borders);
            pane
        })
        .collect()
}

fn toggle_focused_zoom(state: &mut AppState) {
    let ws_idx = state.active_index().expect("test precondition");
    let pane_id = state.workspaces[ws_idx].focused_pane_id();
    state
        .apply_pane_zoom(ws_idx, pane_id, PaneZoomCommand::Toggle)
        .expect("test precondition");
}

#[test]
fn pane_context_resolver_applies_explicit_and_creation_targets() {
    let mut state = app_with_workspaces(&["active", "selected"]);
    let selected_pane = state.workspaces[1].focused_pane_id();
    state.set_active_index(Some(0));
    state.set_selected_index(Some(1));
    state.mode = Mode::Navigate;

    let explicit = state
        .resolve_pane_context(
            Some((1, selected_pane)),
            None,
            PaneContextFallback::ActiveWorkspace,
        )
        .expect("explicit pane target");
    assert_eq!(explicit.workspace_index, 1);
    assert_eq!(explicit.pane_id, selected_pane);

    let creation = state
        .resolve_pane_context(None, None, PaneContextFallback::WorkspaceCreation)
        .expect("selected workspace fallback");
    assert_eq!(creation.workspace_index, 1);

    let active = state
        .resolve_pane_context(None, None, PaneContextFallback::ActiveWorkspace)
        .expect("active workspace fallback");
    assert_eq!(active.workspace_index, 0);
}

#[test]
fn pane_removal_command_returns_the_removed_container_scope() {
    let mut state = app_with_workspaces(&["one"]);
    let second_pane = state.workspaces[0].test_split(Direction::Horizontal);
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

    let PaneRemovalCommit::Removed(outcome) = state.remove_pane(0, second_pane) else {
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
    let mut prepared_layout = state.workspaces[0].layout().clone();
    let new_pane = prepared_layout
        .split_pane(root_pane, Direction::Horizontal, 0.5)
        .expect("test precondition");
    assert_eq!(state.workspaces[0].pane_count(), 1);
    let terminal_id = shepr_protocol::TerminalId::alloc();
    let terminal = shepr_mux::terminal::TerminalState::new(
        terminal_id.clone(),
        std::path::PathBuf::from("/shepr-test/cwd"),
    );
    let previous_focus = state.current_pane_focus_target();

    let outcome = state
        .commit_pane_split(
            0,
            new_pane,
            prepared_layout,
            terminal,
            true,
            true,
            previous_focus,
        )
        .expect("prepared pane split commits");

    assert_eq!(outcome.pane_id, new_pane);
    assert_eq!(outcome.terminal_id, terminal_id);
    assert_eq!(state.workspaces[0].pane_count(), 2);
    assert!(
        state.workspaces[0]
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

    let outcome = state.commit_workspace_creation(workspace, terminal, true);

    assert_eq!(outcome.workspace_index, 0);
    assert_eq!(outcome.root_pane, root_pane);
    assert_eq!(state.active_index(), Some(0));
    assert!(state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn apply_workspace_git_statuses_updates_matching_workspace() {
    let mut state = app_with_workspaces(&["one", "two"]);
    let first_id = state.workspaces[0].id.to_string();
    let first_cwd = state.workspaces[0]
        .resolved_identity_cwd()
        .expect("test precondition");
    let second_id = state.workspaces[1].id.to_string();

    let terminal_runtimes = shepr_mux::pane::PaneRuntimeRegistry::new();
    let changed = state.apply_workspace_git_statuses(
        &terminal_runtimes,
        vec![WorkspaceGitStatus {
            workspace_id: first_id,
            resolved_identity_cwd: first_cwd.clone(),
            status_cache_key: first_cwd,
            demand: shepr_mux::git::GitStatusRefreshDemand::ALL,
            auto_label: "one".into(),
            branch: Some("main".into()),
            ahead_behind: Some(shepr_mux::git::AheadBehind {
                ahead: 2,
                behind: 1,
            }),
            space: None,
        }],
    );

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
    let workspace_id = state.workspaces[0].id.to_string();
    state.workspaces[0].cached_git_branch = Some("old".into());
    state.workspaces[0].cached_git_ahead_behind = Some(shepr_mux::git::AheadBehind {
        ahead: 1,
        behind: 0,
    });

    let terminal_runtimes = shepr_mux::pane::PaneRuntimeRegistry::new();
    let changed = state.apply_workspace_git_statuses(
        &terminal_runtimes,
        vec![WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: std::path::PathBuf::from("/definitely/not/current"),
            status_cache_key: std::path::PathBuf::from("/definitely/not/current"),
            demand: shepr_mux::git::GitStatusRefreshDemand::ALL,
            auto_label: "stale".into(),
            branch: Some("main".into()),
            ahead_behind: Some(shepr_mux::git::AheadBehind {
                ahead: 0,
                behind: 1,
            }),
            space: None,
        }],
    );

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
fn apply_workspace_git_statuses_ignores_unrequested_branch_changes() {
    let mut state = app_with_workspaces(&["one"]);
    let workspace_id = state.workspaces[0].id.to_string();
    let cwd = state.workspaces[0]
        .resolved_identity_cwd()
        .expect("test precondition");
    state.workspaces[0].cached_auto_label = "one".into();
    state.workspaces[0].cached_git_branch = Some("old".into());

    let terminal_runtimes = shepr_mux::pane::PaneRuntimeRegistry::new();
    let changed = state.apply_workspace_git_statuses(
        &terminal_runtimes,
        vec![WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: shepr_mux::git::GitStatusRefreshDemand {
                branch: false,
                ahead_behind: true,
            },
            auto_label: "one".into(),
            branch: Some("new".into()),
            ahead_behind: None,
            space: None,
        }],
    );

    assert!(!changed);
    assert_eq!(state.workspaces[0].branch().as_deref(), Some("old"));
}

#[test]
fn apply_workspace_git_statuses_clears_missing_git_status() {
    let mut state = app_with_workspaces(&["one"]);
    let workspace_id = state.workspaces[0].id.to_string();
    let cwd = state.workspaces[0]
        .resolved_identity_cwd()
        .expect("test precondition");
    state.workspaces[0].cached_git_branch = Some("main".into());
    state.workspaces[0].cached_git_ahead_behind = Some(shepr_mux::git::AheadBehind {
        ahead: 1,
        behind: 2,
    });

    let terminal_runtimes = shepr_mux::pane::PaneRuntimeRegistry::new();
    let changed = state.apply_workspace_git_statuses(
        &terminal_runtimes,
        vec![WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: shepr_mux::git::GitStatusRefreshDemand::ALL,
            auto_label: "one".into(),
            branch: None,
            ahead_behind: None,
            space: None,
        }],
    );

    assert!(changed);
    assert_eq!(state.workspaces[0].branch(), None);
    assert_eq!(state.workspaces[0].git_ahead_behind(), None);
}

#[test]
fn switch_workspace_updates_active_and_selected() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.switch_workspace(2);
    assert_eq!(state.active_index(), Some(2));
    assert_eq!(state.selected_index().unwrap_or(0), 2);
}

#[test]
fn switch_workspace_out_of_bounds_is_noop() {
    let mut state = app_with_workspaces(&["a"]);
    state.switch_workspace(5);
    assert_eq!(state.active_index(), Some(0));
}

#[test]
fn move_workspace_reorders_without_changing_logical_selection() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    let active_id = state.workspaces[1].id.to_string();
    let selected_id = state.workspaces[2].id.to_string();
    state.set_active_index(Some(1));
    state.set_selected_index(Some(2));

    state.move_workspace(1, 0);

    let names: Vec<_> = state
        .workspaces
        .iter()
        .map(shepr_mux::workspace::Workspace::display_name)
        .collect();
    assert_eq!(names, vec!["b", "a", "c"]);
    assert_eq!(state.active_index(), Some(0));
    assert_eq!(state.selected_index().unwrap_or(0), 2);
    assert_eq!(
        state.workspaces[state.active_index().expect("test precondition")].id,
        active_id
    );
    assert_eq!(
        state.workspaces[state.selected_index().unwrap_or(0)].id,
        selected_id
    );
}

#[test]
fn active_and_selected_ids_survive_reorder_and_removal() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    assert!(state.switch_workspace(1));
    state.set_selected_index(Some(2));
    let active_id = state.active.clone();
    let selected_id = state.selected.clone();

    assert!(state.move_workspace(1, 0));
    assert_eq!(state.active, active_id);
    assert_eq!(state.selected, selected_id);
    assert_eq!(state.active_index(), Some(0));
    assert_eq!(state.selected_index(), Some(2));

    state.close_workspace_at(1).expect("background workspace");
    assert_eq!(state.active, active_id);
    assert_eq!(state.selected, selected_id);
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
fn close_workspace_adjusts_indices() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.set_selected_index(Some(1));
    state.set_active_index(Some(1));

    state.close_selected_workspace();

    assert_eq!(state.workspaces.len(), 2);
    assert_eq!(state.selected_index().unwrap_or(0), 1);
    assert_eq!(state.active_index(), Some(1));
    assert_eq!(state.workspaces[1].custom_name.as_deref(), Some("c"));
}

#[test]
fn close_last_workspace_clears_active() {
    let mut state = app_with_workspaces(&["only"]);
    state.set_selected_index(Some(0));
    state.close_selected_workspace();

    assert!(state.workspaces.is_empty());
    assert_eq!(state.active_index(), None);
    assert_eq!(state.selected_index().unwrap_or(0), 0);
}

#[test]
fn close_workspace_at_end_adjusts_selected() {
    let mut state = app_with_workspaces(&["a", "b"]);
    state.set_selected_index(Some(1));
    state.set_active_index(Some(1));

    state.close_selected_workspace();

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.selected_index().unwrap_or(0), 0);
    assert_eq!(state.active_index(), Some(0));
}

#[test]
fn close_non_focused_workspace_keeps_focus() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    state.set_selected_index(Some(1));
    state.set_active_index(Some(0));

    state.close_selected_workspace();

    assert_eq!(state.workspaces.len(), 2);
    assert_eq!(state.workspaces[0].display_name(), "a");
    assert_eq!(state.workspaces[1].display_name(), "c");
    assert_eq!(state.selected_index().unwrap_or(0), 0);
    assert_eq!(state.active_index(), Some(0));
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_last_pane_removes_workspace() {
    let mut state = app_with_workspaces(&["a", "b"]);
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");

    let _detached = state.handle_pane_died(pane_id);

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].custom_name.as_deref(), Some("b"));
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_closing_a_workspace_tears_it_down_like_an_explicit_close() {
    let mut state = app_with_workspaces(&["a", "dying", "c"]);
    state.set_active_index(Some(2));
    state.set_selected_index(Some(0));
    let pane_id = state.workspaces[1].root_pane();
    let terminal_id = state
        .terminal_id_for_pane(1, pane_id)
        .expect("test precondition");
    state.session_dirty = false;

    let detached = state.handle_pane_died(pane_id);

    assert_eq!(state.workspaces.len(), 2);
    assert_eq!(
        state.workspaces[state.active_index().expect("active")].display_name(),
        "c"
    );
    assert_eq!(
        state.workspaces[state.selected_index().unwrap_or(0)].display_name(),
        "a"
    );
    assert!(!state.terminals.contains_key(&terminal_id));
    assert_eq!(detached, std::slice::from_ref(&terminal_id));
    assert!(state.session_dirty);
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_last_workspace_enters_navigate() {
    let mut state = app_with_workspaces(&["only"]);
    state.mode = Mode::Terminal;
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");

    let _detached = state.handle_pane_died(pane_id);

    assert!(state.workspaces.is_empty());
    assert_eq!(state.mode, Mode::Navigate);
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_multi_pane_keeps_workspace() {
    let mut state = app_with_workspaces(&["test"]);
    let second_id = state.workspaces[0].test_split(Direction::Horizontal);
    state.ensure_test_terminals();

    let _detached = state.handle_pane_died(second_id);

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].panes().len(), 1);
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_unknown_pane_is_noop() {
    let mut state = app_with_workspaces(&["test"]);
    let fake_id = shepr_test_fixtures::fixed_pane_id(9999);

    assert!(state.handle_pane_died(fake_id).is_empty());

    assert_eq!(state.workspaces.len(), 1);
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
        state: AgentState::Working,
        visible_blocker: false,
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
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(terminal.detected_agent, Some(Agent::Pi));
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
            state,
            visible_blocker: false,
            process_exited: false,
            observed_at: Instant::now(),
        });
        assert_eq!(
            app.terminals[&terminal_id].last_agent_state_change_seq,
            Some(sequence)
        );
    }
    app.assert_invariants_for_test();
}

#[test]
fn visible_blocker_overrides_hook_working() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.set_active_index(Some(0));
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
        state: AgentState::Idle,
        visible_blocker: false,
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });
    state.handle_app_event(AppEvent::HookStateReported {
        pane_id: bg_pane_id,
        source: "shepr:codex".into(),
        agent_label: "codex".into(),
        state: AgentState::Working,
        message: None,
        seq: Some(1),
        session_ref: None,
    });
    state.handle_app_event(AppEvent::StateChanged {
        pane_id: bg_pane_id,
        agent: Some(Agent::Codex),
        state: AgentState::Blocked,
        visible_blocker: true,

        process_exited: false,
        observed_at: std::time::Instant::now(),
    });

    let terminal = state
        .terminals
        .get(&bg_terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.state, AgentState::Blocked);
}

#[test]
fn reserved_native_state_report_does_not_override_screen_state() {
    let mut state = app_with_workspaces(&["active"]);
    state.set_active_index(Some(0));
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
        state: AgentState::Working,
        visible_blocker: false,
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });
    state.handle_app_event(AppEvent::HookStateReported {
        pane_id,
        source: "shepr:claude".into(),
        agent_label: "claude".into(),
        state: AgentState::Blocked,
        message: None,
        seq: Some(1),
        session_ref: shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
    });
    let terminal = state
        .terminals
        .get(&terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.state, AgentState::Working);
    assert!(terminal.hook_authority.is_none());
    assert!(terminal.persisted_agent_session.is_some());

    state.handle_app_event(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Claude),
        state: AgentState::Idle,
        visible_blocker: false,
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });

    let terminal = state
        .terminals
        .get(&terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.state, AgentState::Idle);
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
        state: AgentState::Idle,
        visible_blocker: false,
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });
    state.handle_app_event(AppEvent::HookStateReported {
        pane_id,
        source: "shepr:devin".into(),
        agent_label: "devin".into(),
        state: AgentState::Working,
        message: None,
        seq: Some(1),
        session_ref: shepr_agent::agent::resume::AgentSessionRef::id("devin-session"),
    });

    let terminal = state
        .terminals
        .get(&terminal_id)
        .expect("test precondition");
    assert_eq!(terminal.state, AgentState::Idle);
    assert!(terminal.hook_authority.is_none());
    assert!(terminal.persisted_agent_session.is_some());
}

#[test]
fn hidden_custom_session_ref_only_update_marks_session_dirty_without_visible_update() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes()
        .keys()
        .next()
        .expect("test precondition");
    let test_dir = ScratchDir::new("custom-session-refs");
    let first_session = test_dir.path().join("one.jsonl").display().to_string();
    let second_session = test_dir.path().join("two.jsonl").display().to_string();

    let first_update = state.handle_app_event(AppEvent::HookStateReported {
        pane_id,
        source: "custom:pi".into(),
        agent_label: "pi".into(),
        state: AgentState::Working,
        message: None,
        seq: Some(20),
        session_ref: shepr_agent::agent::resume::AgentSessionRef::path(first_session),
    });
    assert_eq!(first_update, StateUpdate::Changed);
    state.session_dirty = false;

    let second_update = state.handle_app_event(AppEvent::HookStateReported {
        pane_id,
        source: "custom:pi".into(),
        agent_label: "pi".into(),
        state: AgentState::Working,
        message: None,
        seq: Some(21),
        session_ref: shepr_agent::agent::resume::AgentSessionRef::path(second_session),
    });

    assert_eq!(second_update, StateUpdate::Unchanged);
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
    state.workspaces[0].test_split(Direction::Horizontal);

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
        .apply_pane_zoom(0, pane_id, PaneZoomCommand::Toggle)
        .expect("a lone pane is a valid target");
    assert!(!outcome.changed);
    let outcome = state
        .apply_pane_zoom(0, pane_id, PaneZoomCommand::On)
        .expect("a lone pane is a valid target");
    assert!(!outcome.changed);

    assert!(!state.workspaces[0].zoomed());
    assert!(!state.session_dirty);
}

#[test]
fn navigate_pane_changes_focus_while_zoomed() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].root_pane();
    let right = state.workspaces[0].test_split(Direction::Horizontal);
    state.workspaces[0].focus_pane(root);
    state.workspaces[0].set_zoomed(true);
    let view = test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert_eq!(view.len(), 1);
    assert_eq!(view[0].id, root);

    state.navigate_pane(NavDirection::Right);
    let view = test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert!(state.workspaces[0].zoomed());
    assert_eq!(state.workspaces[0].focused_pane_id(), right);
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].id, right);
    assert!(view[0].inner_rect.x > view[0].rect.x);
}

#[test]
fn swap_pane_direction_preserves_focus_and_swaps_layout_cells() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].root_pane();
    let right = state.workspaces[0].test_split(Direction::Horizontal);
    state.workspaces[0].focus_pane(root);
    let view = test_view(&mut state, Rect::new(0, 0, 100, 20));
    let rect_of = |view: &[shepr_mux::workspace::PaneChromeInfo], pane_id| {
        view.iter()
            .find(|info| info.id == pane_id)
            .expect("test precondition")
            .rect
    };
    let before_root_rect = rect_of(&view, root);
    let before_right_rect = rect_of(&view, right);

    assert!(state.swap_pane(NavDirection::Right));
    let view = test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert_eq!(state.workspaces[0].focused_pane_id(), root);
    assert_eq!(rect_of(&view, root), before_right_rect);
    assert_eq!(rect_of(&view, right), before_root_rect);
}

#[test]
fn swap_pane_direction_stays_zoomed_and_mutates_hidden_layout() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].root_pane();
    let right = state.workspaces[0].test_split(Direction::Horizontal);
    state.workspaces[0].focus_pane(root);
    state.workspaces[0].set_zoomed(true);
    test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert!(state.swap_pane(NavDirection::Right));
    let view = test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert!(state.workspaces[0].zoomed());
    assert_eq!(state.workspaces[0].focused_pane_id(), root);
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].id, root);

    state.workspaces[0].set_zoomed(false);
    let view = test_view(&mut state, Rect::new(0, 0, 100, 20));
    let root_rect = view
        .iter()
        .find(|info| info.id == root)
        .expect("test precondition")
        .rect;
    let right_rect = view
        .iter()
        .find(|info| info.id == right)
        .expect("test precondition")
        .rect;

    assert!(root_rect.x > right_rect.x);
}

#[test]
fn close_pane_removes_from_workspace() {
    let mut state = app_with_workspaces(&["test"]);
    let closed = state.workspaces[0].test_split(Direction::Horizontal);
    state.ensure_test_terminals();
    assert_eq!(state.workspaces[0].panes().len(), 2);
    assert!(matches!(
        state.remove_pane(0, closed),
        PaneRemovalCommit::Removed(_)
    ));
    assert_eq!(state.workspaces[0].panes().len(), 1);
    state.assert_invariants_for_test();
}

#[test]
fn pane_process_exit_publish_marks_agent_idle_before_pane_removal() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.set_active_index(Some(1));
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
            .state,
        AgentState::Working
    );

    assert!(
        state.publish_pane_process_exit_if_agent(pane_id),
        "the exit releases the agent"
    );

    assert_eq!(
        state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .state,
        AgentState::Idle
    );
}

#[test]
fn close_pane_removes_unattached_terminal_state() {
    let mut state = app_with_workspaces(&["test"]);
    let pane_id = state.workspaces[0].test_split(Direction::Horizontal);
    state.ensure_test_terminals();
    let terminal_id = state
        .terminal_id_for_pane(0, pane_id)
        .expect("test precondition");

    assert!(matches!(
        state.remove_pane(0, pane_id),
        PaneRemovalCommit::Removed(_)
    ));

    assert!(!state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn closing_the_last_workspace_leaves_terminal_mode() {
    let mut state = app_with_workspaces(&["only"]);
    assert_eq!(state.mode, Mode::Terminal);

    state.close_workspace_at(0);

    assert!(state.workspaces.is_empty());
    assert_eq!(state.mode, Mode::Navigate);
    state.assert_invariants_for_test();
}

#[test]
fn close_workspace_at_keeps_the_sidebar_selection_and_focus() {
    let mut state = app_with_workspaces(&["a", "b", "c", "d"]);
    let active_id = state.workspaces[3].id.to_string();
    let selected_id = state.workspaces[2].id.to_string();
    state.set_active_index(Some(3));
    state.set_selected_index(Some(2));

    state.close_workspace_at(0);

    assert_eq!(
        state.workspaces[state.active_index().expect("active")].id,
        active_id
    );
    assert_eq!(
        state.workspaces[state.selected_index().unwrap_or(0)].id,
        selected_id
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
    state.close_selected_workspace();

    assert!(!state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn close_pane_last_pane_closes_active_workspace_not_selected_workspace() {
    let mut state = app_with_workspaces(&["selected", "active"]);
    let active_terminal_id = state
        .terminal_id_for_pane(1, state.workspaces[1].root_pane())
        .expect("test precondition");
    state.set_active_index(Some(1));
    state.set_selected_index(Some(0));

    let pane_id = state.workspaces[1].root_pane();
    assert!(matches!(
        state.remove_pane(1, pane_id),
        PaneRemovalCommit::Removed(_)
    ));

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].display_name(), "selected");
    assert!(!state.terminals.contains_key(&active_terminal_id));
    state.assert_invariants_for_test();
}
