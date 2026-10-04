use crate::shell::config::ClientShellConfig;
use crate::shell::overlays::Overlay;
use crate::shell::overlays::context_menu::{
    ContextMenuAction, ContextMenuOverlay, ContextMenuTarget,
};
use crate::shell::overlays::rename::RenameTarget;
use crate::shell::sidebar::preferences;
use crate::shell::state::{ClientShellAction, ClientShellInput, ClientShellState};
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_config::ClientConfig;
use shepr_protocol::FrameData;
use shepr_protocol::command::EndpointCommand;
use shepr_termio::input::raw_input::RawInputEvent;

use shepr_protocol::{ClientShellWorkspace, SurfaceRect};
use shepr_surface::ratatui_conversion::FrameDataExt as _;

use crate::shell::tests::{rename_target, snapshot, surface};
use crate::tests::{test_pane_id, test_workspace_id};

#[test]
fn focused_workspace_change_reveals_new_workspace_in_full_sidebar() {
    let mut initial = snapshot();
    let template = initial.workspaces[0].clone();
    initial.workspaces = (1..=12)
        .map(|number| ClientShellWorkspace {
            workspace_id: test_workspace_id(&format!("w{number}")),
            label: format!("space-{number}"),
            branch: None,
            ..template.clone()
        })
        .collect();

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(initial));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("full sidebar");
    assert!(state.drawn().workspace_max_scroll() > 0);
    assert!(
        state
            .drawn()
            .workspaces()
            .all(|hit| hit.location.workspace_id() != Some(crate::tests::test_workspace_id("w12")))
    );

    let mut update = state.endpoints.active.snapshot().expect("snapshot").clone();
    update.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    update.focused_workspace_id = Some(test_workspace_id("w12"));
    let mut updated_surface = surface();
    updated_surface.projection_revision =
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    state.set_snapshot(Box::new(update));
    state.receive_pane_surface_from(
        updated_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 2).expect("zero-height workspace body");
    assert!(state.sidebar_scroll.workspace_reveal().focused_pending());
    state.compose(106, 20).expect("updated full sidebar");

    assert!(
        state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.workspace_id() == Some(crate::tests::test_workspace_id("w12")))
    );
}

fn workspaces_and_agents_snapshot(
    workspaces: usize,
    agents: usize,
) -> shepr_protocol::ClientShellSnapshot {
    let mut value = snapshot();
    let workspace_template = value.workspaces[0].clone();
    let pane_template = value.panes[0].clone();
    value.workspaces = (1..=workspaces)
        .map(|number| ClientShellWorkspace {
            workspace_id: test_workspace_id(&format!("w{number}")),
            label: format!("space-{number}"),
            branch: None,
            ..workspace_template.clone()
        })
        .collect();
    value.agents = (1..=agents)
        .map(|number| shepr_protocol::ClientShellAgent {
            pane_id: test_pane_id(&format!("w1:p{number}")),
            agent: Some(shepr_config::ConfigAgent::Pi),
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: shepr_protocol::AgentStatus::Idle,
            state_change_seq: shepr_test_fixtures::counter_at(1),
        })
        .collect();
    value.panes = value
        .agents
        .iter()
        .map(|agent| shepr_protocol::ClientShellPane {
            pane_id: agent.pane_id,
            ..pane_template.clone()
        })
        .collect();
    value
}

#[test]
fn collapsed_sidebar_keeps_a_reveal_until_its_workspace_area_is_visible() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.chrome.set_collapsed(true);
    state.set_snapshot(Box::new(workspaces_and_agents_snapshot(40, 0)));
    state.compose(106, 20).expect("collapsed sidebar");
    assert!(state.drawn().workspace_max_scroll() > 0);

    let mut update = state.endpoints.active.snapshot().expect("snapshot").clone();
    update.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    update.focused_workspace_id = Some(test_workspace_id("w40"));
    state.set_snapshot(Box::new(update));

    // No rows are left for the workspace area: the reveal has nothing to scroll.
    let _ = state.compose(106, 0);
    assert!(state.sidebar_scroll.workspace_reveal().focused_pending());

    state
        .compose(106, 20)
        .expect("collapsed sidebar at full height");
    assert!(
        state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.workspace_id() == Some(test_workspace_id("w40")))
    );
}

#[test]
fn an_empty_sidebar_body_keeps_both_list_scroll_positions() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(workspaces_and_agents_snapshot(40, 40)));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 28).expect("full sidebar");
    assert!(state.drawn().workspace_max_scroll() >= 3);
    assert!(state.drawn().agent_max_scroll() >= 2);

    state.sidebar_scroll.scroll_workspaces_to(3);
    state.sidebar_scroll.scroll_agents_to(2);
    state.compose(106, 28).expect("scrolled sidebar");
    assert_eq!(state.sidebar_scroll.workspace_start(), 3);
    assert_eq!(state.sidebar_scroll.agent_start(), 2);

    state.compose(106, 2).expect("empty sidebar bodies");
    state.compose(106, 28).expect("full sidebar again");
    assert_eq!(state.sidebar_scroll.workspace_start(), 3);
    assert_eq!(state.sidebar_scroll.agent_start(), 2);
}

#[test]
fn a_reveal_after_a_sidebar_toggle_uses_the_new_layout() {
    use shepr_termio::input::KeybindAction;

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.chrome.set_collapsed(true);
    state.set_snapshot(Box::new(workspaces_and_agents_snapshot(40, 0)));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 28).expect("collapsed sidebar");

    // One input batch: expand the sidebar, then switch to a workspace far down the list.
    // The reveal must be resolved against the expanded layout composed afterwards.
    let mut outcome = ClientShellInput::default();
    state.record_binding(&KeybindAction::ToggleSidebar, &mut outcome);
    state.record_binding(&KeybindAction::SwitchWorkspace(39), &mut outcome);
    state.compose(106, 28).expect("expanded sidebar");

    assert!(
        state
            .drawn()
            .workspaces()
            .any(|hit| hit.location.workspace_id() == Some(test_workspace_id("w40")))
    );
}

#[test]
fn client_owned_sidebar_dividers_resize_live() {
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
    state.compose(106, 30).expect("expanded sidebar");
    assert!(state.drawn().machines().next().is_none());
    let workspace_body = state.drawn().workspace_body();
    let needless_scroll =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: workspace_body.x,
            row: workspace_body.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert_eq!(state.drawn().workspace_max_scroll(), 0);
    assert_eq!(state.sidebar_scroll.workspace_start(), 0);
    assert!(!needless_scroll.repaint);
    let width_divider = state.drawn().sidebar_divider();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: width_divider.x,
        row: width_divider.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);
    let resize =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 31,
            row: width_divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert_eq!(state.chrome.width(), 32);
    assert_eq!(
        state.chrome.preferences().sidebar_width,
        Some(state.chrome.width())
    );
    assert!(resize.repaint);
    // The endpoint is resized once, on release, not once per column crossed.
    assert!(!resize.resize);
    let waiting_frame = state.compose(106, 30).expect("waiting for resized surface");
    let waiting_text: String = waiting_frame
        .cells()
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect();
    assert!(
        waiting_text.contains(" workspaces"),
        "local sidebar must keep spaces while resizing: {waiting_text}"
    );
    assert!(!waiting_text.contains(" machines"));
    assert!(!waiting_text.contains("Select a connected machine"));
    // The retained surface stays on screen while the drag is in progress.
    assert!(waiting_text.contains("LIVE"));
    assert!(state.pane_surface().is_some());
    assert!(!state.pane_hits().is_empty());
    assert!(state.drawn().machines().next().is_none());
    assert_eq!(state.drawn().sidebar_divider().x, 31);
    assert_eq!(
        state
            .drawn()
            .workspaces()
            .next()
            .expect("a workspace hit")
            .location
            .workspace_id()
            .expect("workspace hit names a workspace")
            .to_string(),
        "w1"
    );

    let next_resize =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 32,
            row: width_divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(!next_resize.resize);
    state.compose(106, 30).expect("continued resize");
    assert_eq!(state.drawn().sidebar_divider().x, 32);
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 32,
            row: width_divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(release.resize);
    assert!(state.pointer.chrome_drag.is_none());

    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let recovered_frame = state.compose(106, 30).expect("resized sidebar");
    let recovered_text: String = recovered_frame
        .cells()
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect();
    assert!(recovered_text.contains(" workspaces"));
    assert!(recovered_text.contains("LIVE"));
    assert!(!state.pane_hits().is_empty());
    let section_divider = state.drawn().section_divider();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: section_divider.x + 2,
        row: section_divider.y,
        modifiers: KeyModifiers::empty(),
    })]);
    let split = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: section_divider.x + 2,
        row: 20,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(state.chrome.split().get() > 0.6);
    assert!(split.repaint);
    assert!(!split.resize);
}

#[test]
fn context_menus_capture_stable_targets_and_route_actions() {
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
    state.compose(106, 20).expect("composed frame");

    let workspace = state
        .drawn()
        .workspaces()
        .next()
        .expect("a workspace hit")
        .rect;
    let open_workspace_menu =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: workspace.x + 2,
            row: workspace.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(open_workspace_menu.actions.is_empty());
    assert!(matches!(
        state.overlay,
        Some(Overlay::ContextMenu(ContextMenuOverlay {
            target: ContextMenuTarget::Workspace { ref workspace_id, .. },
            ..
        })) if workspace_id == &crate::tests::test_workspace_id("w1")
    ));
    let workspace_items = match state.overlay.as_ref() {
        Some(Overlay::ContextMenu(menu)) => menu.items(),
        _ => panic!("workspace context menu"),
    };
    assert!(
        workspace_items
            .iter()
            .any(|item| item.action == ContextMenuAction::Close)
    );
    state.compose(106, 20).expect("workspace context menu");
    let rename = state.drawn().context_menu_rows()[0].0;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rename.x + 1,
        row: rename.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        rename_target(&state),
        Some(RenameTarget::Workspace { workspace_id })
            if workspace_id == &crate::tests::test_workspace_id("w1")
    ));

    state.handle_input_bytes(b"\x1b");
    assert!(state.overlay.is_none());
    state.compose(106, 20).expect("composed frame");
    let pane = state.pane_hits()[0].rect;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: pane.x + 1,
        row: pane.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.compose(106, 20).expect("pane context menu");
    let split_index = match state.overlay.as_ref() {
        Some(Overlay::ContextMenu(menu)) => menu
            .items()
            .iter()
            .position(|item| item.action == ContextMenuAction::SplitRight)
            .expect("split right item"),
        _ => panic!("pane context menu"),
    };
    let split = state.drawn().context_menu_rows()[split_index].0;
    let outcome =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: split.x + 1,
            row: split.y,
            modifiers: KeyModifiers::empty(),
        })]);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("pane split context action should use endpoint API");
    };
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneSplit(params)
            if params.pane_id == crate::tests::test_pane_id("w1:p1")
                && params.direction == shepr_protocol::command::SplitDirection::Right
    ));
}

#[test]
fn global_menu_opens_from_sidebar_and_routes_client_actions() {
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
    state.compose(106, 30).expect("shell frame");
    let launcher = state.drawn().global_launcher();
    assert_ne!(launcher, Rect::default());

    let open = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: launcher.x,
        row: launcher.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(open.repaint);
    let menu = state.compose(106, 30).expect("global menu");
    let text = menu
        .cells()
        .chunks(menu.width() as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("keybinds"));
    assert!(text.contains("detach"));

    let keybinds = state.drawn().global_menu_rows()[0].0;
    let help = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: keybinds.x,
        row: keybinds.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(help.actions.is_empty());
    assert!(matches!(state.overlay, Some(Overlay::Help(_))));

    // The launcher's toggle reopens the menu over Help; Down highlights detach.
    state.toggle_global_menu();
    state.handle_input_bytes(b"\x1b[B");
    let detach = state.handle_input_bytes(b"\r");
    assert!(detach.detach);
    assert!(state.overlay.is_none());
}

#[test]
fn lost_sidebar_drag_release_still_resizes_on_the_next_press() {
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
    state.compose(106, 30).expect("expanded sidebar");
    let divider = state.drawn().sidebar_divider();
    let mouse = |kind, column| {
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row: divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })
    };
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        divider.x,
    )]);
    let drag = state.handle_raw_events(vec![mouse(MouseEventKind::Drag(MouseButton::Left), 31)]);
    assert!(!drag.resize);
    // The release happened outside the terminal; the next press elsewhere settles the drag.
    let press = state.handle_raw_events(vec![mouse(MouseEventKind::Down(MouseButton::Left), 80)]);
    assert!(press.resize);
    assert!(state.pointer.chrome_drag.is_none());
}

#[test]
fn lost_sidebar_drag_release_still_persists_the_width_on_the_next_press() {
    let scratch = shepr_test_support::ScratchDir::new("lost-release-prefs");
    let path = scratch.join("preferences.json");
    let config = ClientShellConfig::from_config(&ClientConfig::default())
        .with_preferences_path(path.clone());
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 30).expect("expanded sidebar");
    let divider = state.drawn().sidebar_divider();
    let mouse = |kind, column| {
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row: divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })
    };
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        divider.x,
    )]);
    state.handle_raw_events(vec![mouse(MouseEventKind::Drag(MouseButton::Left), 31)]);
    assert!(preferences::load(&path).is_none_or(|stored| stored.sidebar_width.is_none()));
    // No release arrives; a press elsewhere settles the drag.
    state.handle_raw_events(vec![mouse(MouseEventKind::Down(MouseButton::Left), 80)]);
    assert!(state.pointer.chrome_drag.is_none());
    let stored = preferences::load(&path).expect("preferences stored");
    assert_eq!(stored.sidebar_width, Some(state.chrome.width()));
}

#[test]
fn focus_loss_persists_a_sidebar_drag_but_keeps_it_for_its_release() {
    let scratch = shepr_test_support::ScratchDir::new("focus-loss-prefs");
    let path = scratch.join("preferences.json");
    let config = ClientShellConfig::from_config(&ClientConfig::default())
        .with_preferences_path(path.clone());
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 30).expect("expanded sidebar");
    let divider = state.drawn().sidebar_divider();
    let mouse = |kind, column| {
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row: divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })
    };
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        divider.x,
    )]);
    state.handle_raw_events(vec![mouse(MouseEventKind::Drag(MouseButton::Left), 31)]);
    state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    let stored = preferences::load(&path).expect("preferences stored on focus loss");
    assert_eq!(stored.sidebar_width, Some(state.chrome.width()));
    assert!(
        state.pointer.chrome_drag.is_some(),
        "the drag stays recorded so a release that still arrives finishes it"
    );
}

#[test]
fn oversized_retained_surface_is_clipped_with_its_hits() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    // A surface produced for a pane area far larger than the one composed below, as after a
    // resize or sidebar toggle before the resized surface arrives.
    let lines = (0..60).map(|_| "x".repeat(200)).collect::<Vec<_>>();
    let mut oversized = surface();
    oversized.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &Buffer::with_lines(lines.iter().map(String::as_str)),
        None,
        &[],
    )
    .expect("test buffer is a valid frame");
    let full = SurfaceRect {
        x: 0,
        y: 0,
        width: 200,
        height: 60,
    };
    oversized.panes[0].rect = full;
    oversized.panes[0].inner_rect = full;
    oversized.panes[0].pixel_mouse = shepr_term::mouse::PanePixelMouse::new(
        true,
        shepr_core::geometry::PanePixelExtent::new(
            shepr_core::geometry::GridSize::clamped(200, 60),
            1600,
            960,
        ),
    );
    let mut off_screen = oversized.panes[0].clone();
    off_screen.pane_id = test_pane_id("w1:p2");
    let far = SurfaceRect {
        x: 190,
        y: 0,
        width: 10,
        height: 60,
    };
    off_screen.rect = far;
    off_screen.inner_rect = far;
    oversized.panes.push(off_screen);
    state.receive_pane_surface_from(
        oversized,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );

    state.compose(106, 30).expect("clipped frame");
    let area = state.layout(106, 30).pane_surface;
    assert_eq!(
        state.pane_hits().len(),
        1,
        "a pane with no visible cell has no hit"
    );
    let hit = &state.pane_hits()[0];
    assert_eq!(hit.pane_id.to_string(), "w1:p1");
    assert_eq!(hit.inner_rect, area);
    assert_eq!(
        hit.presented, None,
        "a clipped pane cannot map pixels, so it has no presented grid"
    );
    assert!(state.drawn().pane_splits().is_empty());
}

#[test]
fn selection_without_a_previous_surface_is_dropped_by_the_next_surface() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.mouse_selection.selection = Some(shepr_term::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_term::Point::new(shepr_term::AbsRow(0), 0),
        shepr_term::Point::new(shepr_term::AbsRow(0), 2),
    ));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert!(state.mouse_selection.selection.is_none());
}
