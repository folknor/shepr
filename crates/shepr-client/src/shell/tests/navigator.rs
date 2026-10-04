use crate::endpoint::ClientEndpointId;
use crate::shell::config::ClientShellConfig;
use crate::shell::navigation::location::{Location, LocationTarget};
use crate::shell::overlays::navigator::ClientNavigatorFilter;
use crate::shell::overlays::{Overlay, navigator::NavigatorOverlay};
use crate::shell::presentation::text;
use crate::shell::state::{ClientShellAction, ClientShellInput, ClientShellState};
use crate::shell::tests::{cell_fg, cell_is_bold, cell_symbol_position, snapshot, surface};
use crate::tests::{test_pane_id, test_workspace_id};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use shepr_config::ClientConfig;
use shepr_config::theme::Palette;
use shepr_protocol::command::EndpointCommand;
use shepr_protocol::{AgentStatus, ClientShellAgent, ClientShellSnapshot};
use shepr_termio::input::raw_input::RawInputEvent;

#[test]
fn navigator_workspace_headings_use_the_active_themes_primary_text() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.open_navigator_overlay();
    for palette in [
        Palette::catppuccin(),
        Palette::catppuccin_latte(),
        Palette::terminal(),
    ] {
        state.config.palette = palette;
        let frame = state.compose(106, 30).expect("navigator");
        let (rect, _) = state
            .drawn()
            .navigator_rows()
            .find(|(_, target)| matches!(target.target, LocationTarget::Workspace(_)))
            .expect("workspace heading");
        let position = cell_symbol_position(&frame, rect, "client-shell");
        assert_eq!(cell_fg(&frame, position), state.config.palette.text);
        assert!(cell_is_bold(&frame, position));
    }
}

#[test]
fn navigator_renders_every_terminal_in_workspace_sections() {
    let mut snapshot = snapshot();
    snapshot.focused_pane_id = None;
    snapshot.panes[0].label = Some("agent".into());
    let mut shell = snapshot.panes[0].clone();
    shell.pane_id = test_pane_id("w1:p2");
    shell.label = Some("shell".into());
    snapshot.panes.push(shell);
    for label in ["notes", "logs"] {
        let mut pane = snapshot.panes[0].clone();
        pane.pane_id = shepr_protocol::PublicPaneId::new(
            &crate::tests::test_workspace_id("w1"),
            shepr_protocol::PanePublicNumber::new(snapshot.panes.len() + 1)
                .expect("nonzero test number"),
        );
        pane.label = Some(label.into());
        snapshot.panes.push(pane);
    }
    let mut workspace = snapshot.workspaces[0].clone();
    workspace.workspace_id = test_workspace_id("w2");
    workspace.label = "second".into();
    let mut pane = snapshot.panes[0].clone();
    pane.pane_id = test_pane_id("w2:p1");
    snapshot.workspaces.push(workspace);
    snapshot.panes.push(pane);
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.open_navigator_overlay();
    let visible_rows = |state: &mut ClientShellState, height| {
        let frame = state.compose(106, height).expect("navigator frame");
        state
            .drawn()
            .navigator_rows()
            .map(|(rect, _)| {
                frame.cells()[rect.y as usize * frame.width() as usize + rect.x as usize..]
                    .iter()
                    .take(rect.width as usize)
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    let visible = visible_rows(&mut state, 30);
    assert_eq!(visible.len(), 7);
    for (row, prefix) in visible
        .iter()
        .zip([" client", " ├─ ", " ├─ ", " ├─ ", " └─ ", " second", " └─ "])
    {
        assert!(
            row.starts_with(prefix),
            "{row:?} should start with {prefix:?}"
        );
    }
    for (row, label) in visible.iter().zip([
        "client-shell",
        "agent · 1",
        "shell · 2",
        "notes · 3",
        "logs · 4",
        "second",
        "agent",
    ]) {
        assert!(row.contains(label), "{row:?} should contain {label}");
        assert!(!row.contains("/repo"));
        assert!(!row.contains("──"));
    }

    let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("expected navigator");
    };
    navigator.scroll = 2;
    navigator.selected = Some(Location::pane(
        state.endpoints.presented().clone(),
        test_pane_id("w1:p2"),
    ));
    let visible = visible_rows(&mut state, 11);
    assert_eq!(visible.len(), 2);
    assert!(visible.iter().any(|row| row.contains("shell · 2")));
    assert!(visible[0].starts_with(" ├─ "));
    assert!(visible[1].starts_with(" ├─ "));

    // Filtering by the pane's id retains the section and the exact split destination.
    let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("expected navigator");
    };
    navigator.query = "w1:p2".into();
    navigator.scroll = 0;
    let visible = visible_rows(&mut state, 30);
    assert_eq!(visible.len(), 2);
    assert!(visible[1].starts_with(" └─ "));
    assert!(visible.iter().any(|row| row.contains("shell · 2")));
    assert!(visible.iter().all(|row| !row.contains("second")));
}

#[test]
fn navigator_search_matches_non_adjacent_words_without_losing_the_pane_target() {
    let mut projected = snapshot();
    projected.panes[0].label = Some("alpha beta gamma".into());
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.open_navigator_overlay();
    let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    for (query, matches) in [
        ("alpha gamma", true),
        ("  ALP\tGAM  ", true),
        ("gamma alpha", true),
        ("beta gamma", true),
        ("alpha missing", false),
        ("alphagamma", false),
    ] {
        navigator.query = query.into();
        navigator.selected = None;
        let rows =
            text::client_navigator_rows(&state.endpoints, state.endpoints.presented(), navigator);
        let target = crate::shell::navigation::aggregate_navigation::selected_navigator_target(
            &rows, navigator,
        );
        assert_eq!(
            target,
            matches.then(|| {
                Location::pane(state.endpoints.presented().clone(), test_pane_id("w1:p1"))
            }),
            "query={query:?}"
        );
    }
}

#[test]
fn navigator_searches_ancestor_context_and_keeps_split_agents_individually_actionable() {
    let mut projected = snapshot();
    projected.panes[0].pane_id = test_pane_id("w1:p1");
    projected.focused_pane_id = Some(test_pane_id("w1:p1"));
    let mut second = projected.panes[0].clone();
    second.pane_id = test_pane_id("w1:p2");
    second.foreground_cwd = Some("/repo/subproject".into());
    projected.panes.push(second);
    let first_agent = ClientShellAgent {
        pane_id: test_pane_id("w1:p1"),
        agent: Some(shepr_config::ConfigAgent::Pi),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: shepr_test_fixtures::counter_at(1),
    };
    let mut second_agent = first_agent.clone();
    second_agent.pane_id = "w1:p2".parse().expect("test precondition");
    second_agent.agent = Some(shepr_config::ConfigAgent::Claude);
    second_agent.terminal_title_stripped = Some("checking navigation".into());
    second_agent.agent_status = AgentStatus::Blocked;
    projected.agents = vec![first_agent, second_agent];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.open_navigator_overlay();
    for (query, filter, expected) in [
        ("", None, vec!["w1:p1", "w1:p2"]),
        ("client-shell", None, vec!["w1:p1", "w1:p2"]),
        ("main", None, vec!["w1:p1", "w1:p2"]),
        ("claude", None, vec!["w1:p2"]),
        ("checking navigation", None, vec!["w1:p2"]),
        ("/repo/subproject", None, vec!["w1:p2"]),
        (
            "client-shell",
            Some(ClientNavigatorFilter::Blocked),
            vec!["w1:p2"],
        ),
        ("", Some(ClientNavigatorFilter::Working), vec!["w1:p1"]),
        ("no such agent", None, vec![]),
    ] {
        let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
            panic!("navigator");
        };
        navigator.query = query.into();
        navigator.filter = filter;
        navigator.selected = None;
        let rows =
            text::client_navigator_rows(&state.endpoints, state.endpoints.presented(), navigator);
        let pane_ids = rows
            .iter()
            .filter_map(|row| row.target.pane_id().map(|pane_id| pane_id.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(pane_ids, expected, "query={query:?} filter={filter:?}");
        assert_eq!(
            rows.len(),
            if expected.is_empty() {
                0
            } else {
                expected.len() + 1
            }
        );
        if !expected.is_empty() {
            let selected =
                crate::shell::navigation::aggregate_navigation::navigator_selected_index(
                    &rows, navigator,
                )
                .expect("search destination");
            assert!(rows[selected].target.pane_id().is_some());
        }
    }
    let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    navigator.query.clear();
    let frame = state.compose(160, 48).expect("navigator");
    assert_eq!(state.drawn().navigator_popup().width, 116);
    let pane_rows = state
        .drawn()
        .navigator_rows()
        .filter(|(_, target)| target.pane_id().is_some())
        .collect::<Vec<_>>();
    assert_eq!(pane_rows.len(), 2);
    for ((rect, _), (name, kind, status)) in pane_rows.iter().zip([
        ("pi · 1", "pi", "working"),
        ("checking navigation", "claude", "blocked"),
    ]) {
        cell_symbol_position(&frame, *rect, name);
        cell_symbol_position(&frame, *rect, kind);
        cell_symbol_position(&frame, *rect, status);
    }
    let rect = pane_rows[1].0;
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.right() - 1,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    // An explicit pick goes through the runtime, which knows whether the
    // endpoint is shown.
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint(Location {
            endpoint: ClientEndpointId::Local,
            target: LocationTarget::Pane(pane_id),
        })] if pane_id == &crate::tests::test_pane_id("w1:p2")
    ));
}

#[test]
fn navigator_distinguishes_unnamed_terminals_in_one_workspace() {
    let mut projected = snapshot();
    for number in [2, 3] {
        let mut pane = projected.panes[0].clone();
        pane.pane_id = shepr_protocol::PublicPaneId::new(
            &crate::tests::test_workspace_id("w1"),
            shepr_protocol::PanePublicNumber::new(number).expect("nonzero test number"),
        );
        projected.panes.push(pane);
    }
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.open_navigator_overlay();
    let Some(Overlay::Navigator(navigator)) = &state.overlay else {
        panic!("navigator");
    };
    let rows =
        text::client_navigator_rows(&state.endpoints, state.endpoints.presented(), navigator);
    let labels = rows
        .iter()
        .filter(|row| row.target.pane_id().is_some())
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["terminal · 1", "terminal · 2", "terminal · 3"]);
}

#[test]
fn navigator_keeps_empty_workspaces_searchable_without_status_filters() {
    let mut projected = snapshot();
    projected.panes.clear();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.open_navigator_overlay();
    for (query, filter, expected) in [
        ("", None, true),
        ("client-shell", None, true),
        ("main", None, true),
        ("missing", None, false),
        ("main", Some(ClientNavigatorFilter::Idle), false),
        ("", Some(ClientNavigatorFilter::Working), false),
    ] {
        let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
            panic!("navigator");
        };
        navigator.query = query.into();
        navigator.filter = filter;
        let rows =
            text::client_navigator_rows(&state.endpoints, state.endpoints.presented(), navigator);
        assert_eq!(
            rows.len(),
            usize::from(expected),
            "query={query:?}, filter={filter:?}"
        );
        let target = crate::shell::navigation::aggregate_navigation::selected_navigator_target(
            &rows, navigator,
        );
        assert_eq!(
            target,
            expected.then(|| {
                Location::workspace(ClientEndpointId::Local, shepr_test_fixtures::id("w1"))
            })
        );
    }
}

#[test]
fn navigator_horizontal_arrows_jump_sections_but_edit_the_search_cursor() {
    let mut projected = snapshot();
    projected.panes[0].label = Some("needle-first".into());
    let mut sibling = projected.panes[0].clone();
    sibling.pane_id = test_pane_id("w1:p2");
    sibling.label = Some("other".into());
    projected.panes.push(sibling);
    let mut empty = projected.workspaces[0].clone();
    empty.workspace_id = test_workspace_id("w8");
    empty.label = "empty".into();
    projected.workspaces.push(empty);
    let mut last = projected.workspaces[0].clone();
    last.workspace_id = test_workspace_id("w9");
    last.label = "last".into();
    for (id, label) in [("w9:p1", "needle-last"), ("w9:p2", "other-last")] {
        let mut pane = projected.panes[0].clone();
        pane.pane_id = test_pane_id(id);
        pane.label = Some(label.into());
        projected.panes.push(pane);
    }
    projected.workspaces.push(last);
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.open_navigator_overlay();
    let press = |state: &mut ClientShellState, code| {
        let outcome = state.handle_raw_events(vec![RawInputEvent::Key(
            shepr_term::key::TerminalKey::new(code, KeyModifiers::empty()),
        )]);
        assert!(outcome.actions.is_empty());
    };
    let selected = |state: &ClientShellState| {
        let Some(Overlay::Navigator(navigator)) = &state.overlay else {
            panic!("navigator");
        };
        navigator.selected.clone()
    };
    let target = |id: &str| Some(Location::pane(ClientEndpointId::Local, test_pane_id(id)));
    press(&mut state, KeyCode::Left);
    assert_eq!(selected(&state), target("w1:p1"));
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), target("w9:p1"));
    press(&mut state, KeyCode::Down);
    assert_eq!(selected(&state), target("w9:p2"));
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), target("w9:p2"));
    press(&mut state, KeyCode::Left);
    assert_eq!(selected(&state), target("w1:p1"));
    let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    navigator.query = "needle".into();
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), target("w9:p1"));
    press(&mut state, KeyCode::Left);
    assert_eq!(selected(&state), target("w1:p1"));
    press(&mut state, KeyCode::Char('/'));
    press(&mut state, KeyCode::Left);
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), target("w1:p1"));
    press(&mut state, KeyCode::Left);
    press(&mut state, KeyCode::Char('X'));
    let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    assert_eq!(navigator.query.as_str(), "needlXe");
    navigator.search_focused = false;
    navigator.selected = None;
    press(&mut state, KeyCode::Left);
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), None);
}

#[test]
fn navigator_scrollbar_click_and_drag_scroll_without_opening_a_destination() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.open_navigator_overlay();
    state.compose(106, 24).expect("small navigator");
    assert!(state.drawn().navigator_scrollbar().is_empty());

    let mut projected = snapshot();
    for index in 2..=60 {
        let mut pane = projected.panes[0].clone();
        pane.pane_id = shepr_protocol::PublicPaneId::new(
            &crate::tests::test_workspace_id("w1"),
            shepr_protocol::PanePublicNumber::new(index).expect("nonzero test number"),
        );
        pane.label = Some(format!("agent {index}"));
        projected.panes.push(pane);
    }
    state.set_snapshot(Box::new(projected));
    let frame = state.compose(106, 24).expect("overflowing navigator");
    let track = state.drawn().navigator_scrollbar();
    let metrics = state
        .drawn()
        .navigator_scroll_metrics()
        .expect("scroll metrics");
    assert!(!track.is_empty());
    assert_eq!(metrics.start(), 0);
    assert!(track.y > state.drawn().navigator_search().y);
    assert!(track.bottom() < state.drawn().navigator_popup().bottom() - 3);
    assert!(
        state
            .drawn()
            .navigator_rows()
            .all(|(rect, _)| rect.right() == track.x)
    );
    assert_eq!(
        cell_fg(&frame, (track.x, track.y)),
        state.config.palette.overlay1
    );
    assert_eq!(
        cell_fg(&frame, (track.x, track.bottom() - 1)),
        state.config.palette.overlay0
    );
    let mouse = |state: &mut ClientShellState, kind, row| {
        let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind,
            column: track.x,
            row,
            modifiers: KeyModifiers::empty(),
        })]);
        assert!(outcome.actions.is_empty());
        assert!(matches!(state.overlay, Some(Overlay::Navigator(_))));
    };
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        track.bottom() - 1,
    );
    state.compose(106, 24).expect("track jump");
    assert_eq!(
        state
            .drawn()
            .navigator_scroll_metrics()
            .expect("metrics")
            .start(),
        metrics.max_start()
    );
    let last_pane = shepr_protocol::PublicPaneId::new(
        &crate::tests::test_workspace_id("w1"),
        shepr_protocol::PanePublicNumber::new(60).expect("nonzero literal"),
    );
    assert!(
        state
            .drawn()
            .navigator_rows()
            .any(|(_, target)| target.pane_id() == Some(last_pane))
    );
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), track.y);
    state.compose(106, 24).expect("jump back to top");
    assert_eq!(
        state
            .drawn()
            .navigator_scroll_metrics()
            .expect("metrics")
            .start(),
        0
    );
    let thumb =
        shepr_term::scroll::scrollbar_thumb(metrics, crate::shell::view::list::scroll_track(track))
            .expect("thumb");
    let grab = thumb.len - 1;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        thumb.top + grab,
    );
    // The navigator holds its own scrollbar grab, not the shell's chrome drag.
    assert!(state.pointer.chrome_drag.is_none());
    assert!(matches!(
        state.overlay,
        Some(Overlay::Navigator(NavigatorOverlay {
            drag: Some(grab_row_offset),
            ..
        })) if grab_row_offset == grab
    ));
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        thumb.top + grab,
    );
    state.compose(106, 24).expect("grab does not move viewport");
    assert_eq!(
        state
            .drawn()
            .navigator_scroll_metrics()
            .expect("metrics")
            .start(),
        0
    );
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        track.bottom() + 5,
    );
    state.compose(106, 24).expect("drag to bottom");
    assert_eq!(
        state
            .drawn()
            .navigator_scroll_metrics()
            .expect("metrics")
            .start(),
        metrics.max_start()
    );
    mouse(
        &mut state,
        MouseEventKind::Up(MouseButton::Left),
        track.bottom() + 5,
    );
    assert!(matches!(
        state.overlay,
        Some(Overlay::Navigator(NavigatorOverlay { drag: None, .. }))
    ));
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Up,
        KeyModifiers::empty(),
    ))]);
    state.compose(106, 24).expect("keyboard resumes after drag");
    assert_eq!(
        state
            .drawn()
            .navigator_scroll_metrics()
            .expect("metrics")
            .start(),
        metrics.max_start() - 1
    );

    let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    // The last pane's id is the only text that matches it alone.
    navigator.query = last_pane.to_string().as_str().into();
    navigator.selected = None;
    state.compose(106, 24).expect("filtered navigator");
    assert!(state.drawn().navigator_scrollbar().is_empty());
    assert_eq!(state.drawn().navigator_rows().count(), 2);
    state.compose(106, 90).expect("tall filtered navigator");
    assert!(state.drawn().navigator_scrollbar().is_empty());
}

#[test]
fn navigator_selection_moves_within_a_scrolled_viewport() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let mut projected = snapshot();
    for index in 2..=60 {
        let mut pane = projected.panes[0].clone();
        pane.pane_id = shepr_protocol::PublicPaneId::new(
            &crate::tests::test_workspace_id("w1"),
            shepr_protocol::PanePublicNumber::new(index).expect("nonzero test number"),
        );
        pane.label = Some(format!("agent {index}"));
        projected.panes.push(pane);
    }
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.open_navigator_overlay();
    state.compose(106, 24).expect("navigator");

    let press_key = |state: &mut ClientShellState, code| {
        state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
            code,
            KeyModifiers::empty(),
        ))]);
        state.compose(106, 24).expect("navigator frame");
    };
    let start = |state: &ClientShellState| {
        state
            .drawn()
            .navigator_scroll_metrics()
            .expect("navigator metrics")
            .start()
    };
    let selected_rect = |state: &ClientShellState| {
        let Some(Overlay::Navigator(navigator)) = state.overlay.as_ref() else {
            panic!("navigator");
        };
        let selected = navigator.selected.clone().expect("selected row");
        state
            .drawn()
            .navigator_rows()
            .find(|(_, target)| **target == selected)
            .map(|(rect, _)| rect)
            .expect("the selected row is drawn")
    };
    for _ in 0..80 {
        if start(&state) > 0 {
            break;
        }
        press_key(&mut state, KeyCode::Down);
    }
    let scrolled = start(&state);
    assert!(scrolled > 0, "the viewport scrolled with the selection");
    let before = selected_rect(&state);

    press_key(&mut state, KeyCode::Up);

    assert_eq!(start(&state), scrolled, "moving up inside the viewport");
    assert_eq!(selected_rect(&state).y, before.y - 1);
}

#[test]
fn navigator_narrow_layout_and_long_search_stay_inside_the_popup() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.open_navigator_overlay();
    for (width, height) in [(24, 12), (50, 24), (106, 30)] {
        let Some(Overlay::Navigator(navigator)) = state.overlay.as_mut() else {
            panic!("navigator");
        };
        navigator.search_focused = true;
        navigator.query = "界".repeat(100).as_str().into();
        let frame = state.compose(width, height).expect("navigator frame");
        let popup = state.drawn().navigator_popup();
        let cursor = frame.cursor().expect("search cursor");
        assert!(crate::shell::input::hit_test::contains(
            popup,
            (cursor.x, cursor.y)
        ));
        assert!(state.drawn().navigator_rows().next().is_none());
        assert!(popup.right() <= width && popup.bottom() <= height);
    }
}

fn navigator_scale_snapshot(workspaces: usize, panes: usize) -> ClientShellSnapshot {
    let mut result = snapshot();
    let workspace_template = result.workspaces[0].clone();
    let pane_template = result.panes[0].clone();
    result.workspaces.clear();
    result.panes.clear();
    for w in 0..workspaces {
        let mut workspace = workspace_template.clone();
        workspace.workspace_id =
            shepr_protocol::WorkspaceId::from_number(w + 1).expect("one-based workspace number");
        workspace.label = format!("workspace {w}");
        for p in 0..panes {
            let mut pane = pane_template.clone();
            pane.pane_id = shepr_protocol::PublicPaneId::new(
                &workspace.workspace_id,
                shepr_protocol::PanePublicNumber::new(p + 1).expect("nonzero test number"),
            );
            pane.label = Some(format!("terminal {p}"));
            result.panes.push(pane);
        }
        result.workspaces.push(workspace);
    }
    result.focused_workspace_id = Some(result.workspaces[0].workspace_id);
    result.focused_pane_id = Some(result.panes[0].pane_id);
    result
}

#[test]
fn navigator_grouping_keeps_snapshot_order_with_interleaved_panes() {
    let mut snapshot = navigator_scale_snapshot(2, 2);
    snapshot.panes.reverse();
    let expected = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| {
            snapshot
                .panes
                .iter()
                .filter(|pane| pane.pane_id.workspace_id() == &workspace.workspace_id)
                .map(|pane| pane.pane_id)
        })
        .collect::<Vec<_>>();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let remote = shepr_config::MachineConfig {
        label: shepr_config::MachineLabel::parse("Remote").expect("test precondition"),
        ssh: shepr_config::SshTarget::parse("dev@example.invalid").expect("test precondition"),
    };
    let remote_id = ClientEndpointId::Ssh(remote.label.clone());
    state.set_machines(&[remote]);
    state.connect_endpoint_with_snapshot(&remote_id, 1, Box::new(snapshot.clone()));
    state.set_snapshot(Box::new(snapshot));
    state.open_navigator_overlay();
    let Some(Overlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("navigator")
    };
    let rows =
        text::client_navigator_rows(&state.endpoints, state.endpoints.presented(), navigator);
    let actual = rows
        .iter()
        .filter_map(|row| {
            row.target.pane_id().map(|pane_id| {
                assert!(
                    row.target.endpoint == *state.endpoints.presented()
                        || row.target.endpoint == remote_id
                );
                (row.target.endpoint.clone(), pane_id)
            })
        })
        .collect::<Vec<_>>();
    let expected = [state.endpoints.presented().clone(), remote_id]
        .into_iter()
        .flat_map(|endpoint| {
            expected
                .iter()
                .copied()
                .map(move |pane| (endpoint.clone(), pane))
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn navigator_owns_search_mouse_selection_and_stable_target_focus() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let mut open = ClientShellInput::default();
    state.record_binding(
        &shepr_termio::input::KeybindAction::OpenNavigator,
        &mut open,
    );
    let navigator = state.compose(106, 30).expect("navigator overlay");
    let navigator_text = navigator
        .cells()
        .chunks(navigator.width() as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(navigator_text.contains("client-shell"));
    assert!(navigator_text.contains("terminal"));
    assert!(!navigator_text.contains("pane 1"));

    let search = state.drawn().navigator_search();
    let focus_search =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: search.x,
            row: search.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(focus_search.repaint);
    assert!(matches!(
        state.overlay,
        Some(Overlay::Navigator(NavigatorOverlay {
            search_focused: true,
            ..
        }))
    ));
    assert!(state.handle_input_bytes(b"client").actions.is_empty());
    let filtered = state.compose(106, 30).expect("filtered navigator");
    assert!(filtered.cursor().is_some_and(|cursor| cursor.visible));

    state.handle_input_bytes(b"\x1b");
    state.handle_input_bytes(b"a");
    state.compose(106, 30).expect("navigator rows");
    let pane_target = {
        let Overlay::Navigator(navigator) = state.overlay.as_ref().expect("navigator") else {
            panic!("expected navigator");
        };
        text::client_navigator_rows(&state.endpoints, state.endpoints.presented(), navigator)
            .iter()
            .find(|row| row.target.pane_id().is_some())
            .map(|row| row.target.clone())
            .expect("pane row")
    };
    let pane_rect = state
        .drawn()
        .navigator_rows()
        .find(|(_, target)| **target == pane_target)
        .map(|(rect, _)| rect)
        .expect("visible pane row");
    let select =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column: pane_rect.x + 6,
            row: pane_rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(select.repaint);
    let accept =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: pane_rect.x + 6,
            row: pane_rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
    // The pick goes through the runtime, which focuses an endpoint that owns
    // the presentation through the endpoint API.
    let [
        ClientShellAction::ActivateEndpoint(Location {
            endpoint: ClientEndpointId::Local,
            target,
        }),
    ] = &accept.actions[..]
    else {
        panic!("navigator pane click should be an explicit local pick");
    };
    let focus = state.focus_endpoint_target(*target);
    let [ClientShellAction::Endpoint { request, .. }] = &focus[..] else {
        panic!("navigator pane click should use endpoint API");
    };
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneFocus(target) if target.pane_id == crate::tests::test_pane_id("w1:p1")
    ));
    assert!(state.overlay.is_none());
}
