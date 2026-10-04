use crate::endpoint::EndpointFailureStatus;
use crate::shell::config::ClientShellConfig;
use crate::shell::ledger::DropReason;
use crate::shell::navigation::location::{Location, LocationTarget};
use crate::shell::overlays::Overlay;
use crate::shell::state::{
    ClientShellAction, ClientShellEndpointError, ClientShellInput, ClientShellMode,
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use shepr_config::ClientConfig;
use shepr_protocol::AgentStatus;
use shepr_protocol::command::{EndpointCommand, EndpointReply};
use shepr_protocol::{ClientShellAgent, ClientShellPane, ClientShellSnapshot, SurfaceRect};
use shepr_termio::input::raw_input::RawInputEvent;

use crate::shell::state::ClientShellState;

use crate::endpoint::ClientEndpointId;

use crossterm::event::MouseEvent;

use crate::shell::tests::{agent, remote_machine, state_with_remote, state_with_remote_config};
use crate::shell::tests::{cell_bg, enter_navigation, preview_key, snapshot, surface};
use ratatui::layout::Rect;

use crate::tests::test_workspace_id;

/// A client config on the terminal 16-color theme.
fn terminal_theme() -> ClientConfig {
    let mut config = ClientConfig::default();
    config.theme.name = Some("terminal".into());
    config
}

fn workspaces(count: usize) -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.workspaces = (1..=count)
        .map(|number| {
            let mut workspace = projected.workspaces[0].clone();
            workspace.workspace_id = test_workspace_id(&format!("w{number}"));
            workspace
        })
        .collect();
    projected
}

fn navigation_state(
    mut projected: ClientShellSnapshot,
    config: &ClientConfig,
) -> (ClientShellState, ClientEndpointId) {
    let (mut state, remote) = state_with_remote_config(config);
    state.set_snapshot(Box::new(projected.clone()));
    projected.boot_id = crate::tests::test_boot_id("remote-boot");
    state.set_endpoint_snapshot(&remote, Box::new(projected));
    (state, remote)
}

#[test]
fn pane_scrollbar_click_clears_a_workspace_preview_when_leaving_navigation() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(workspaces(2)));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
        2,
        shepr_term::AbsRow(0),
    ));
    pane_surface.panes[0].scrollbar_rect = Some(SurfaceRect {
        x: 3,
        y: 0,
        width: 1,
        height: 2,
    });
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(100, 28).expect("pane frame");
    enter_navigation(&mut state);
    preview_key(&mut state, b"\x1b[B");
    assert_selected(&state, &ClientEndpointId::Local, "w2");

    let track = state.pane_hits()[0]
        .scrollbar_rect
        .expect("pane scrollbar hit");
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: track.x,
        row: track.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    assert!(state.mode.preview().is_none());
    assert_eq!(
        state.workspace_action_id().as_ref(),
        Some(&crate::tests::test_workspace_id("w1"))
    );
}

fn assert_selected(state: &ClientShellState, endpoint: &ClientEndpointId, workspace: &str) {
    assert_eq!(
        state.mode.preview().cloned(),
        state.navigation_target(endpoint, &shepr_test_fixtures::id(workspace))
    );
}

fn workspace_rect(state: &ClientShellState, endpoint: &ClientEndpointId, workspace: &str) -> Rect {
    state
        .drawn()
        .workspaces()
        .find(|hit| {
            &hit.location.endpoint == endpoint
                && hit
                    .location
                    .workspace_id()
                    .is_some_and(|id| id.to_string() == workspace)
        })
        .map(|hit| hit.rect)
        .expect("visible workspace")
}

#[test]
fn local_navigation_highlight_stays_visible_with_terminal_theme() {
    use ratatui::style::Color;

    for compact in [false, true] {
        for selection_bg in [Color::Reset, Color::Rgb(70, 63, 93)] {
            let mut values = terminal_theme();
            if let Color::Rgb(r, g, b) = selection_bg {
                values.theme.custom = Some(shepr_config::CustomThemeColors {
                    selection_bg: Some(format!("#{r:02x}{g:02x}{b:02x}")),
                    ..Default::default()
                });
            }
            let config = ClientShellConfig::from_config(&values);
            let expected_bg = if selection_bg == Color::Reset {
                config.palette.active_row_bg
            } else {
                selection_bg
            };
            let mut state = ClientShellState::new(config);
            state.set_snapshot(Box::new(workspaces(3)));
            state.receive_pane_surface_from(
                surface(),
                state
                    .endpoints
                    .active
                    .generation()
                    .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
            );
            state.chrome.set_collapsed(compact);
            state.compose(100, 28).expect("test precondition");
            enter_navigation(&mut state);

            for workspace_id in ["w1", "w2"] {
                assert_selected(&state, &ClientEndpointId::Local, workspace_id);
                let frame = state.compose(100, 28).expect("test precondition");
                let selected = workspace_rect(&state, &ClientEndpointId::Local, workspace_id);
                for y in selected.y..selected.bottom() {
                    for x in selected.x..selected.right() {
                        assert_eq!(
                            cell_bg(&frame, (x, y)),
                            expected_bg,
                            "compact={compact}, {workspace_id}, ({x}, {y})"
                        );
                    }
                }
                let untouched = workspace_rect(&state, &ClientEndpointId::Local, "w3");
                assert_ne!(cell_bg(&frame, (untouched.x, untouched.y)), expected_bg);
                if workspace_id != "w1" {
                    let focused = workspace_rect(&state, &ClientEndpointId::Local, "w1");
                    assert_eq!(
                        cell_bg(&frame, (focused.x, focused.y)),
                        if selection_bg == Color::Reset {
                            state.config.palette.sidebar_bg
                        } else {
                            state.config.palette.active_row_bg
                        }
                    );
                    assert_ne!(cell_bg(&frame, (focused.x, focused.y)), expected_bg);
                }
                preview_key(&mut state, b"\x1b[B");
            }
            assert_eq!(
                state
                    .endpoints
                    .active
                    .shared_snapshot()
                    .expect("test precondition")
                    .focused_workspace_id
                    .as_ref(),
                Some(&crate::tests::test_workspace_id("w1"))
            );
            preview_key(&mut state, b"\x1b");
            let frame = state.compose(100, 28).expect("test precondition");
            let focused = workspace_rect(&state, &ClientEndpointId::Local, "w1");
            assert_eq!(
                cell_bg(&frame, (focused.x, focused.y)),
                state.config.palette.active_row_bg
            );
            let cancelled = workspace_rect(&state, &ClientEndpointId::Local, "w3");
            assert_eq!(
                cell_bg(&frame, (cancelled.x, cancelled.y)),
                state.config.palette.sidebar_bg
            );
        }
    }
}

#[test]
fn navigation_highlights_only_the_preview_and_activates_on_enter() {
    for (compact, cols) in [(true, 100), (false, 100), (false, 44)] {
        for terminal_theme in [false, true] {
            let config = if terminal_theme {
                self::terminal_theme()
            } else {
                ClientConfig::default()
            };
            let (mut state, remote) = navigation_state(workspaces(2), &config);
            state.chrome.set_collapsed(compact);
            state.compose(cols, 28).expect("test precondition");
            enter_navigation(&mut state);
            for (endpoint, collision, steps) in [
                (&ClientEndpointId::Local, &remote, 1),
                (&remote, &ClientEndpointId::Local, 2),
            ] {
                for _ in 0..steps {
                    preview_key(&mut state, b"\x1b[B");
                }
                assert_selected(&state, endpoint, "w2");
                let frame = state.compose(cols, 28).expect("test precondition");
                let selected = workspace_rect(&state, endpoint, "w2");
                let other = workspace_rect(&state, collision, "w2");
                let focused = workspace_rect(&state, &ClientEndpointId::Local, "w1");
                let palette = &state.config.palette;
                let color = if cols == 44 {
                    palette.surface0
                } else {
                    palette.selection_bg
                };
                let color = if color == ratatui::style::Color::Reset {
                    palette.active_row_bg
                } else {
                    color
                };
                assert_eq!(cell_bg(&frame, (selected.x + 2, selected.y)), color);
                assert_ne!(cell_bg(&frame, (other.x + 2, other.y)), color);
                assert_eq!(
                    cell_bg(&frame, (focused.x + 2, focused.y)),
                    if terminal_theme {
                        if cols == 44 {
                            palette.panel_bg
                        } else {
                            palette.sidebar_bg
                        }
                    } else if cols == 44 {
                        palette.surface_dim
                    } else {
                        palette.active_row_bg
                    }
                );
            }
            assert_eq!(
                state
                    .endpoints
                    .active
                    .snapshot()
                    .expect("test precondition")
                    .boot_id,
                crate::tests::test_boot_id("boot-1")
            );
            assert_eq!(
                state
                    .endpoints
                    .active
                    .shared_snapshot()
                    .expect("test precondition")
                    .focused_workspace_id
                    .as_ref(),
                Some(&crate::tests::test_workspace_id("w1"))
            );
            assert_eq!(
                state.pane_surface().expect("test precondition").boot_id,
                crate::tests::test_boot_id("boot-1")
            );
            let enter = state.handle_input_bytes(b"\r");
            assert!(enter.requests.is_empty());
            assert!(
                matches!(enter.actions.as_slice(), [ClientShellAction::ActivateEndpoint(Location {
                endpoint: endpoint_id, target: LocationTarget::Workspace(id),
            })] if endpoint_id == &remote && id == &test_workspace_id("w2"))
            );
            assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);
            assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
            assert!(state.mode.preview().is_none());
        }
    }
}

#[test]
fn foreign_preview_blocks_keyboard_actions_but_keeps_active_action_context() {
    for confirm in [false, true] {
        let mut config = ClientConfig::default();
        config.ui.confirm_close = confirm;
        config.ui.prompt_new_workspace_name = false;
        let (mut state, remote) = state_with_remote_config(&config);
        state.compose(100, 28).expect("test precondition");
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        for key in [
            b"W".as_slice(),
            b"D",
            b"\x1b[D",
            b"\x1b[C",
            b"\t",
            b"1",
            b"c",
            b"N",
        ] {
            preview_key(&mut state, key);
            assert!(state.overlay.is_none());
            assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
        }
        assert_selected(&state, &remote, "w1");
        let mut remote_snapshot = workspaces(2);
        remote_snapshot.boot_id = crate::tests::test_boot_id("remote-boot");
        // A later snapshot over the same connection.
        state.set_endpoint_snapshot_for_generation(
            &remote,
            crate::tests::test_generation(1),
            Box::new(remote_snapshot),
        );
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &remote, "w2");
        assert_eq!(
            state.workspace_action_id().as_ref(),
            Some(&crate::tests::test_workspace_id("w1"))
        );
        // Without the name prompt, creation follows the active workspace at once.
        let mut create = ClientShellInput::default();
        state.record_binding(
            &shepr_termio::input::KeybindAction::NewWorkspace,
            &mut create,
        );
        assert!(
            matches!(create.actions.as_slice(), [ClientShellAction::Endpoint { endpoint_id: ClientEndpointId::Local, request, .. }]
            if matches!(&request.command, EndpointCommand::WorkspaceCreate(params) if matches!(&params.source, shepr_protocol::command::WorkspaceCreateSource::Follow(id) if id == &test_workspace_id("w1"))))
        );
        preview_key(&mut state, b"\x1b");
        assert!(state.mode.preview().is_none());
        assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);
        assert!(state.activate_endpoint_projection(&remote));
        enter_navigation(&mut state);
        preview_key(&mut state, b"W");
        assert!(matches!(state.overlay, Some(Overlay::Rename(_))));
    }
}

#[test]
fn blocked_preview_notice_names_the_configured_open_key() {
    let (mut state, _) = state_with_remote();
    state.compose(100, 28).expect("test precondition");
    enter_navigation(&mut state);
    preview_key(&mut state, b"\x1b[B");
    preview_key(&mut state, b"W");
    // The hint is the configured key's label, lowercase like every other
    // key label, so a rebound navigate_open_workspace is named correctly.
    let body = &state
        .notices
        .visible()
        .expect("blocked preview notice")
        .body;
    assert!(body.contains("press enter before"), "{body}");
}

#[test]
fn navigate_back_matches_its_configured_modifiers_exactly() {
    let (mut state, _) = state_with_remote();
    state.compose(100, 28).expect("test precondition");
    enter_navigation(&mut state);
    // Alt+Esc (kitty encoding) is not the configured "esc": navigate_back
    // matches exactly like every other binding, so navigate mode stays open.
    let alt_esc = state.handle_input_bytes(b"\x1b[27;3u");
    assert!(alt_esc.actions.is_empty() && alt_esc.requests.is_empty());
    assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
    preview_key(&mut state, b"\x1b[27u");
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
}

#[test]
fn empty_workspace_navigation_enter_exits_without_focusing() {
    let (mut state, _) = state_with_remote();
    let mut empty = workspaces(0);
    empty.panes.clear();
    empty.focused_workspace_id = None;
    empty.focused_pane_id = None;
    state.set_snapshot(Box::new(empty));
    enter_navigation(&mut state);
    assert!(state.mode.preview().is_none());
    let enter = state.handle_input_bytes(b"\r");
    assert!(enter.actions.is_empty() && enter.requests.is_empty() && enter.repaint);
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
}

#[test]
fn mouse_clicks_cancel_remote_workspace_navigation() {
    for pane in [false, true] {
        let (mut state, _) = state_with_remote();
        state.compose(100, 28).expect("test precondition");
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        state.compose(100, 28).expect("test precondition");
        let rect = if pane {
            state.pane_hits()[0].inner_rect
        } else {
            workspace_rect(&state, &ClientEndpointId::Local, "w1")
        };
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
                kind,
                column: rect.x + 2,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            })]);
        }
        assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
        assert!(state.mode.preview().is_none());
        enter_navigation(&mut state);
        assert_selected(&state, &ClientEndpointId::Local, "w1");
    }
}

#[test]
fn foreign_preview_survives_local_updates_and_rejects_stale_enter() {
    for invalidation in ["offline", "deleted", "boot", "generation"] {
        let (mut state, remote_id) = state_with_remote();
        let mut remote = workspaces(2);
        remote.boot_id = crate::tests::test_boot_id("remote-boot");
        state.set_endpoint_snapshot_for_generation(
            &remote_id,
            crate::tests::test_generation(7),
            Box::new(remote.clone()),
        );
        state.compose(100, 28).expect("test precondition");
        enter_navigation(&mut state);
        for _ in 0..2 {
            preview_key(&mut state, b"\x1b[B");
        }
        assert_selected(&state, &remote_id, "w2");
        let selected = state.mode.preview().cloned();
        remote.revision = remote.revision.checked_next().expect("test precondition");
        state.set_endpoint_snapshot_for_generation(
            &remote_id,
            crate::tests::test_generation(7),
            Box::new(remote.clone()),
        );
        assert_eq!(state.mode.preview().cloned(), selected);
        assert!(state.navigation_target_valid(selected.as_ref().expect("test precondition")));
        let mut local = snapshot();
        local.revision = local.revision.checked_next().expect("test precondition");
        state.set_snapshot(Box::new(local));
        assert_eq!(state.mode.preview().cloned(), selected);
        match invalidation {
            "offline" => state.set_endpoint_status(&remote_id, EndpointFailureStatus::Reconnecting),
            "deleted" => {
                remote.revision = remote.revision.checked_next().expect("test precondition");
                remote.workspaces.pop();
                state.set_endpoint_snapshot_for_generation(
                    &remote_id,
                    crate::tests::test_generation(7),
                    Box::new(remote),
                );
            }
            "boot" => {
                remote.boot_id = crate::tests::test_boot_id("restarted-remote");
                state.set_endpoint_snapshot_for_generation(
                    &remote_id,
                    crate::tests::test_generation(7),
                    Box::new(remote),
                );
            }
            "generation" => {
                state.cache_endpoint_snapshot_for_generation(
                    &remote_id,
                    crate::tests::test_generation(8),
                    Box::new(remote),
                );
            }
            _ => unreachable!(),
        }
        preview_key(&mut state, b"\r");
        assert_eq!(*state.active_endpoint_id(), ClientEndpointId::Local);
        assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
        assert!(state.notices.visible().is_some());
        assert!(!state.navigation_target_valid(state.mode.preview().expect("test precondition")));
        preview_key(&mut state, b"\x1b[B");
        assert!(state.navigation_target_valid(state.mode.preview().expect("test precondition")));
    }
}

#[test]
fn active_preview_is_not_retargeted_by_deletion_or_reboot() {
    for invalidation in ["deleted", "boot", "generation"] {
        for confirm in [false, true] {
            let mut config = ClientConfig::default();
            config.ui.confirm_close = confirm;
            let (mut state, _) = state_with_remote_config(&config);
            let mut local = workspaces(2);
            state.set_endpoint_snapshot_for_generation(
                &ClientEndpointId::Local,
                crate::tests::test_generation(7),
                Box::new(local.clone()),
            );
            state.receive_pane_surface_from(
                surface(),
                state
                    .endpoints
                    .active
                    .generation()
                    .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
            );
            state.compose(100, 28).expect("test precondition");
            enter_navigation(&mut state);
            preview_key(&mut state, b"\x1b[B");
            assert_selected(&state, &ClientEndpointId::Local, "w2");
            let selected = state.mode.preview().cloned();
            match invalidation {
                "boot" => local.boot_id = crate::tests::test_boot_id("new-local-boot"),
                "deleted" => {
                    local.revision = local.revision.checked_next().expect("test precondition");
                    local.workspaces.pop();
                }
                _ => {}
            }
            let generation =
                crate::tests::test_generation(if invalidation == "generation" { 8 } else { 7 });
            state.set_endpoint_snapshot_for_generation(
                &ClientEndpointId::Local,
                generation,
                Box::new(local),
            );
            assert_eq!(state.mode.preview().cloned(), selected);
            preview_key(&mut state, b"\r");
            assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
            assert!(state.notices.visible().is_some());
            for key in [b"W", b"D"] {
                preview_key(&mut state, key);
            }
            assert!(state.overlay.is_none());
            assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
            assert_eq!(
                state.workspace_action_id().as_ref(),
                Some(&crate::tests::test_workspace_id("w1"))
            );
        }
    }
}

#[test]
fn aggregate_navigation_reveals_overflow_and_preserves_order() {
    for compact in [true, false] {
        let (mut state, remote_id) = state_with_remote();
        let mut remote = workspaces(15);
        remote.boot_id = crate::tests::test_boot_id("remote-boot");
        state.set_endpoint_snapshot(&remote_id, Box::new(remote));
        state.chrome.set_collapsed(compact);
        state.endpoints.collapsed.insert(remote_id.clone());
        state.compose(100, 18).expect("test precondition");
        enter_navigation(&mut state);
        for number in 1..=15 {
            preview_key(&mut state, b"\x1b[B");
            let id = format!("w{number}");
            assert_selected(&state, &remote_id, &id);
            state.compose(100, 18).expect("test precondition");
            workspace_rect(&state, &remote_id, &id);
        }
        assert!(!state.endpoints.collapsed.contains(&remote_id));
        // Navigation wraps from the last remote workspace back to the first.
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w1");
        preview_key(&mut state, b"\x1b[A");
        assert_selected(&state, &remote_id, "w15");
        state.set_endpoint_status(&remote_id, EndpointFailureStatus::Reconnecting);
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w1");
    }
}

/// Workspace numbers are list positions: the sidebar shows a workspace's
/// place in the snapshot and the number keys index that list, whatever
/// number its ID was allocated with.
#[test]
fn workspace_numbers_and_switching_follow_list_position_not_the_id() {
    let mut projected = snapshot();
    let template = projected.workspaces[0].clone();
    projected.workspaces = [("w5", "first"), ("w2", "second")]
        .into_iter()
        .map(|(id, label)| {
            let mut workspace = template.clone();
            workspace.workspace_id = test_workspace_id(id);
            workspace.label = label.into();
            workspace
        })
        .collect();
    projected.focused_workspace_id = Some(test_workspace_id("w5"));
    projected.panes[0].pane_id = crate::tests::test_pane_id("w5:p1");
    projected.focused_pane_id = Some(crate::tests::test_pane_id("w5:p1"));

    for compact in [false, true] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.chrome.set_collapsed(compact);
        state.set_snapshot(Box::new(projected.clone()));
        let frame = state.compose(100, 28).expect("test precondition");
        let rows = super::frame_rows(&frame);
        for (workspace, number) in [("w5", "1"), ("w2", "2")] {
            let rect = workspace_rect(&state, &ClientEndpointId::Local, workspace);
            let text = rows[usize::from(rect.y)]
                .chars()
                .skip(usize::from(rect.x))
                .take(usize::from(rect.width))
                .collect::<String>();
            assert!(
                text.trim_start().starts_with(number),
                "compact={compact}: {workspace} should be numbered {number}, row is {text:?}"
            );
        }

        for (index, workspace) in [(0, "w5"), (1, "w2")] {
            let mut input = ClientShellInput::default();
            state.record_binding(
                &shepr_termio::input::KeybindAction::SwitchWorkspace(index),
                &mut input,
            );
            assert!(
                matches!(
                    input.actions.as_slice(),
                    [ClientShellAction::Endpoint { request, .. }]
                        if matches!(
                            &request.command,
                            EndpointCommand::WorkspaceFocus(target)
                                if target.workspace_id == test_workspace_id(workspace)
                        )
                ),
                "compact={compact}: switching to {index} should focus {workspace}"
            );
        }
    }
}

fn local_navigation_state(compact: bool) -> ClientShellState {
    local_navigation_state_with(compact, ClientConfig::default())
}

/// A local shell on `config`, which also gets the terminal theme, with three workspaces
/// drawn once.
fn local_navigation_state_with(compact: bool, mut config: ClientConfig) -> ClientShellState {
    config.theme = terminal_theme().theme;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.chrome.set_collapsed(compact);
    state.set_snapshot(Box::new(workspaces(3)));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(100, 28).expect("test precondition");
    state
}

fn request_local_navigation(
    state: &mut ClientShellState,
    down: usize,
) -> shepr_protocol::RequestId {
    enter_navigation(state);
    for _ in 0..down {
        preview_key(state, b"\x1b[B");
    }
    let activation = state.handle_input_bytes(b"\r");
    let [
        ClientShellAction::ActivateEndpoint(Location {
            endpoint: ClientEndpointId::Local,
            target,
        }),
    ] = activation.actions.as_slice()
    else {
        panic!("expected a local activation request");
    };
    let actions = state.focus_endpoint_target(*target);
    let [ClientShellAction::Endpoint { request, .. }] = actions.as_slice() else {
        panic!("expected a local workspace focus request");
    };
    assert!(matches!(
        request.command,
        EndpointCommand::WorkspaceFocus(_)
    ));
    request.id.clone()
}

fn assert_local_highlight(state: &mut ClientShellState, selected_id: &str) {
    let frame = state.compose(100, 28).expect("test precondition");
    for workspace_id in ["w1", "w2", "w3"] {
        let rect = workspace_rect(state, &ClientEndpointId::Local, workspace_id);
        assert_eq!(
            (rect.x..rect.right())
                .any(|x| cell_bg(&frame, (x, rect.y)) == state.config.palette.active_row_bg),
            workspace_id == selected_id,
            "expected only {selected_id} highlighted, checking {workspace_id}"
        );
    }
}

/// The reply a server gives a successful workspace focus.
fn workspace_focus_reply(workspace_id: &str) -> EndpointReply {
    EndpointReply::WorkspaceInfo {
        workspace: shepr_protocol::command::WorkspaceInfo {
            workspace_id: test_workspace_id(workspace_id),
            label: workspace_id.into(),
            pane_count: 1,
            agent_status: shepr_protocol::AgentStatus::Idle,
        },
    }
}

fn set_local_focus(state: &mut ClientShellState, workspace_id: &str, revision: u64) {
    let mut snapshot = workspaces(3);
    snapshot.revision =
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(revision);
    snapshot.focused_workspace_id = Some(test_workspace_id(workspace_id));
    state.set_snapshot(Box::new(snapshot));
    let mut frame = surface();
    frame.projection_revision =
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(revision);
    state.receive_pane_surface_from(
        frame,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
}

#[test]
fn accepted_local_navigation_keeps_highlight_until_authoritative_focus() {
    for compact in [false, true] {
        for response_first in [false, true] {
            let mut state = local_navigation_state(compact);
            let request_id = request_local_navigation(&mut state, 2);
            assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
            assert!(state.mode.preview().is_none());
            assert_eq!(
                state
                    .endpoints
                    .active
                    .shared_snapshot()
                    .expect("test precondition")
                    .focused_workspace_id
                    .as_ref(),
                Some(&crate::tests::test_workspace_id("w1"))
            );
            assert_eq!(
                state.focused_pane_id().as_ref(),
                Some(&crate::tests::test_pane_id("w1:p1"))
            );
            assert_local_highlight(&mut state, "w3");
            state.invalidate_pane_surface();
            assert_local_highlight(&mut state, "w3");
            state.receive_pane_surface_from(
                surface(),
                state
                    .endpoints
                    .active
                    .generation()
                    .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
            );
            if response_first {
                state.handle_endpoint_result(
                    &crate::tests::test_boot_id("boot-1"),
                    &request_id,
                    Ok(workspace_focus_reply("w3")),
                );
                assert_local_highlight(&mut state, "w3");
            }
            set_local_focus(&mut state, "w1", 2);
            assert_local_highlight(&mut state, "w3");
            set_local_focus(&mut state, "w3", 3);
            assert_local_highlight(&mut state, "w3");
            if !response_first {
                state.handle_endpoint_result(
                    &crate::tests::test_boot_id("boot-1"),
                    &request_id,
                    Ok(workspace_focus_reply("w3")),
                );
            }
            set_local_focus(&mut state, "w2", 4);
            assert_local_highlight(&mut state, "w2");
        }
    }
}

#[test]
fn failed_local_navigation_releases_only_its_own_highlight() {
    for failure in ["rejected", "timeout", "cancelled"] {
        let mut state = local_navigation_state(false);
        let request_id = request_local_navigation(&mut state, 2);
        assert_local_highlight(&mut state, "w3");
        if failure == "cancelled" {
            assert_eq!(
                state.drop_request(&request_id, DropReason::Interrupted),
                crate::shell::state::Repaint::Needed
            );
        } else {
            state.handle_endpoint_result(
                &crate::tests::test_boot_id("boot-1"),
                &request_id,
                Err(if failure == "timeout" {
                    ClientShellEndpointError::Timeout
                } else {
                    ClientShellEndpointError::Server(
                        shepr_protocol::command::EndpointError::WorkspaceGone(
                            "w3".parse().expect("workspace id"),
                        ),
                    )
                }),
            );
        }
        assert_local_highlight(&mut state, "w1");
        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(workspace_focus_reply("w3")),
        );
        assert_local_highlight(&mut state, "w1");
    }
    for old_down in [1, 2] {
        let mut state = local_navigation_state(false);
        let old_request = request_local_navigation(&mut state, old_down);
        let latest_request = request_local_navigation(&mut state, 2);
        state.drop_request(&old_request, DropReason::Interrupted);
        assert_local_highlight(&mut state, "w3");
        state.drop_request(&latest_request, DropReason::Interrupted);
        assert_local_highlight(&mut state, "w1");
    }
}

#[test]
fn pending_navigation_highlight_does_not_survive_identity_changes() {
    for change in ["disconnect", "boot", "generation", "deleted", "endpoint"] {
        let mut state = local_navigation_state(false);
        let request_id = request_local_navigation(&mut state, 2);
        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(workspace_focus_reply("w3")),
        );
        assert_local_highlight(&mut state, "w3");
        let mut snapshot = workspaces(3);
        match change {
            "disconnect" => {
                state.mark_endpoint_disconnected(&ClientEndpointId::Local);
                state.connect_endpoint_with_snapshot(
                    &ClientEndpointId::Local,
                    1,
                    Box::new(snapshot),
                );
            }
            "boot" => {
                snapshot.boot_id = crate::tests::test_boot_id("replacement-boot");
                state.set_snapshot(Box::new(snapshot));
            }
            "generation" => state.set_endpoint_snapshot_for_generation(
                &ClientEndpointId::Local,
                crate::tests::test_generation(2),
                Box::new(snapshot),
            ),
            "deleted" => {
                snapshot.workspaces.pop();
                state.set_snapshot(Box::new(snapshot));
                state.set_snapshot(Box::new(workspaces(3)));
            }
            "endpoint" => {
                let machine = remote_machine();
                let remote = ClientEndpointId::Ssh(machine.label.clone());
                state.set_machines(&[machine]);
                state.connect_endpoint_with_snapshot(&remote, 1, Box::new(snapshot));
                assert!(state.activate_endpoint_projection(&remote));
                assert!(state.pending_workspace_highlight.is_none());
                assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
            }
            _ => unreachable!(),
        }
        assert!(state.pending_workspace_highlight.is_none(), "{change}");
        let mut frame = surface();
        frame.boot_id = state
            .endpoints
            .active
            .shared_snapshot()
            .expect("test precondition")
            .boot_id
            .clone();
        state.receive_pane_surface_from(
            frame,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        assert_local_highlight(&mut state, "w1");
    }
}

#[test]
fn navigation_highlight_yields_to_new_intent() {
    let mut state = local_navigation_state(false);
    request_local_navigation(&mut state, 2);
    enter_navigation(&mut state);
    preview_key(&mut state, b"\x1b");
    assert_local_highlight(&mut state, "w1");

    request_local_navigation(&mut state, 2);
    let mut unrelated = ClientShellInput::default();
    state.push_endpoint_command(
        EndpointCommand::PaneClear(shepr_protocol::command::PaneTarget {
            pane_id: shepr_test_fixtures::id("w1:p1"),
        }),
        &mut unrelated,
    );
    let [ClientShellAction::Endpoint { request, .. }] = unrelated.actions.as_slice() else {
        panic!("expected unrelated request");
    };
    state.drop_request(&request.id, DropReason::Interrupted);
    assert_local_highlight(&mut state, "w3");
    let mut focus = ClientShellInput::default();
    state.focus_or_activate(
        crate::shell::navigation::location::Location::workspace(
            ClientEndpointId::Local,
            shepr_test_fixtures::id("w2"),
        ),
        &mut focus,
    );
    assert!(state.pending_workspace_highlight.is_none());
}

#[test]
fn directional_pane_focus_releases_an_accepted_workspace_highlight() {
    use shepr_protocol::command::PaneDirection;

    for (key, direction) in [
        (b'h', PaneDirection::Left),
        (b'j', PaneDirection::Down),
        (b'k', PaneDirection::Up),
        (b'l', PaneDirection::Right),
    ] {
        for rejected in [false, true] {
            let mut state = local_navigation_state(false);
            let pending_request = request_local_navigation(&mut state, 2);
            assert_local_highlight(&mut state, "w3");
            preview_key(&mut state, &[0x02]);
            let outcome = state.handle_input_bytes(&[key]);
            let [ClientShellAction::Endpoint { request, .. }] = outcome.actions.as_slice() else {
                panic!("expected a directional pane focus request");
            };
            let EndpointCommand::PaneFocusDirection(params) = &request.command else {
                panic!("expected PaneFocusDirection");
            };
            assert_eq!(params.direction, direction);
            assert_eq!(params.pane_id.to_string(), "w1:p1");
            assert!(state.pending_workspace_highlight.is_none());
            assert_local_highlight(&mut state, "w1");
            let result = if rejected {
                Err(ClientShellEndpointError::Server(
                    shepr_protocol::command::EndpointError::WorkspaceGone(
                        "w3".parse().expect("workspace id"),
                    ),
                ))
            } else {
                Ok(EndpointReply::Done)
            };
            state.handle_endpoint_result(
                &crate::tests::test_boot_id("boot-1"),
                &pending_request,
                result,
            );
            assert_local_highlight(&mut state, "w1");
        }
    }
}

#[test]
fn direct_agent_focus_repaints_when_releasing_a_workspace_highlight() {
    let mut config = ClientConfig::default();
    config.keys.focus_agent = shepr_config::BindingConfig::one("ctrl+alt+1");
    let mut projected = workspaces(3);
    projected.agents.push(agent(AgentStatus::Idle, 1));

    for pending in [false, true] {
        let mut state = local_navigation_state_with(false, config.clone());
        state.set_snapshot(Box::new(projected.clone()));
        state.compose(100, 28).expect("test precondition");
        if pending {
            request_local_navigation(&mut state, 2);
            assert_local_highlight(&mut state, "w3");
        }

        // Direct bindings do not inherit the repaint from leaving prefix mode.
        let outcome =
            state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
                KeyCode::Char('1'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ))]);
        assert!(
            matches!(outcome.actions.as_slice(), [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.command, EndpointCommand::PaneFocus(params)
                if params.pane_id == crate::tests::test_pane_id("w1:p1")))
        );
        assert!(state.pending_workspace_highlight.is_none());
        assert_eq!(outcome.repaint, pending);
        assert_local_highlight(&mut state, "w1");
    }
}

#[test]
fn cancelled_close_does_not_restore_an_older_navigation_highlight() {
    let mut state = local_navigation_state(false);
    request_local_navigation(&mut state, 2);
    enter_navigation(&mut state);
    state.open_confirm_close_overlay(shepr_test_fixtures::id("w1"));
    preview_key(&mut state, b"\x1b");
    assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
    preview_key(&mut state, b"\x1b");
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    assert_local_highlight(&mut state, "w1");
}

#[test]
fn cancelled_close_returns_to_the_mode_it_was_opened_from() {
    let mut state = local_navigation_state(false);
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    state.open_confirm_close_overlay(shepr_test_fixtures::id("w1"));
    preview_key(&mut state, b"\x1b");
    assert!(state.overlay.is_none());
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    assert!(state.mode.preview().is_none());
}

#[test]
fn coalesced_navigation_focus_does_not_leave_a_permanent_highlight() {
    let mut state = local_navigation_state(false);
    let before_request = std::time::Instant::now();
    let request_id = request_local_navigation(&mut state, 2);
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &request_id,
        Ok(workspace_focus_reply("w3")),
    );
    // Another client can focus the original workspace before the server projects
    // either change, so a successful request need not produce a new snapshot.
    assert!(!state.tick_workspace_highlight(before_request));
    let highlight_deadline = state
        .workspace_highlight_deadline()
        .expect("pending highlight has an expiry");
    assert_eq!(state.next_timer_deadline(), Some(highlight_deadline));
    assert_local_highlight(&mut state, "w3");
    let now = std::time::Instant::now();
    assert!(state.tick_workspace_highlight(now + std::time::Duration::from_secs(2)));
    assert_local_highlight(&mut state, "w1");
    assert!(!state.tick_workspace_highlight(now + std::time::Duration::from_secs(3)));
}

#[test]
fn navigation_highlight_ends_for_noop_focus_and_creation() {
    let mut state = local_navigation_state(false);
    request_local_navigation(&mut state, 0);
    assert!(state.pending_workspace_highlight.is_none());
    set_local_focus(&mut state, "w2", 2);
    assert_local_highlight(&mut state, "w2");

    for command in [
        EndpointCommand::WorkspaceCreate(shepr_protocol::command::WorkspaceCreateParams {
            source: shepr_protocol::command::WorkspaceCreateSource::Default,
            label: None,
        }),
        EndpointCommand::PaneSplit(shepr_protocol::command::PaneSplitParams {
            pane_id: shepr_test_fixtures::id("w1:p1"),
            direction: shepr_protocol::command::SplitDirection::Right,
        }),
    ] {
        let mut state = local_navigation_state(false);
        request_local_navigation(&mut state, 2);
        let mut outcome = ClientShellInput::default();
        state.push_endpoint_command(command, &mut outcome);
        assert!(state.pending_workspace_highlight.is_none());
        assert_local_highlight(&mut state, "w1");
    }
}

/// `projected` with an agent in pane 1 of each of `workspaces`, in that order, each
/// pane listed.
fn with_agents(mut projected: ClientShellSnapshot, workspaces: &[&str]) -> ClientShellSnapshot {
    for workspace in workspaces {
        let pane_id = crate::tests::test_pane_id(&format!("{workspace}:p1"));
        if !projected.panes.iter().any(|pane| pane.pane_id == pane_id) {
            projected.panes.push(ClientShellPane {
                pane_id,
                ..projected.panes[0].clone()
            });
        }
        projected.agents.push(ClientShellAgent {
            pane_id,
            ..agent(AgentStatus::Idle, 1)
        });
    }
    projected
}

/// A local shell showing `projected`, its sidebar expanded or `compact`, drawn once.
fn agent_navigation_state(
    projected: ClientShellSnapshot,
    compact: bool,
    rows: u16,
) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.chrome.set_collapsed(compact);
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(100, rows).expect("test precondition");
    state
}

/// The agent panel's entries, in the order it draws them.
fn agent_targets(state: &ClientShellState) -> Vec<Location> {
    state.endpoints.agent_panel_model.targets().to_vec()
}

fn assert_agent_selected(state: &ClientShellState, agent: &Location) {
    assert_eq!(
        state.mode.preview().map(|selected| &selected.location),
        Some(agent)
    );
    assert!(state.navigation_target_valid(state.mode.preview().expect("test precondition")));
}

fn agent_rect(state: &ClientShellState, agent: &Location) -> Rect {
    state
        .drawn()
        .agents()
        .find(|hit| &hit.location == agent)
        .map(|hit| hit.rect)
        .expect("visible agent")
}

#[test]
fn navigation_continues_from_the_last_workspace_into_the_agents_and_back() {
    for compact in [false, true] {
        let mut state =
            agent_navigation_state(with_agents(workspaces(2), &["w1", "w2"]), compact, 28);
        let agents = agent_targets(&state);
        assert_eq!(agents.len(), 2);
        enter_navigation(&mut state);
        assert_selected(&state, &ClientEndpointId::Local, "w1");
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w2");
        preview_key(&mut state, b"\x1b[B");
        assert_agent_selected(&state, &agents[0]);
        // Moving only highlights: nothing was focused.
        assert_eq!(
            state.focused_pane_id(),
            Some(crate::tests::test_pane_id("w1:p1"))
        );

        // The selected agent takes the selected-workspace look, and only it.
        let frame = state.compose(100, 28).expect("test precondition");
        let palette = &state.config.palette;
        let selection = if palette.selection_bg == ratatui::style::Color::Reset {
            palette.active_row_bg
        } else {
            palette.selection_bg
        };
        let selected = agent_rect(&state, &agents[0]);
        for y in selected.y..selected.bottom() {
            for x in selected.x..selected.right() {
                assert_eq!(
                    cell_bg(&frame, (x, y)),
                    selection,
                    "compact={compact}, ({x}, {y})"
                );
            }
        }
        let other = agent_rect(&state, &agents[1]);
        assert_ne!(cell_bg(&frame, (other.x, other.y)), selection);
        for workspace in ["w1", "w2"] {
            let rect = workspace_rect(&state, &ClientEndpointId::Local, workspace);
            assert_ne!(
                cell_bg(&frame, (rect.x, rect.y)),
                selection,
                "compact={compact}, {workspace}"
            );
        }

        preview_key(&mut state, b"\x1b[B");
        assert_agent_selected(&state, &agents[1]);
        // Past the last agent the list wraps to the first workspace, and back.
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w1");
        preview_key(&mut state, b"\x1b[A");
        assert_agent_selected(&state, &agents[1]);
        preview_key(&mut state, b"\x1b[A");
        assert_agent_selected(&state, &agents[0]);
        // Up from the first agent is the last workspace.
        preview_key(&mut state, b"\x1b[A");
        assert_selected(&state, &ClientEndpointId::Local, "w2");
        assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
    }
}

#[test]
fn enter_on_an_agent_focuses_its_pane_on_its_machine_and_workspace() {
    // Both machines have an agent on w2 while w1 is focused.
    let (mut state, remote) = navigation_state(
        with_agents(workspaces(2), &["w2"]),
        &ClientConfig::default(),
    );
    state.compose(100, 28).expect("test precondition");
    let agents = agent_targets(&state);
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().any(|agent| agent.endpoint == remote));
    for (index, agent) in agents.iter().enumerate() {
        enter_navigation(&mut state);
        // Past both machines' two workspaces.
        for _ in 0..4 + index {
            preview_key(&mut state, b"\x1b[B");
        }
        assert_agent_selected(&state, agent);

        let enter = state.handle_input_bytes(b"\r");
        assert!(enter.requests.is_empty());
        assert!(
            matches!(enter.actions.as_slice(), [ClientShellAction::ActivateEndpoint(target)] if target == agent),
            "{agent:?}"
        );
        assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
        assert!(state.mode.preview().is_none());
        // The runtime focuses a presented machine's pick the way it does a click's: the
        // pane focus moves its server to the pane's workspace.
        if agent.endpoint == ClientEndpointId::Local {
            let actions = state.focus_endpoint_target(agent.target);
            assert!(
                matches!(actions.as_slice(), [ClientShellAction::Endpoint { request, .. }]
                    if matches!(&request.command, EndpointCommand::PaneFocus(target)
                        if target.pane_id == crate::tests::test_pane_id("w2:p1")))
            );
        }
    }
}

#[test]
fn agent_selection_follows_its_agent_through_list_changes() {
    let (mut state, remote) = navigation_state(
        with_agents(workspaces(2), &["w1", "w2"]),
        &ClientConfig::default(),
    );
    state.compose(100, 28).expect("test precondition");
    let local_agent =
        |pane: &str| Location::pane(ClientEndpointId::Local, crate::tests::test_pane_id(pane));
    let selected = local_agent("w2:p1");
    assert_eq!(agent_targets(&state)[1], selected);
    enter_navigation(&mut state);
    for _ in 0..4 + 1 {
        preview_key(&mut state, b"\x1b[B");
    }
    assert_agent_selected(&state, &selected);

    // An agent appearing ahead of it leaves the selection on its agent.
    state.edit_endpoint_snapshot(&ClientEndpointId::Local, |snapshot| {
        let pane_id = crate::tests::test_pane_id("w1:p2");
        snapshot.panes.push(ClientShellPane {
            pane_id,
            ..snapshot.panes[0].clone()
        });
        snapshot.agents.insert(
            0,
            ClientShellAgent {
                pane_id,
                ..agent(AgentStatus::Working, 2)
            },
        );
    });
    assert_eq!(agent_targets(&state)[2], selected);
    assert_agent_selected(&state, &selected);

    // Its agent going away hands the selection to the agent now at its place.
    state.edit_endpoint_snapshot(&ClientEndpointId::Local, |snapshot| {
        snapshot
            .agents
            .retain(|agent| agent.pane_id != crate::tests::test_pane_id("w2:p1"));
    });
    let in_its_place = agent_targets(&state)[2].clone();
    assert_eq!(
        in_its_place,
        Location::pane(remote.clone(), crate::tests::test_pane_id("w1:p1"))
    );
    assert_agent_selected(&state, &in_its_place);

    // Its machine going stale hands it to the last agent still selectable.
    state.set_endpoint_status(&remote, EndpointFailureStatus::Reconnecting);
    assert_agent_selected(&state, &local_agent("w1:p1"));

    // With no agent left, the selection is the last workspace, where moving up from the
    // first agent leads; the stale machine's workspaces are not selectable.
    state.edit_endpoint_snapshot(&ClientEndpointId::Local, |snapshot| snapshot.agents.clear());
    assert_selected(&state, &ClientEndpointId::Local, "w2");
    assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
}

#[test]
fn navigation_ends_at_the_workspaces_without_agents_to_select() {
    // No agents at all, then agents in an agent panel too short to show a row.
    let cases: [(&[&str], u16); 2] = [(&[], 28), (&["w1", "w2"], 6)];
    for (agents, rows) in cases {
        let mut state = agent_navigation_state(with_agents(workspaces(2), agents), false, rows);
        assert_eq!(state.drawn().agents().count(), 0, "{agents:?}");
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w2");
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w1");
        preview_key(&mut state, b"\x1b[A");
        assert_selected(&state, &ClientEndpointId::Local, "w2");
    }
}
