use super::*;
use crate::detect::{Agent, AgentState};
use crate::workspace::Workspace;
use ratatui::layout::{Direction, Rect};

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

fn refresh_test_view(state: &mut AppState, area: Rect) {
    state.view.terminal_area = area;
    state.view.pane_infos = state
        .active_index()
        .and_then(|ws_idx| state.workspaces.get(ws_idx))
        .and_then(|workspace| workspace.tabs.get(workspace.active_tab))
        .map(|tab| {
            state
                .pane_geometry_in(area)
                .tab_panes(&tab.layout, tab.zoomed)
        })
        .unwrap_or_default()
        .into_iter()
        .map(|mut pane| {
            pane.inner_rect = crate::workspace::pane_inner_rect(pane.rect, pane.borders);
            pane
        })
        .collect();
}

fn toggle_focused_zoom(state: &mut AppState) {
    let ws_idx = state.active_index().expect("test precondition");
    let pane_id = state.workspaces[ws_idx]
        .focused_pane_id()
        .expect("test precondition");
    state
        .apply_pane_zoom(ws_idx, pane_id, PaneZoomCommand::Toggle)
        .expect("test precondition");
}

#[test]
fn pane_context_resolver_applies_explicit_and_creation_targets() {
    let mut state = app_with_workspaces(&["active", "selected"]);
    let selected_pane = state.workspaces[1]
        .focused_pane_id()
        .expect("test precondition");
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
    assert_eq!(creation.tab_index, 0);

    let active = state
        .resolve_pane_context(None, None, PaneContextFallback::ActiveWorkspace)
        .expect("active workspace fallback");
    assert_eq!(active.workspace_index, 0);
}

#[test]
fn pane_removal_command_returns_the_removed_container_scope() {
    let mut state = app_with_workspaces(&["one"]);
    let second_tab = state.workspaces[0].test_add_tab(Some("logs"));
    state.ensure_test_terminals();
    let first_pane = state.workspaces[0].tabs[0].root_pane;

    let plan = state
        .prepare_pane_removal(0, first_pane)
        .expect("test precondition");
    assert_eq!(plan.scope, PaneRemovalScope::Tab);
    let PaneRemovalCommit::Removed(outcome) = state.commit_pane_removal(&plan) else {
        panic!("prepared tab removal must commit");
    };
    assert_eq!(outcome.removal.scope, PaneRemovalScope::Tab);
    assert_eq!(state.workspaces[0].tabs.len(), 1);
    assert_eq!(state.workspaces[0].active_tab, second_tab - 1);

    let final_pane = state.workspaces[0].tabs[0].root_pane;
    let PaneRemovalCommit::Removed(outcome) = state.remove_pane(0, final_pane) else {
        panic!("final pane removal must commit");
    };
    assert_eq!(outcome.removal.scope, PaneRemovalScope::Workspace);
    assert!(state.workspaces.is_empty());
    state.assert_invariants_for_test();
}

#[test]
fn pane_split_state_command_commits_prepared_geometry_and_terminal() {
    let mut state = app_with_workspaces(&["one"]);
    let root_pane = state.workspaces[0].tabs[0].root_pane;
    let mut prepared_layout = state.workspaces[0].tabs[0].layout.clone();
    let new_pane = prepared_layout
        .split_pane(root_pane, Direction::Horizontal, 0.5)
        .expect("test precondition");
    assert_eq!(state.workspaces[0].pane_count(), 1);
    let terminal_id = crate::protocol::TerminalId::alloc();
    let terminal =
        crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/tmp"));
    let previous_focus = state.current_pane_focus_target();

    let outcome = state
        .commit_pane_split(
            0,
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
    assert_eq!(state.workspaces[0].focused_pane_id(), Some(new_pane));
    state.assert_invariants_for_test();
}

#[test]
fn workspace_creation_state_command_commits_spawned_values() {
    let mut state = AppState::test_new();
    let workspace = Workspace::test_new("created");
    let root_pane = workspace.tabs[0].root_pane;
    let terminal_id = workspace
        .terminal_id(root_pane)
        .expect("test precondition")
        .clone();
    let terminal =
        crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/tmp"));

    let outcome = state.commit_workspace_creation(workspace, terminal, true);

    assert_eq!(outcome.workspace_index, 0);
    assert_eq!(outcome.root_pane, Some(root_pane));
    assert_eq!(state.active_index(), Some(0));
    assert!(state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn tab_creation_state_command_commits_spawned_values_and_focus() {
    let mut state = app_with_workspaces(&["one"]);
    let workspace = &state.workspaces[0];
    let (layout, root_pane) = crate::core::layout::TileLayout::new();
    let terminal_id = crate::protocol::TerminalId::alloc();
    let mut pane = crate::workspace::TabPane::new(crate::pane::PaneState::new(terminal_id.clone()));
    pane.public_number = workspace.next_public_pane_number();
    let tab = crate::workspace::Tab {
        custom_name: None,
        number: workspace.next_public_tab_number(),
        root_pane,
        layout,
        panes: std::collections::HashMap::from([(root_pane, pane)]),
        zoomed: false,
    };
    let terminal =
        crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/tmp"));

    let outcome = state
        .commit_tab_creation(0, tab, terminal, true)
        .expect("prepared tab creation commits");

    assert_eq!(outcome.tab_index, 1);
    assert_eq!(outcome.root_pane, root_pane);
    assert_eq!(state.workspaces[0].active_tab, 1);
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

    let terminal_runtimes = crate::pane::PaneRuntimeRegistry::new();
    let changed = state.apply_workspace_git_statuses(
        &terminal_runtimes,
        vec![WorkspaceGitStatus {
            workspace_id: first_id,
            resolved_identity_cwd: first_cwd.clone(),
            status_cache_key: first_cwd,
            demand: crate::git::GitStatusRefreshDemand::ALL,
            auto_label: "one".into(),
            branch: Some("main".into()),
            ahead_behind: Some(crate::git::AheadBehind {
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
        Some(crate::git::AheadBehind {
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
    state.workspaces[0].cached_git_ahead_behind = Some(crate::git::AheadBehind {
        ahead: 1,
        behind: 0,
    });

    let terminal_runtimes = crate::pane::PaneRuntimeRegistry::new();
    let changed = state.apply_workspace_git_statuses(
        &terminal_runtimes,
        vec![WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: std::path::PathBuf::from("/definitely/not/current"),
            status_cache_key: std::path::PathBuf::from("/definitely/not/current"),
            demand: crate::git::GitStatusRefreshDemand::ALL,
            auto_label: "stale".into(),
            branch: Some("main".into()),
            ahead_behind: Some(crate::git::AheadBehind {
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
        Some(crate::git::AheadBehind {
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

    let terminal_runtimes = crate::pane::PaneRuntimeRegistry::new();
    let changed = state.apply_workspace_git_statuses(
        &terminal_runtimes,
        vec![WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: crate::git::GitStatusRefreshDemand {
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
    state.workspaces[0].cached_git_ahead_behind = Some(crate::git::AheadBehind {
        ahead: 1,
        behind: 2,
    });

    let terminal_runtimes = crate::pane::PaneRuntimeRegistry::new();
    let changed = state.apply_workspace_git_statuses(
        &terminal_runtimes,
        vec![WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: crate::git::GitStatusRefreshDemand::ALL,
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
        .map(crate::workspace::Workspace::display_name)
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
    let second_tab = state.workspaces[1].test_add_tab(Some("second"));
    state.ensure_test_terminals();
    assert!(state.switch_workspace_tab(1, second_tab));
    state.set_selected_index(Some(2));
    let active_id = state.active.clone();
    let selected_id = state.selected.clone();
    let active_tab_id = state.active_tab_id.clone();

    assert!(state.move_workspace(1, 0));
    assert_eq!(state.active, active_id);
    assert_eq!(state.selected, selected_id);
    assert_eq!(state.active_tab_id, active_tab_id);
    assert_eq!(state.active_index(), Some(0));
    assert_eq!(state.selected_index(), Some(2));

    state.close_workspace_at(1).expect("background workspace");
    assert_eq!(state.active, active_id);
    assert_eq!(state.selected, selected_id);
    assert_eq!(state.active_tab_id, active_tab_id);
    state.assert_invariants_for_test();
}

#[test]
fn move_workspace_accepts_insert_at_end() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);

    state.move_workspace(0, state.workspaces.len());

    let names: Vec<_> = state
        .workspaces
        .iter()
        .map(crate::workspace::Workspace::display_name)
        .collect();
    assert_eq!(names, vec!["b", "c", "a"]);
}

#[test]
fn move_workspace_block_collects_non_contiguous_members() {
    let mut state = app_with_workspaces(&["child-one", "normal", "parent", "child-two", "tail"]);
    let parent_id = state.workspaces[2].id.to_string();
    let child_one_id = state.workspaces[0].id.to_string();
    let child_two_id = state.workspaces[3].id.to_string();
    let tail_id = state.workspaces[4].id.to_string();
    state.set_active_index(Some(0));
    state.set_selected_index(Some(4));

    assert!(state.move_workspace_block(
        &[parent_id, child_one_id.clone(), child_two_id],
        Some(&tail_id),
    ));

    let names = state
        .workspaces
        .iter()
        .map(crate::workspace::Workspace::display_name)
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["normal", "parent", "child-one", "child-two", "tail"]
    );
    assert_eq!(
        state.workspaces[state.active_index().expect("test precondition")].id,
        child_one_id
    );
    assert_eq!(
        state.workspaces[state.selected_index().unwrap_or(0)].id,
        tail_id
    );
}

#[test]
fn move_workspace_block_rejects_invalid_and_noop_orders() {
    let mut state = app_with_workspaces(&["a", "b", "c"]);
    let ids = state
        .workspaces
        .iter()
        .map(|workspace| workspace.id.to_string())
        .collect::<Vec<_>>();

    assert!(!state.move_workspace_block(&[], None));
    assert!(!state.move_workspace_block(&[ids[0].clone(), ids[0].clone()], None));
    assert!(!state.move_workspace_block(&["missing".into()], None));
    assert!(!state.move_workspace_block(&[ids[0].clone()], Some(&ids[0])));
    assert!(!state.move_workspace_block(&[ids[0].clone()], Some(&ids[1])));
    assert_eq!(
        state
            .workspaces
            .iter()
            .map(crate::workspace::Workspace::display_name)
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
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
        .panes
        .keys()
        .next()
        .expect("test precondition");

    state.handle_pane_died(pane_id);

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].custom_name.as_deref(), Some("b"));
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_closing_a_workspace_tears_it_down_like_an_explicit_close() {
    let mut state = app_with_workspaces(&["a", "dying", "c"]);
    state.set_active_index(Some(2));
    state.set_selected_index(Some(0));
    let pane_id = state.workspaces[1].tabs[0].root_pane;
    let terminal_id = state
        .terminal_id_for_pane(1, pane_id)
        .expect("test precondition");
    state
        .public_pane_id_aliases
        .insert("wOLD:p9".into(), pane_id);
    state.direct_attach_resize_locks.insert(terminal_id.clone());
    state.session_dirty = false;

    state.handle_pane_died(pane_id);

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
    assert!(state.terminal_runtime_shutdowns.contains(&terminal_id));
    assert!(!state.direct_attach_resize_locks.contains(&terminal_id));
    assert!(state.public_pane_id_aliases.is_empty());
    assert!(state.session_dirty);
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_last_workspace_enters_navigate() {
    let mut state = app_with_workspaces(&["only"]);
    state.mode = Mode::Terminal;
    let pane_id = *state.workspaces[0]
        .panes
        .keys()
        .next()
        .expect("test precondition");

    state.handle_pane_died(pane_id);

    assert!(state.workspaces.is_empty());
    assert_eq!(state.mode, Mode::Navigate);
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_multi_pane_keeps_workspace() {
    let mut state = app_with_workspaces(&["test"]);
    let second_id = state.workspaces[0].test_split(Direction::Horizontal);
    state.ensure_test_terminals();

    state.handle_pane_died(second_id);

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].panes.len(), 1);
    state.assert_invariants_for_test();
}

#[test]
fn pane_died_unknown_pane_is_noop() {
    let mut state = app_with_workspaces(&["test"]);
    let fake_id = PaneId::from_raw(9999);

    state.handle_pane_died(fake_id);

    assert_eq!(state.workspaces.len(), 1);
    state.assert_invariants_for_test();
}
#[test]
fn state_changed_updates_pane() {
    let mut state = app_with_workspaces(&["test"]);
    let pane_id = *state.workspaces[0]
        .panes
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
        .panes
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
fn agent_state_sequences_track_transitions_for_waiters() {
    let mut app = app_with_workspaces(&["active", "background"]);
    let pane_id = app.workspaces[1].tabs[0].root_pane;
    let terminal_id = app.workspaces[1].panes[&pane_id]
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
fn managed_launch_becomes_interactive_after_state_detection() {
    let mut app = app_with_workspaces(&["active", "background"]);
    let pane_id = app.workspaces[1].tabs[0].root_pane;
    let terminal_id = app.workspaces[1].panes[&pane_id]
        .attached_terminal_id
        .clone();
    app.terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .begin_managed_agent(
            "worker".into(),
            Agent::Pi,
            Instant::now(),
            std::time::Duration::ZERO,
            std::time::Duration::from_secs(60),
        );

    for state in [AgentState::Blocked, AgentState::Idle] {
        app.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state,
            visible_blocker: state == AgentState::Blocked,
            process_exited: false,
            observed_at: Instant::now(),
        });
    }

    let terminal = &app.terminals[&terminal_id];
    assert!(terminal.managed_agent_interactive_ready());
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn agent_prompt_observation_changes_readiness_without_state_change() {
    let mut app = app_with_workspaces(&["active", "background"]);
    let pane_id = app.workspaces[1].tabs[0].root_pane;
    let terminal_id = app.workspaces[1].panes[&pane_id]
        .attached_terminal_id
        .clone();
    app.terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .begin_managed_agent(
            "reviewer".into(),
            Agent::Codex,
            Instant::now(),
            std::time::Duration::ZERO,
            std::time::Duration::from_secs(60),
        );
    app.handle_app_event(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Codex),
        state: AgentState::Unknown,
        visible_blocker: false,
        process_exited: false,
        observed_at: Instant::now(),
    });
    app.handle_app_event(AppEvent::AgentPromptObserved {
        pane_id,
        agent: Agent::Codex,
        ready: true,
    });

    let terminal = &app.terminals[&terminal_id];
    assert!(terminal.managed_agent_interactive_ready());
    assert_eq!(terminal.state, AgentState::Unknown);
}

#[test]
fn visible_blocker_overrides_hook_working() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.set_active_index(Some(0));
    let bg_pane_id = *state.workspaces[1]
        .panes
        .keys()
        .next()
        .expect("test precondition");
    let bg_terminal_id = state.workspaces[1]
        .panes
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
        .panes
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .panes
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
        session_ref: crate::agent::resume::AgentSessionRef::id("claude-session"),
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
fn official_release_preserves_process_owned_agent_identity() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .panes
        .get(&pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();

    state.handle_app_event(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Pi),
        state: AgentState::Working,
        visible_blocker: false,
        process_exited: false,
        observed_at: std::time::Instant::now(),
    });
    let terminal = state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition");
    terminal.set_persisted_agent_session(crate::agent::resume::PersistedAgentSession {
        source: "shepr:pi".into(),
        agent: crate::agent::Agent::Pi,
        session_ref: crate::agent::resume::AgentSessionRef::path(
            std::env::current_dir()
                .expect("test precondition")
                .join("release-session.jsonl")
                .display()
                .to_string(),
        )
        .expect("test precondition"),
    });
    terminal.set_hook_authority(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        Some(1),
    );
    terminal.set_agent_name("reviewer".into());
    state.session_dirty = false;

    let updates = state.handle_app_event(AppEvent::HookAgentReleased {
        pane_id,
        source: "shepr:pi".into(),
        agent_label: "pi".into(),
        known_agent: Some(Agent::Pi),
        seq: Some(2),
    });

    assert!(updates.is_empty());
    let terminal = &state.terminals[&terminal_id];
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(terminal.detected_agent, Some(Agent::Pi));
    assert_eq!(terminal.agent_name.as_deref(), Some("reviewer"));
    assert!(terminal.full_lifecycle_hook_authority_active());
    assert!(!state.session_dirty);
}

#[test]
fn devin_state_report_refreshes_session_without_overriding_screen_state() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .panes
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
        session_ref: crate::agent::resume::AgentSessionRef::id("devin-session"),
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
        .panes
        .keys()
        .next()
        .expect("test precondition");
    let test_dir = std::env::current_dir().expect("test precondition");
    let first_session = test_dir.join("one.jsonl").display().to_string();
    let second_session = test_dir.join("two.jsonl").display().to_string();

    let first_updates = state.handle_app_event(AppEvent::HookStateReported {
        pane_id,
        source: "custom:pi".into(),
        agent_label: "pi".into(),
        state: AgentState::Working,
        message: None,
        seq: Some(20),
        session_ref: crate::agent::resume::AgentSessionRef::path(first_session),
    });
    assert_eq!(first_updates.len(), 1);
    state.session_dirty = false;

    let second_updates = state.handle_app_event(AppEvent::HookStateReported {
        pane_id,
        source: "custom:pi".into(),
        agent_label: "pi".into(),
        state: AgentState::Working,
        message: None,
        seq: Some(21),
        session_ref: crate::agent::resume::AgentSessionRef::path(second_session),
    });

    assert!(second_updates.is_empty());
    assert!(state.session_dirty);
}

#[test]
fn custom_release_clears_report_owned_agent() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .pane_state(pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .set_hook_authority(
            "custom:agent".into(),
            "custom-agent".into(),
            AgentState::Working,
            None,
            Some(1),
        );

    state.handle_app_event(AppEvent::HookAgentReleased {
        pane_id,
        source: "custom:agent".into(),
        agent_label: "custom-agent".into(),
        known_agent: None,
        seq: Some(2),
    });

    let terminal = &state.terminals[&terminal_id];
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Unknown);
}

#[test]
fn terminal_cwd_report_updates_terminal_cwd_and_marks_session_dirty() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .pane_state(pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    // Deliberately a path that does not exist: the reporting thread
    // validates the directory, and the main loop must not stat it again.
    let cwd = std::path::PathBuf::from(format!(
        "/shepr-cwd-report-test-{}/does-not-exist",
        std::process::id()
    ));
    state.session_dirty = false;

    let updates = state.handle_app_event(AppEvent::TerminalCwdReported {
        pane_id,
        cwd: cwd.clone(),
    });

    assert!(updates.is_empty());
    assert_eq!(
        state
            .terminals
            .get(&terminal_id)
            .expect("test precondition")
            .cwd,
        cwd
    );
    assert!(state.session_dirty);
}

#[test]
fn relative_terminal_cwd_report_is_ignored() {
    let mut state = app_with_workspaces(&["active"]);
    let pane_id = *state.workspaces[0]
        .panes
        .keys()
        .next()
        .expect("test precondition");
    let terminal_id = state.workspaces[0]
        .pane_state(pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    let before = state.terminals[&terminal_id].cwd.clone();
    state.session_dirty = false;

    state.handle_app_event(AppEvent::TerminalCwdReported {
        pane_id,
        cwd: std::path::PathBuf::from("relative/dir"),
    });

    assert_eq!(state.terminals[&terminal_id].cwd, before);
    assert!(!state.session_dirty);
}

#[test]
fn metadata_expiry_state_change_bumps_state_sequence() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.set_active_index(Some(0));
    let pane_id = state.workspaces[1].tabs[0].root_pane;
    let terminal_id = state
        .terminal_id_for_pane(1, pane_id)
        .expect("test precondition");
    let before_report = Instant::now();
    {
        let terminal = state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        terminal.set_agent_metadata(crate::terminal::AgentMetadataReport {
            source: "custom:status".into(),
            agent_label: None,
            applies_to_source: None,
            title: Some("temporary".into()),
            display_agent: None,
            state_labels: std::collections::HashMap::new(),
            clear_title: false,
            clear_display_agent: false,
            clear_state_labels: false,
            ttl: Some(std::time::Duration::from_millis(1)),
            seq: None,
        });
        // Leave the effective state stale so the expiry's recompute is
        // what moves it, the way a time-dependent authority change would.
        terminal.state = AgentState::Working;
    }
    let seq_before = state.next_agent_state_change_seq;

    let updates = state.expire_agent_metadata_at(
        before_report,
        Instant::now() + std::time::Duration::from_secs(1),
    );

    let update = updates.first().expect("expiry publishes a state update");
    assert_eq!(update.previous.state, AgentState::Working);
    assert_eq!(update.current.state, AgentState::Idle);
    assert_eq!(state.next_agent_state_change_seq, seq_before + 1);
    let terminal = &state.terminals[&terminal_id];
    assert_eq!(
        terminal.last_agent_state_change_seq,
        Some(state.next_agent_state_change_seq)
    );
}

#[test]
fn toggle_zoom_works() {
    let mut state = app_with_workspaces(&["test"]);
    state.workspaces[0].test_split(Direction::Horizontal);

    assert!(!state.workspaces[0].zoomed);
    toggle_focused_zoom(&mut state);
    assert!(state.workspaces[0].zoomed);
    toggle_focused_zoom(&mut state);
    assert!(!state.workspaces[0].zoomed);
}

#[test]
fn toggle_zoom_single_pane_noop() {
    let mut state = app_with_workspaces(&["test"]);
    toggle_focused_zoom(&mut state);
    assert!(!state.workspaces[0].zoomed);
}

#[test]
fn navigate_pane_changes_focus_while_zoomed() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].tabs[0].root_pane;
    let right = state.workspaces[0].test_split(Direction::Horizontal);
    state.workspaces[0].layout.focus_pane(root);
    state.workspaces[0].zoomed = true;
    refresh_test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert_eq!(state.view.pane_infos.len(), 1);
    assert_eq!(state.view.pane_infos[0].id, root);

    state.navigate_pane(NavDirection::Right);
    refresh_test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert!(state.workspaces[0].zoomed);
    assert_eq!(state.workspaces[0].focused_pane_id(), Some(right));
    assert_eq!(state.view.pane_infos.len(), 1);
    assert_eq!(state.view.pane_infos[0].id, right);
    assert!(state.view.pane_infos[0].inner_rect.x > state.view.pane_infos[0].rect.x);
}

#[test]
fn swap_pane_direction_preserves_focus_and_swaps_layout_cells() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].tabs[0].root_pane;
    let right = state.workspaces[0].test_split(Direction::Horizontal);
    state.workspaces[0].layout.focus_pane(root);
    refresh_test_view(&mut state, Rect::new(0, 0, 100, 20));
    let before_root_rect = state
        .view
        .pane_infos
        .iter()
        .find(|info| info.id == root)
        .expect("test precondition")
        .rect;
    let before_right_rect = state
        .view
        .pane_infos
        .iter()
        .find(|info| info.id == right)
        .expect("test precondition")
        .rect;

    assert!(state.swap_pane(NavDirection::Right));
    refresh_test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert_eq!(state.workspaces[0].focused_pane_id(), Some(root));
    assert_eq!(
        state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == root)
            .expect("test precondition")
            .rect,
        before_right_rect
    );
    assert_eq!(
        state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == right)
            .expect("test precondition")
            .rect,
        before_root_rect
    );
}

#[test]
fn swap_pane_direction_stays_zoomed_and_mutates_hidden_layout() {
    let mut state = app_with_workspaces(&["test"]);
    let root = state.workspaces[0].tabs[0].root_pane;
    let right = state.workspaces[0].test_split(Direction::Horizontal);
    state.workspaces[0].layout.focus_pane(root);
    state.workspaces[0].zoomed = true;
    refresh_test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert!(state.swap_pane(NavDirection::Right));
    refresh_test_view(&mut state, Rect::new(0, 0, 100, 20));

    assert!(state.workspaces[0].zoomed);
    assert_eq!(state.workspaces[0].focused_pane_id(), Some(root));
    assert_eq!(state.view.pane_infos.len(), 1);
    assert_eq!(state.view.pane_infos[0].id, root);

    state.workspaces[0].zoomed = false;
    refresh_test_view(&mut state, Rect::new(0, 0, 100, 20));
    let root_rect = state
        .view
        .pane_infos
        .iter()
        .find(|info| info.id == root)
        .expect("test precondition")
        .rect;
    let right_rect = state
        .view
        .pane_infos
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
    assert_eq!(state.workspaces[0].panes.len(), 2);
    assert!(matches!(
        state.remove_pane(0, closed),
        PaneRemovalCommit::Removed(_)
    ));
    assert_eq!(state.workspaces[0].panes.len(), 1);
    state.assert_invariants_for_test();
}

#[test]
fn pane_process_exit_publish_marks_agent_idle_before_pane_removal() {
    let mut state = app_with_workspaces(&["active", "background"]);
    state.set_active_index(Some(1));
    state.ensure_test_terminals();
    let pane_id = state.workspaces[0].tabs[0].root_pane;
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

    let update = state
        .publish_pane_process_exit_if_agent(pane_id)
        .expect("process exit update");

    assert_eq!(update.workspace_id, state.workspaces[0].id);
    assert_eq!(update.previous.state, AgentState::Working);
    assert_eq!(update.current.state, AgentState::Idle);
    assert_eq!(update.current.agent_label.as_deref(), Some("pi"));
    assert_eq!(update.current.known_agent, Some(Agent::Pi));
    assert!(update.cause.released());
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
fn close_tab_removes_unattached_terminal_states() {
    let mut state = app_with_workspaces(&["test"]);
    let tab_idx = state.workspaces[0].test_add_tab(Some("logs"));
    state.ensure_test_terminals();
    state.workspaces[0].switch_tab(tab_idx);
    let pane_id = state.workspaces[0].tabs[tab_idx].root_pane;
    let terminal_id = state
        .terminal_id_for_pane(0, pane_id)
        .expect("test precondition");
    assert!(matches!(
        state.remove_active_tab(),
        TabRemovalCommit::Removed(_)
    ));

    assert!(!state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn close_workspace_prunes_aliases_and_resize_locks_of_its_panes() {
    let mut state = app_with_workspaces(&["closing", "kept"]);
    let closing_pane = state.workspaces[0].tabs[0].root_pane;
    let kept_pane = state.workspaces[1].tabs[0].root_pane;
    let closing_terminal = state
        .terminal_id_for_pane(0, closing_pane)
        .expect("test precondition");
    let kept_terminal = state
        .terminal_id_for_pane(1, kept_pane)
        .expect("test precondition");
    state
        .public_pane_id_aliases
        .insert("wOLD:p1".into(), closing_pane);
    state
        .public_pane_id_aliases
        .insert("wOLD:p2".into(), kept_pane);
    state
        .direct_attach_resize_locks
        .insert(closing_terminal.clone());
    state
        .direct_attach_resize_locks
        .insert(kept_terminal.clone());

    state.close_workspace_at(0);

    assert!(!state.public_pane_id_aliases.contains_key(&"wOLD:p1".into()));
    assert_eq!(
        state.public_pane_id_aliases.get(&"wOLD:p2".into()),
        Some(&kept_pane)
    );
    assert!(!state.direct_attach_resize_locks.contains(&closing_terminal));
    assert!(state.direct_attach_resize_locks.contains(&kept_terminal));
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
    let pane_id = state.workspaces[0].tabs[0].root_pane;
    let terminal_id = state
        .terminal_id_for_pane(0, pane_id)
        .expect("test precondition");
    let _ = pane_id;
    state.close_selected_workspace();

    assert!(!state.terminals.contains_key(&terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn close_tab_closes_active_workspace_not_selected_workspace() {
    let mut state = app_with_workspaces(&["selected", "active"]);
    let active_terminal_id = state
        .terminal_id_for_pane(1, state.workspaces[1].tabs[0].root_pane)
        .expect("test precondition");
    state.set_active_index(Some(1));
    state.set_selected_index(Some(0));

    assert!(matches!(
        state.remove_active_tab(),
        TabRemovalCommit::Removed(_)
    ));

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].display_name(), "selected");
    assert!(!state.terminals.contains_key(&active_terminal_id));
    state.assert_invariants_for_test();
}

#[test]
fn close_pane_last_pane_closes_active_workspace_not_selected_workspace() {
    let mut state = app_with_workspaces(&["selected", "active"]);
    let active_terminal_id = state
        .terminal_id_for_pane(1, state.workspaces[1].tabs[0].root_pane)
        .expect("test precondition");
    state.set_active_index(Some(1));
    state.set_selected_index(Some(0));

    let pane_id = state.workspaces[1].tabs[0].root_pane;
    assert!(matches!(
        state.remove_pane(1, pane_id),
        PaneRemovalCommit::Removed(_)
    ));

    assert_eq!(state.workspaces.len(), 1);
    assert_eq!(state.workspaces[0].display_name(), "selected");
    assert!(!state.terminals.contains_key(&active_terminal_id));
    state.assert_invariants_for_test();
}
