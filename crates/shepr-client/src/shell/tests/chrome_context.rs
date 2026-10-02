use crate::shell::overlays::preferences;
use crate::shell::state::{
    ClientContextMenuAction, ClientContextMenuTarget, ClientRenameTarget, ClientShellAction,
    ClientShellConfig, ClientShellOverlay, ClientShellState,
};
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_config::ClientConfig;
use shepr_protocol::FrameData;
use shepr_protocol::command::EndpointCommand;
use shepr_termio::input::raw_input::RawInputEvent;

use crate::shell::state::{ClientContextMenuOverlay, ClientGlobalMenuOverlay, ClientRenameOverlay};
use shepr_protocol::{ClientShellWorkspace, SurfaceRect};

use crate::shell::tests::{snapshot, surface};
use crate::tests::{test_pane_id, test_workspace_id};

#[test]
fn focused_workspace_change_reveals_new_workspace_in_full_sidebar() {
    let mut initial = snapshot();
    let template = initial.workspaces[0].clone();
    initial.workspaces = (1..=12)
        .map(|number| ClientShellWorkspace {
            workspace_id: test_workspace_id(&format!("w{number}")),
            number,
            label: format!("space-{number}"),
            branch: None,
            focused: number == 1,
            ..template.clone()
        })
        .collect();

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(initial));
    state.receive_pane_surface(surface());
    state.compose(106, 20).expect("full sidebar");
    assert!(state.hits.workspace_max_scroll > 0);
    assert!(
        state
            .hits
            .workspaces
            .iter()
            .all(|hit| hit.workspace_id != "w12")
    );

    let mut update = state.snapshot.as_deref().expect("snapshot").clone();
    update.revision = shepr_protocol::ProjectionRevision::new(2);
    update.focused_workspace_id = Some(test_workspace_id("w12"));
    for workspace in &mut update.workspaces {
        workspace.focused = workspace.workspace_id == "w12";
    }
    let mut updated_surface = surface();
    updated_surface.projection_revision = shepr_protocol::ProjectionRevision::new(2);
    state.set_snapshot(Box::new(update));
    state.receive_pane_surface(updated_surface);
    state.compose(106, 2).expect("zero-height workspace body");
    assert!(state.reveal_focused_workspace);
    state.compose(106, 20).expect("updated full sidebar");

    assert!(
        state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.workspace_id == "w12")
    );
}

#[test]
fn client_owned_sidebar_dividers_resize_live() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.compose(106, 30).expect("expanded sidebar");
    assert!(state.hits.machines.is_empty());
    let workspace_body = state.hits.workspace_body;
    let needless_scroll =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: workspace_body.x,
            row: workspace_body.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert_eq!(state.hits.workspace_max_scroll, 0);
    assert_eq!(state.workspace_scroll, 0);
    assert!(!needless_scroll.repaint);
    let width_divider = state.hits.sidebar_divider;
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
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect();
    assert!(
        waiting_text.contains(" spaces"),
        "local sidebar must keep spaces while resizing: {waiting_text}"
    );
    assert!(!waiting_text.contains(" machines"));
    assert!(!waiting_text.contains("Select a connected machine"));
    // The retained surface stays on screen while the drag is in progress.
    assert!(waiting_text.contains("LIVE"));
    assert!(state.pane_surface().is_some());
    assert!(!state.hits.panes.is_empty());
    assert!(state.hits.machines.is_empty());
    assert_eq!(state.hits.sidebar_divider.x, 31);
    assert_eq!(state.hits.workspaces[0].workspace_id, "w1");

    let next_resize =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 32,
            row: width_divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(!next_resize.resize);
    state.compose(106, 30).expect("continued resize");
    assert_eq!(state.hits.sidebar_divider.x, 32);
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 32,
            row: width_divider.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(release.resize);
    assert!(state.chrome_drag.is_none());

    state.receive_pane_surface(surface());
    let recovered_frame = state.compose(106, 30).expect("resized sidebar");
    let recovered_text: String = recovered_frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect();
    assert!(recovered_text.contains(" spaces"));
    assert!(recovered_text.contains("LIVE"));
    assert!(!state.hits.panes.is_empty());
    let section_divider = state.hits.sidebar_section_divider;
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
    state.receive_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");

    let workspace = state.hits.workspaces[0].rect;
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
        Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Workspace { ref workspace_id, .. },
            ..
        })) if workspace_id == "w1"
    ));
    let workspace_items = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => menu.items(),
        _ => panic!("workspace context menu"),
    };
    assert!(
        workspace_items
            .iter()
            .any(|item| item.action == ClientContextMenuAction::Close)
    );
    state.compose(106, 20).expect("workspace context menu");
    let rename = state.hits.context_menu_rows[0].0;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rename.x + 1,
        row: rename.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            target: ClientRenameTarget::Workspace { ref workspace_id },
            ..
        })) if workspace_id == "w1"
    ));

    state.overlay = None;
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].rect;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: pane.x + 1,
        row: pane.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.compose(106, 20).expect("pane context menu");
    let split_index = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => menu
            .items()
            .iter()
            .position(|item| item.action == ClientContextMenuAction::SplitRight)
            .expect("split right item"),
        _ => panic!("pane context menu"),
    };
    let split = state.hits.context_menu_rows[split_index].0;
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
            if params.pane_id == "w1:p1"
                && params.direction == shepr_protocol::command::SplitDirection::Right
    ));
}

#[test]
fn global_menu_opens_from_sidebar_and_routes_client_actions() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.compose(106, 30).expect("shell frame");
    let launcher = state.hits.global_launcher;
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
        .cells
        .chunks(menu.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("keybinds"));
    assert!(text.contains("detach"));

    let keybinds = state.hits.global_menu_rows[0].0;
    let help = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: keybinds.x,
        row: keybinds.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(help.actions.is_empty());
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));

    state.overlay = Some(ClientShellOverlay::GlobalMenu(ClientGlobalMenuOverlay {
        highlighted: 1,
        launcher,
    }));
    let detach = state.handle_input_bytes(b"\r");
    assert!(detach.detach);
    assert!(state.overlay.is_none());
}

#[test]
fn lost_sidebar_drag_release_still_resizes_on_the_next_press() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.compose(106, 30).expect("expanded sidebar");
    let divider = state.hits.sidebar_divider;
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
    assert!(state.chrome_drag.is_none());
}

#[test]
fn lost_sidebar_drag_release_still_persists_the_width_on_the_next_press() {
    let scratch = shepr_test_support::ScratchDir::new("lost-release-prefs");
    let path = scratch.join("preferences.json");
    let config = ClientShellConfig::from_config(&ClientConfig::default())
        .with_preferences_path(path.clone());
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.compose(106, 30).expect("expanded sidebar");
    let divider = state.hits.sidebar_divider;
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
    assert!(state.chrome_drag.is_none());
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
    state.receive_pane_surface(surface());
    state.compose(106, 30).expect("expanded sidebar");
    let divider = state.hits.sidebar_divider;
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
        state.chrome_drag.is_some(),
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
    );
    let full = SurfaceRect {
        x: 0,
        y: 0,
        width: 200,
        height: 60,
    };
    oversized.panes[0].rect = full;
    oversized.panes[0].inner_rect = full;
    oversized.panes[0].pixel_width = 1600;
    oversized.panes[0].pixel_height = 960;
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
    state.receive_pane_surface(oversized);

    state.compose(106, 30).expect("clipped frame");
    let area = state.layout(106, 30).pane_surface;
    assert_eq!(
        state.hits.panes.len(),
        1,
        "a pane with no visible cell has no hit"
    );
    let hit = &state.hits.panes[0];
    assert_eq!(hit.pane_id, "w1:p1");
    assert_eq!(hit.inner_rect, area);
    assert_eq!((hit.pixel_width, hit.pixel_height), (0, 0));
    assert!(state.hits.pane_splits.is_empty());
}

#[test]
fn selection_without_a_previous_surface_is_dropped_by_the_next_surface() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.mouse_selection.selection = Some(shepr_vt::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 2),
    ));
    state.receive_pane_surface(surface());
    assert!(state.mouse_selection.selection.is_none());
}
