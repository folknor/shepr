use super::*;

#[test]
fn host_theme_updates_are_forwarded_to_the_server() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));

    let inferred = state.handle_raw_events(vec![RawInputEvent::HostDefaultColor {
        kind: crate::terminal_theme::DefaultColorKind::Background,
        color: crate::terminal_theme::RgbColor {
            r: 255,
            g: 255,
            b: 255,
        },
    }]);
    assert!(inferred.repaint);
    assert!(matches!(
        inferred.requests.as_slice(),
        [ClientMessage::ClientShellHostTheme {
            update: crate::protocol::ClientHostThemeUpdate::DefaultColor {
                kind: crate::protocol::ClientHostDefaultColorKind::Background,
                ..
            }
        }]
    ));
    assert_eq!(
        state.host_background,
        Some(crate::terminal_theme::RgbColor {
            r: 255,
            g: 255,
            b: 255,
        })
    );

    let explicit = state.handle_raw_events(vec![RawInputEvent::HostColorSchemeChanged(
        crate::terminal_theme::HostAppearance::Dark,
    )]);
    assert!(matches!(
        explicit.requests.as_slice(),
        [ClientMessage::ClientShellHostTheme {
            update: crate::protocol::ClientHostThemeUpdate::Appearance(
                crate::protocol::ClientHostAppearance::Dark
            )
        }]
    ));

    let repeated = state.handle_raw_events(vec![RawInputEvent::HostDefaultColor {
        kind: crate::terminal_theme::DefaultColorKind::Background,
        color: crate::terminal_theme::RgbColor {
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
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    for appearance in [
        crate::terminal_theme::HostAppearance::Dark,
        crate::terminal_theme::HostAppearance::Light,
    ] {
        let outcome =
            state.handle_raw_events(vec![RawInputEvent::HostColorSchemeChanged(appearance)]);
        assert!(outcome.query_host_theme);
    }
    let unrelated = state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    assert!(!unrelated.query_host_theme);
}

#[test]
fn focus_gained_forces_a_full_redraw_only_when_configured() {
    for redraw in [false, true] {
        let mut config = Config::default();
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

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut responses = String::new();
    for index in 0..=u8::MAX {
        let _ = write!(responses, "\x1b]4;{index};rgb:1111/2222/3333\x1b\\");
    }

    let outcome = state.handle_input_bytes(responses.as_bytes());

    let [
        ClientMessage::ClientShellHostTheme {
            update: crate::protocol::ClientHostThemeUpdate::PaletteColors(colors),
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
    let key = |code, modifiers| crate::input::TerminalKey::new(code, modifiers);
    assert!(!input::is_modal_paste_shortcut(&key(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL | KeyModifiers::ALT
    )));
    assert!(input::is_modal_paste_shortcut(&key(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL
    )));
    assert!(input::is_modal_paste_shortcut(&key(
        KeyCode::Char('V'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT
    )));
    assert!(!input::is_modal_paste_shortcut(&key(
        KeyCode::Char('v'),
        KeyModifiers::SUPER
    )));
}

#[test]
fn modal_paste_inserts_clipboard_text_through_overlay_text_path() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
        title: "rename pane",
        input: TextEditor::new("replace me", true),
        target: ClientRenameTarget::Pane {
            pane_id: "pane_1".into(),
        },
    }));
    let mut outcome = ClientShellInput::default();
    let key = crate::input::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL);

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
fn physical_release_uses_the_leased_press_code_with_current_modifiers() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let press = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
        .with_windows_record(crate::input::WindowsKeyRecord {
            key_down: true,
            repeat_count: 1,
            virtual_key_code: 0x58,
            virtual_scan_code: 0x2d,
            unicode: 0,
            control_key_state: 0,
        });
    state.handle_raw_events(vec![RawInputEvent::Key(press)]);
    let release = crate::input::TerminalKey::new(KeyCode::Char('z'), KeyModifiers::SHIFT)
        .with_kind(crossterm::event::KeyEventKind::Release)
        .with_windows_record(crate::input::WindowsKeyRecord {
            key_down: false,
            repeat_count: 1,
            virtual_key_code: 0x5a,
            virtual_scan_code: 0x2d,
            unicode: 0,
            control_key_state: 0x0010,
        });

    let outcome = state.handle_raw_events(vec![RawInputEvent::Key(release)]);

    assert!(matches!(
        &outcome.requests[..],
        [ClientMessage::ClientShellPaneInput { events, .. }]
            if matches!(
                &events[..],
                [ClientPaneInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('x'),
                    modifiers,
                    kind: crate::protocol::ClientKeyKind::Release,
                    physical_key_id: Some(0x2d),
                    ..
                }] if *modifiers == KeyModifiers::SHIFT.bits()
            )
    ));
}

#[test]
fn highlighted_search_match_copies_after_in_flight_repeat() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 20,
        viewport_rows: 2,
    });
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(
        &crate::input::KeybindMatch::Action(crate::input::KeybindAction::CopyMode),
        &mut enter,
    );
    let matches = vec![
        crate::api::schema::PaneTextRange {
            start: crate::api::schema::PaneTextPoint { row: 5, col: 2 },
            end: crate::api::schema::PaneTextPoint { row: 5, col: 7 },
        },
        crate::api::schema::PaneTextRange {
            start: crate::api::schema::PaneTextPoint { row: 15, col: 1 },
            end: crate::api::schema::PaneTextPoint { row: 15, col: 6 },
        },
    ];

    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        KeyCode::Char('/'),
        KeyModifiers::empty(),
    ))]);
    state.handle_raw_events(vec![RawInputEvent::Text(crate::input::TextCommit::new(
        "needle",
    ))]);
    let initial = state.handle_raw_events(vec![RawInputEvent::Key(
        crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
    )]);
    let [ClientShellAction::Endpoint { request, .. }] = &initial.actions[..] else {
        panic!("initial search request");
    };
    state.handle_endpoint_result(
        "boot-1",
        &request.id,
        Ok(copy_search_result(matches.clone(), Some(0))),
    );
    let repeat = state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        KeyCode::Char('n'),
        KeyModifiers::empty(),
    ))]);
    let [ClientShellAction::Endpoint { request, .. }] = &repeat.actions[..] else {
        panic!("repeat search request");
    };
    let repeat_id = request.id.clone();

    let early_copy = state.handle_raw_events(vec![RawInputEvent::Key(
        crate::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
    )]);
    assert!(early_copy.actions.is_empty());
    assert_eq!(state.mode, ClientShellMode::Copy);

    let actions = state
        .handle_endpoint_result(
            "boot-1",
            &repeat_id,
            Ok(copy_search_result(matches, Some(1))),
        )
        .actions;
    assert_eq!(state.mode, ClientShellMode::Terminal);
    let selection_request_id = actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(
                    request.method,
                    crate::api::schema::Method::PaneSelectionRead(_)
                ) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        })
        .expect("deferred selection read");
    let clipboard = state
        .handle_endpoint_result(
            "boot-1",
            &selection_request_id,
            Ok(crate::api::schema::ResponseResult::PaneSelection {
                pane_id: "pane_1".into(),
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
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();
    let geometry =
        crate::input::mouse::HostGeometry::new(106, 20, 1060, 400).expect("host geometry");
    let x = u32::from(pane.inner_rect.x) * 10 + 21;
    let y = u32::from(pane.inner_rect.y) * 20 + 21;

    let down = state.handle_pixel_mouse(format!("\x1b[<0;{x};{y}M").as_bytes(), geometry);
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
    let release = state.handle_pixel_mouse(b"\x1b[<0;1;1m", geometry);
    assert!(matches!(
        &release.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "pane_1"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        kind: crate::protocol::ClientMouseKind::Up(
                            crate::protocol::ClientMouseButton::Left
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
    let config = ClientShellConfig::from_config(&Config::default());
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));

    let text = state.handle_input_bytes(b"hello");
    assert_eq!(text.requests.len(), 1);
    let ClientMessage::ClientShellPaneInput { pane_id, events } = &text.requests[0] else {
        panic!("expected targeted pane input");
    };
    assert_eq!(pane_id, "pane_1");
    assert_eq!(events.len(), 5);
    assert!(matches!(
        &events[0],
        ClientPaneInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char('h'),
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
            code: crate::protocol::ClientKeyCode::Char('c'),
            modifiers,
            kind: crate::protocol::ClientKeyKind::Press,
            ..
        }] if *modifiers == KeyModifiers::CONTROL.bits()
    ));

    let alt = state.handle_input_bytes(b"\x1b[120;3u");
    let ClientMessage::ClientShellPaneInput { events, .. } = &alt.requests[0] else {
        panic!("expected semantic alt key");
    };
    assert!(matches!(
        &events[..],
        [ClientPaneInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char('x'),
            modifiers,
            ..
        }] if *modifiers == KeyModifiers::ALT.bits()
    ));
    assert!(!state.handle_input_bytes(&[0x02]).detach);
    let detach = state.handle_input_bytes(b"q");
    assert!(detach.detach);
    assert!(detach.requests.is_empty());
}

#[test]
fn pane_key_release_keeps_the_press_target() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
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
            kind: crate::protocol::ClientKeyKind::Release,
            ..
        }]
    ));
}

#[test]
fn help_overlay_uses_live_keymap_and_owns_filter_state() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut open = ClientShellInput::default();
    state.record_binding(
        &crate::input::KeybindMatch::Action(crate::input::KeybindAction::Help),
        &mut open,
    );
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
fn rename_pane_empty_value_is_preserved_as_a_clear_request() {
    let mut snapshot = snapshot();
    snapshot.panes[0].label = Some("build".into());
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    let mut open = ClientShellInput::default();
    state.record_binding(
        &crate::input::KeybindMatch::Action(crate::input::KeybindAction::RenamePane),
        &mut open,
    );
    assert!(state.handle_input_bytes(&[0x15]).actions.is_empty());
    let save = state.handle_input_bytes(b"\r");
    let [ClientShellAction::Endpoint { request, .. }] = &save.actions[..] else {
        panic!("pane rename should use endpoint API");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneRename(params)
            if params.pane_id == "pane_1" && params.label.as_deref() == Some("")
    ));
}

#[test]
fn styled_client_composition_preserves_pane_hyperlinks() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    let linked = Buffer::with_lines(["LIVE", "PANE"]);
    pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &linked,
        None,
        &[((0, 0), "L".into(), "https://example.test".into())],
    );
    state.set_pane_surface(pane_surface);
    let mut selection =
        crate::selection::Selection::absolute_range("pane_1".to_owned(), (0, 0), (0, 1));
    assert!(selection.finish());
    state.selection = Some(selection);
    let frame = state.compose(106, 20).expect("composed frame");
    let hit = &state.hits.panes[0];
    let index =
        usize::from(hit.inner_rect.y) * usize::from(frame.width) + usize::from(hit.inner_rect.x);
    let link = frame.cells[index].hyperlink.expect("linked cell") as usize;
    assert_eq!(frame.hyperlinks[link], "https://example.test");
}
