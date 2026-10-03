use crate::shell::overlays::text_editor::TextEditor;
use crate::shell::state::{
    ClientRenameTarget, ClientShellAction, ClientShellConfig, ClientShellInput, ClientShellMode,
    ClientShellOverlay,
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use shepr_config::{ClientConfig, SidebarCollapsedModeConfig};
use shepr_protocol::command::{EndpointCommand, EndpointReply};
use shepr_protocol::{ClientMessage, ClientMousePosition, ClientPaneInputEvent};
use shepr_termio::input::raw_input::RawInputEvent;

use crate::shell::state::{ClientRenameOverlay, ClientShellState};
use shepr_protocol::{ClientShellWorkspace, FrameData};

use crossterm::event::MouseEvent;

use crate::shell::tests::copy_search_result;
use crate::shell::tests::{frame_cell, frame_rows, snapshot, surface};

use crate::tests::{test_pane_id, test_workspace_id};

#[test]
fn navigate_arrow_aliases_use_the_configured_alias_matcher() {
    let left = shepr_termio::input::TerminalKey::new(KeyCode::Left, KeyModifiers::empty());
    let right = shepr_termio::input::TerminalKey::new(KeyCode::Right, KeyModifiers::empty());
    let modified_left = shepr_termio::input::TerminalKey::new(KeyCode::Left, KeyModifiers::SHIFT);
    let left_alias =
        shepr_config::navigate_alias!(Left).expect("the navigate table defines its left alias");
    let right_alias =
        shepr_config::navigate_alias!(Right).expect("the navigate table defines its right alias");

    assert!(crate::shell::input::navigate_alias_matches(
        left_alias, &left
    ));
    assert!(crate::shell::input::navigate_alias_matches(
        right_alias,
        &right
    ));
    assert!(!crate::shell::input::navigate_alias_matches(
        left_alias,
        &modified_left
    ));
}

#[test]
fn cycle_pane_uses_snapshot_order_in_prefix_and_navigate_modes() {
    for mode in [ClientShellMode::Prefix, ClientShellMode::Navigate] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        let mut projection = snapshot();
        let mut second = projection.panes[0].clone();
        second.pane_id = test_pane_id("w1:p2");
        second.focused = false;
        let mut third = second.clone();
        third.pane_id = test_pane_id("w1:p3");
        projection.panes.extend([second, third]);
        state.set_snapshot(Box::new(projection));

        // A zoomed pane view can omit panes from the workspace snapshot.
        let mut pane_surface = surface();
        let mut third_surface_pane = pane_surface.panes[0].clone();
        third_surface_pane.pane_id = test_pane_id("w1:p3");
        pane_surface.panes.push(third_surface_pane);
        state.receive_pane_surface(pane_surface);
        state.mode = mode;

        let outcome = state.handle_input_bytes(b"\t");
        let [ClientShellAction::Endpoint { request, .. }] = outcome.actions.as_slice() else {
            panic!("pane cycling should issue one focus request");
        };
        assert!(matches!(
            &request.command,
            shepr_protocol::command::EndpointCommand::PaneFocus(target)
                if target.pane_id == test_pane_id("w1:p2")
        ));
    }
}

#[test]
fn host_theme_updates_are_forwarded_to_the_server() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));

    let inferred = state.handle_raw_events(vec![RawInputEvent::HostDefaultColor {
        kind: shepr_termio::host_term::theme::DefaultColorKind::Background,
        color: shepr_termio::host_term::theme::RgbColor {
            r: 255,
            g: 255,
            b: 255,
        },
    }]);
    assert!(inferred.repaint);
    assert!(matches!(
        inferred.requests.as_slice(),
        [ClientMessage::ClientShellHostTheme {
            update: shepr_protocol::ClientHostThemeUpdate::DefaultColor {
                kind: shepr_protocol::ClientHostDefaultColorKind::Background,
                ..
            }
        }]
    ));
    assert_eq!(
        state.host_background,
        Some(shepr_termio::host_term::theme::RgbColor {
            r: 255,
            g: 255,
            b: 255,
        })
    );

    let explicit = state.handle_raw_events(vec![RawInputEvent::HostColorSchemeChanged(
        shepr_termio::host_term::theme::HostAppearance::Dark,
    )]);
    assert!(matches!(
        explicit.requests.as_slice(),
        [ClientMessage::ClientShellHostTheme {
            update: shepr_protocol::ClientHostThemeUpdate::Appearance(
                shepr_protocol::ClientHostAppearance::Dark
            )
        }]
    ));

    let repeated = state.handle_raw_events(vec![RawInputEvent::HostDefaultColor {
        kind: shepr_termio::host_term::theme::DefaultColorKind::Background,
        color: shepr_termio::host_term::theme::RgbColor {
            r: 255,
            g: 255,
            b: 255,
        },
    }]);
    assert!(!repeated.repaint);
    assert_eq!(repeated.requests.len(), 1);
}

#[test]
fn host_appearance_switch_requeries_the_host_theme() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    for appearance in [
        shepr_termio::host_term::theme::HostAppearance::Dark,
        shepr_termio::host_term::theme::HostAppearance::Light,
    ] {
        let outcome =
            state.handle_raw_events(vec![RawInputEvent::HostColorSchemeChanged(appearance)]);
        assert!(outcome.query_host_theme);
    }
    let unrelated = state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    assert!(!unrelated.query_host_theme);
}

#[test]
fn passive_host_events_do_not_dismiss_endpoint_errors() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let now = std::time::Instant::now();
    state.set_endpoint_error("action failed", now);
    let deadline = state.endpoint_error.deadline();

    for event in [
        RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        }),
        RawInputEvent::OuterFocusGained,
        RawInputEvent::HostDefaultColor {
            kind: shepr_termio::host_term::theme::DefaultColorKind::Background,
            color: shepr_termio::host_term::theme::RgbColor {
                r: 12,
                g: 34,
                b: 56,
            },
        },
    ] {
        state.handle_raw_events(vec![event]);
        assert_eq!(state.endpoint_error.message(), Some("action failed"));
        assert_eq!(state.endpoint_error.deadline(), deadline);
    }

    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_kind(crossterm::event::KeyEventKind::Release),
    )]);
    assert_eq!(state.endpoint_error.message(), Some("action failed"));

    let key = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
    )]);
    assert!(state.endpoint_error.message().is_none());
    assert!(key.repaint);
}

#[test]
fn focus_gained_forces_a_full_redraw_only_when_configured() {
    for redraw in [false, true] {
        let mut config = ClientConfig::default();
        config.ui.redraw_on_focus_gained = redraw;
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        let outcome = state.handle_raw_events(vec![RawInputEvent::OuterFocusGained]);
        assert_eq!(outcome.full_redraw, redraw);
        if redraw {
            assert!(outcome.repaint);
        }
    }
}

#[test]
fn full_host_palette_response_is_sent_as_one_theme_update() {
    use std::fmt::Write as _;

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let mut responses = String::new();
    for index in 0..=u8::MAX {
        write!(responses, "\x1b]4;{index};rgb:1111/2222/3333\x1b\\")
            .expect("test precondition: writing to a String cannot fail");
    }

    let outcome = state.handle_input_bytes(responses.as_bytes());

    let [
        ClientMessage::ClientShellHostTheme {
            update: shepr_protocol::ClientHostThemeUpdate::PaletteColors(colors),
        },
    ] = outcome.requests.as_slice()
    else {
        panic!(
            "expected one batched palette update, got {} requests",
            outcome.requests.len()
        );
    };
    assert_eq!(colors.len(), 256);
    assert_eq!(
        colors.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        (0..=u8::MAX).collect::<Vec<_>>()
    );
}

#[test]
fn modal_paste_shortcut_is_ctrl_v() {
    let key = |code, modifiers| shepr_termio::input::TerminalKey::new(code, modifiers);
    assert!(!crate::shell::input::is_modal_paste_shortcut(&key(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL | KeyModifiers::ALT
    )));
    assert!(crate::shell::input::is_modal_paste_shortcut(&key(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL
    )));
    assert!(crate::shell::input::is_modal_paste_shortcut(&key(
        KeyCode::Char('V'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    )));
    assert!(!crate::shell::input::is_modal_paste_shortcut(&key(
        KeyCode::Char('v'),
        KeyModifiers::SUPER
    )));
}

#[test]
fn modal_paste_inserts_clipboard_text_through_overlay_text_path() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
        title: "rename pane",
        input: TextEditor::new("replace me", true),
        target: ClientRenameTarget::Pane {
            pane_id: test_pane_id("w1:p1"),
        },
    }));
    let mut outcome = ClientShellInput::default();
    let key = shepr_termio::input::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL);

    assert!(
        state.handle_modal_paste_shortcut_with(&key, &mut outcome, || {
            Some("feature/pasted".into())
        })
    );
    assert!(outcome.repaint);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay { ref input, .. }))
            if input.as_str() == "feature/pasted"
    ));
}

#[test]
fn highlighted_search_match_copies_after_in_flight_repeat() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 20,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let matches = vec![
        shepr_protocol::command::PaneTextRange {
            start: shepr_protocol::command::PaneTextPoint {
                row: shepr_vt::AbsRow(5),
                col: 2,
            },
            end: shepr_protocol::command::PaneTextPoint {
                row: shepr_vt::AbsRow(5),
                col: 7,
            },
        },
        shepr_protocol::command::PaneTextRange {
            start: shepr_protocol::command::PaneTextPoint {
                row: shepr_vt::AbsRow(15),
                col: 1,
            },
            end: shepr_protocol::command::PaneTextPoint {
                row: shepr_vt::AbsRow(15),
                col: 6,
            },
        },
    ];

    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('/'), KeyModifiers::empty()),
    )]);
    state.handle_raw_events(vec![RawInputEvent::Paste("needle".into())]);
    let initial = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
    )]);
    let [ClientShellAction::Endpoint { request, .. }] = &initial.actions[..] else {
        panic!("initial search request");
    };
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &request.id,
        Ok(copy_search_result(matches.clone(), Some(0))),
    );
    let repeat = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('n'), KeyModifiers::empty()),
    )]);
    let [ClientShellAction::Endpoint { request, .. }] = &repeat.actions[..] else {
        panic!("repeat search request");
    };
    let repeat_id = request.id.clone();

    let early_copy = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
    )]);
    assert!(early_copy.actions.is_empty());
    assert_eq!(state.mode, ClientShellMode::Copy);

    let actions = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &repeat_id,
            Ok(copy_search_result(matches, Some(1))),
        )
        .actions;
    assert_eq!(state.mode, ClientShellMode::Terminal);
    let selection_request_id = actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.command, EndpointCommand::PaneSelectionRead(_)) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        })
        .expect("deferred selection read");
    let clipboard = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &selection_request_id,
            Ok(EndpointReply::PaneSelection {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                text: "needle".into(),
            }),
        )
        .actions;
    assert!(matches!(
        &clipboard[..],
        [ClientShellAction::ClipboardWrite(bytes)] if bytes == b"needle"
    ));
}

#[test]
fn pixel_host_reports_use_cells_without_target_pixel_mode_and_release_outside() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();
    let geometry = shepr_termio::input::mouse::HostPixelExtent::new(106, 20, 1060, 400)
        .expect("host geometry");
    let x = u32::from(pane.inner_rect.x) * 10 + 21;
    let y = u32::from(pane.inner_rect.y) * 20 + 21;

    let down = state.handle_pixel_mouse_bytes(format!("\x1b[<0;{x};{y}M").as_bytes(), geometry);
    assert!(matches!(
        &down.requests[..],
        [ClientMessage::ClientShellPaneInput { events, .. }]
            if matches!(
                &events[..],
                [ClientPaneInputEvent::Mouse {
                    position: ClientMousePosition::Cell { column: 2, row: 1 },
                    ..
                }]
            )
    ));

    state.hits.panes.clear();
    let release = state.handle_pixel_mouse_bytes(b"\x1b[<0;1;1m", geometry);
    assert!(matches!(
        &release.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "w1:p1"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        kind: shepr_protocol::ClientMouseKind::Up(
                            shepr_protocol::ClientMouseButton::Left
                        ),
                        position: ClientMousePosition::Cell { .. },
                        ..
                    }]
                )
    ));
    assert!(state.pane_mouse_gesture.is_none());
}

#[test]
fn shell_targets_unconsumed_input_and_keeps_prefix_local() {
    let config = ClientShellConfig::from_config(&ClientConfig::default());
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));

    let text = state.handle_input_bytes(b"hello");
    assert_eq!(text.requests.len(), 1);
    let ClientMessage::ClientShellPaneInput { pane_id, events } = &text.requests[0] else {
        panic!("expected targeted pane input");
    };
    assert_eq!(pane_id, "w1:p1");
    assert_eq!(events.len(), 5);
    assert!(matches!(
        &events[0],
        ClientPaneInputEvent::Key {
            code: shepr_protocol::ClientKeyCode::Char('h'),
            generated_text: Some(text),
            ..
        } if text == "h"
    ));

    let interrupt = state.handle_input_bytes(b"\x1b[99;5u");
    assert_eq!(interrupt.requests.len(), 1);
    let ClientMessage::ClientShellPaneInput { events, .. } = &interrupt.requests[0] else {
        panic!("expected semantic interrupt");
    };
    assert!(matches!(
        &events[..],
        [ClientPaneInputEvent::Key {
            code: shepr_protocol::ClientKeyCode::Char('c'),
            modifiers,
            kind: shepr_protocol::ClientKeyKind::Press,
            ..
        }] if *modifiers == shepr_protocol::WireModifiers::CONTROL
    ));

    let alt = state.handle_input_bytes(b"\x1b[120;3u");
    let ClientMessage::ClientShellPaneInput { events, .. } = &alt.requests[0] else {
        panic!("expected semantic alt key");
    };
    assert!(matches!(
        &events[..],
        [ClientPaneInputEvent::Key {
            code: shepr_protocol::ClientKeyCode::Char('x'),
            modifiers,
            ..
        }] if *modifiers == shepr_protocol::WireModifiers::ALT
    ));
    assert!(!state.handle_input_bytes(&[0x02]).detach);
    let detach = state.handle_input_bytes(b"q");
    assert!(detach.detach);
    assert!(detach.requests.is_empty());
}

#[test]
fn pane_key_release_keeps_the_press_target() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));

    let press = state.handle_input_bytes(b"\x1b[99;5u");
    let release = state.handle_input_bytes(b"\x1b[99;5:3u");
    let ClientMessage::ClientShellPaneInput {
        pane_id: press_target,
        ..
    } = &press.requests[0]
    else {
        panic!("expected targeted press");
    };
    let ClientMessage::ClientShellPaneInput {
        pane_id: release_target,
        events,
    } = &release.requests[0]
    else {
        panic!("expected targeted release");
    };
    assert_eq!(release_target, press_target);
    assert!(matches!(
        &events[..],
        [ClientPaneInputEvent::Key {
            kind: shepr_protocol::ClientKeyKind::Release,
            ..
        }]
    ));
}

#[test]
fn text_key_release_follows_its_press_only_while_the_host_reports_all_keys() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    // `h` with its associated text, then its release, as kitty reports them.
    let press_bytes = b"\x1b[104;1;104u";
    let release_bytes = b"\x1b[104;1:3u";

    state.host_reports_all_keys = true;
    let press = state.handle_input_bytes(press_bytes);
    assert!(matches!(
        &press.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "w1:p1"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Key { generated_text: Some(text), .. }] if text == "h"
                )
    ));
    let release = state.handle_input_bytes(release_bytes);
    assert!(
        matches!(
            &release.requests[..],
            [ClientMessage::ClientShellPaneInput { pane_id, events }]
                if pane_id == "w1:p1"
                    && matches!(
                        &events[..],
                        [ClientPaneInputEvent::Key {
                            kind: shepr_protocol::ClientKeyKind::Release,
                            ..
                        }]
                    )
        ),
        "the leased release goes to the pane that got the press"
    );

    state.host_reports_all_keys = false;
    let _ = state.handle_input_bytes(press_bytes);
    let release = state.handle_input_bytes(release_bytes);
    assert!(
        release.requests.is_empty(),
        "without report-all a text press holds no lease to release"
    );
}

#[test]
fn help_overlay_uses_live_keymap_and_owns_filter_state() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let mut open = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::Help, &mut open);
    let initial = state.compose(106, 30).expect("help overlay");
    let text = initial
        .cells
        .chunks(initial.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("keybinds"));
    assert!(text.contains("prefix mode"));

    assert!(state.handle_input_bytes(b"/").actions.is_empty());
    assert!(state.handle_input_bytes(b"workspace").actions.is_empty());
    let filtered = state.compose(106, 30).expect("filtered help");
    let text = filtered
        .cells
        .chunks(filtered.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("workspace navigation"));
    assert!(!text.contains("prefix mode"));
    assert!(
        filtered
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.visible)
    );

    assert!(state.handle_input_bytes(b"\x1b").repaint);
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));
    assert!(state.handle_input_bytes(b"\x1b").repaint);
    assert!(state.overlay.is_none());
}

#[test]
fn overlay_that_does_not_fit_still_presents_the_frame() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let mut open = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::Help, &mut open);
    // Help needs at least 10 rows; this terminal has 8.
    let frame = state
        .compose(106, 8)
        .expect("a frame is presented even though help does not fit");
    let text = frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("window too small"));
    assert!(state.hits.help_popup.is_empty());
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));

    // Once the terminal is large enough the overlay is drawn again.
    let frame = state.compose(106, 30).expect("help overlay");
    assert!(!state.hits.help_popup.is_empty());
    assert_eq!(frame.height, 30);
}

#[test]
fn collapsed_sidebar_scrolls_to_workspaces_past_its_height() {
    let mut many = snapshot();
    let template = many.workspaces[0].clone();
    many.workspaces = (1..=30)
        .map(|number| ClientShellWorkspace {
            workspace_id: test_workspace_id(&format!("w{number}")),
            number,
            focused: number == 30,
            ..template.clone()
        })
        .collect();
    many.focused_workspace_id = Some(test_workspace_id("w30"));
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.config.sidebar_collapsed_mode = SidebarCollapsedModeConfig::Compact;
    state.chrome.set_collapsed(true);
    state.set_snapshot(Box::new(many));
    state.receive_pane_surface(surface());
    state.compose(106, 20).expect("collapsed frame");
    // The focused workspace is revealed, so it is on screen and clickable.
    assert!(
        state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.workspace_id == "w30")
    );
    assert!(
        !state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.workspace_id == "w1")
    );

    // The wheel scrolls the list back up.
    let body = state.hits.workspace_body;
    for _ in 0..30 {
        state.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: body.x,
                row: body.y,
                modifiers: KeyModifiers::NONE,
            },
            std::time::Instant::now(),
            &mut ClientShellInput::default(),
        );
    }
    state.compose(106, 20).expect("scrolled frame");
    assert!(
        state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.workspace_id == "w1")
    );
}

#[test]
fn hit_maps_stay_live_until_the_matching_surface_is_composed() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let mut open = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::Help, &mut open);
    state.compose(106, 30).expect("help overlay");
    let popup = state.hits.help_popup;
    assert!(!popup.is_empty());

    // A newer snapshot arrives; its surface has not. The old frame is still on screen.
    let mut next = snapshot();
    next.revision = next.revision.checked_next().expect("test precondition");
    state.set_snapshot(Box::new(next));
    assert!(!state.hits.panes.is_empty());
    assert_eq!(state.hits.help_popup, popup);

    // A click inside the visible popup must not close it.
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: popup.x + popup.width / 2,
            row: popup.y + popup.height / 2,
            modifiers: KeyModifiers::NONE,
        },
        std::time::Instant::now(),
        &mut outcome,
    );
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));

    // Parking the next revision's surface leaves the visible pair, and its hits, alone.
    let mut parked = surface();
    parked.projection_revision = shepr_protocol::ProjectionRevision::new(3);
    state.receive_pane_surface(parked);
    assert!(state.surfaces.waiting_baseline().is_some());
    assert!(!state.hits.panes.is_empty());
    assert_eq!(state.hits.help_popup, popup);
}

#[test]
fn rename_pane_empty_value_is_sent_as_a_clear_request() {
    let mut snapshot = snapshot();
    snapshot.panes[0].label = Some("build".into());
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot));
    let mut open = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::RenamePane, &mut open);
    assert!(state.handle_input_bytes(&[0x15]).actions.is_empty());
    let save = state.handle_input_bytes(b"\r");
    let [ClientShellAction::Endpoint { request, .. }] = &save.actions[..] else {
        panic!("pane rename should use endpoint API");
    };
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneRename(params)
            if params.pane_id == "w1:p1" && params.label.is_none()
    ));
}

#[test]
fn styled_client_composition_preserves_pane_hyperlinks() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    let linked = Buffer::with_lines(["LIVE", "PANE"]);
    pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &linked,
        None,
        &[((0, 0), "L".into(), "https://example.test".into())],
    );
    state.receive_pane_surface(pane_surface);
    let mut selection = shepr_vt::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 1),
    );
    assert!(selection.finish());
    state.mouse_selection.selection = Some(selection);
    let frame = state.compose(106, 20).expect("composed frame");
    let hit = &state.hits.panes[0];
    let index =
        usize::from(hit.inner_rect.y) * usize::from(frame.width) + usize::from(hit.inner_rect.x);
    let link = frame.cells[index].hyperlink.expect("linked cell") as usize;
    assert_eq!(frame.hyperlinks[link], "https://example.test");
}

fn open_help(state: &mut ClientShellState) {
    let mut open = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::Help, &mut open);
}

fn last_row_text(frame: &FrameData) -> String {
    frame_rows(frame).pop().expect("frame has rows")
}

#[test]
fn overlay_that_gives_up_commits_nothing_but_the_hint_row() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let before = state.compose(106, 8).expect("frame without overlay");
    open_help(&mut state);
    let after = state.compose(106, 8).expect("frame with the overlay open");
    let body = usize::from(before.width) * usize::from(before.height - 1);
    // Help needs at least 10 rows: nothing it drew may reach the frame, not even its
    // backdrop dimming; only the last row carries the hint.
    assert_eq!(after.cells[..body], before.cells[..body]);
    assert!(last_row_text(&after).contains("window too small"));
    assert!(after.cursor.is_none());
}

#[test]
fn overlay_backdrop_dims_the_frame_and_panels_are_opaque() {
    use shepr_protocol::WireStyleFlags;

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    for cell in &mut pane_surface.frame.cells {
        cell.style.flags = WireStyleFlags::BOLD;
        cell.style.underline = shepr_vt::UnderlineStyle::Curly;
    }
    state.receive_pane_surface(pane_surface);
    let plain = state.compose(106, 30).expect("frame without overlay");
    let hit = state.hits.panes[0].clone();
    let pane_origin = (hit.inner_rect.x, hit.inner_rect.y);
    assert!(
        !frame_cell(&plain, pane_origin)
            .style
            .flags
            .contains(WireStyleFlags::DIM)
    );

    open_help(&mut state);
    let frame = state.compose(106, 30).expect("help frame");
    let popup = state.hits.help_popup;
    assert!(!popup.is_empty());
    // Outside the popup the backdrop adds DIM and keeps everything else, shapes included.
    let dimmed = frame_cell(&frame, pane_origin);
    assert!(dimmed.style.flags.contains(WireStyleFlags::DIM));
    assert!(dimmed.style.flags.contains(WireStyleFlags::BOLD));
    assert_eq!(dimmed.style.underline, shepr_vt::UnderlineStyle::Curly);
    assert_eq!(dimmed.symbol, frame_cell(&plain, pane_origin).symbol);
    // Inside it every cell is the popup's own: no DIM and no pane underline.
    for y in popup.y..popup.bottom() {
        for x in popup.x..popup.right() {
            let cell = frame_cell(&frame, (x, y));
            assert!(
                !cell.style.flags.contains(WireStyleFlags::DIM),
                "({x}, {y})"
            );
            assert_eq!(cell.style.underline, shepr_vt::UnderlineStyle::None);
            assert_eq!(cell.hyperlink, None);
        }
    }
}

#[test]
fn mode_bar_is_drawn_only_while_no_overlay_is_open() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.mode = ClientShellMode::Prefix;
    let frame = state.compose(106, 30).expect("frame with the mode bar");
    assert!(frame_rows(&frame).iter().any(|row| row.contains("PREFIX")));

    // An overlay that fits replaces the bar's row content; one that does not fit still
    // counts as open, so the bar's tail must not show beside the hint either.
    open_help(&mut state);
    state.mode = ClientShellMode::Prefix;
    let frame = state.compose(106, 30).expect("help frame");
    assert!(!frame_rows(&frame).iter().any(|row| row.contains("PREFIX")));
    let frame = state
        .compose(106, 8)
        .expect("frame with a help that does not fit");
    assert!(last_row_text(&frame).contains("window too small"));
    assert!(
        !frame_rows(&frame)
            .iter()
            .any(|row| row.contains("keybinds"))
    );
    assert!(!frame_rows(&frame).iter().any(|row| row.contains("PREFIX")));
}
