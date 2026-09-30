use super::*;
use shepr_termio::input::{KeybindAction, KeybindMatch, TerminalKey};

fn shell(field: usize) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut frame = surface();
    frame.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.set_pane_surface(frame);
    state.compose(106, 30).expect("initial shell");
    match field {
        0 => state.open_new_workspace_overlay(&mut ClientShellInput::default()),
        1 => state.open_rename_workspace_overlay(),
        2 => state.open_rename_pane_overlay(),
        3 => {
            state.open_navigator_overlay();
            state.handle_input_bytes(b"/");
        }
        4 => {
            state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
                query: TextEditor::default(),
                search_focused: true,
                scroll: 0,
            }));
        }
        5 => {
            state.record_binding(
                &KeybindMatch::Action(KeybindAction::CopyMode),
                &mut ClientShellInput::default(),
            );
            state.handle_input_bytes(b"/");
        }
        _ => unreachable!(),
    }
    state
}

fn editor(state: &mut ClientShellState) -> &mut TextEditor {
    match state.overlay.as_mut() {
        Some(ClientShellOverlay::Rename(v)) => &mut v.input,
        Some(ClientShellOverlay::Navigator(v)) => &mut v.query,
        Some(ClientShellOverlay::Help(v)) => &mut v.query,
        _ => {
            &mut state
                .copy_mode
                .as_mut()
                .expect("copy mode")
                .search_prompt
                .as_mut()
                .expect("prompt")
                .query
        }
    }
}

fn press(state: &mut ClientShellState, code: KeyCode, modifiers: KeyModifiers) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(TerminalKey::new(code, modifiers))])
}

#[test]
fn all_six_fields_route_shared_text_editing() {
    for field in 0..6 {
        let mut state = shell(field);
        *editor(&mut state) = TextEditor::from("ab");
        press(&mut state, KeyCode::Left, KeyModifiers::NONE);
        let result = press(&mut state, KeyCode::Char('X'), KeyModifiers::NONE);
        assert!(result.repaint, "field {field}");
        assert!(result.requests.is_empty() && result.actions.is_empty());
        assert_eq!(editor(&mut state).as_str(), "aXb");
    }
}

/// Opens the new-workspace prompt and returns the checkout-root request it
/// sent, with the request id and its lookup id.
fn open_new_workspace(state: &mut ClientShellState) -> (String, u64, String) {
    let mut outcome = ClientShellInput::default();
    state.open_new_workspace_overlay(&mut outcome);
    let [ClientShellAction::Endpoint { request, .. }] = outcome.actions.as_slice() else {
        panic!("expected one endpoint request, got {:?}", outcome.actions);
    };
    let EndpointCommand::WorkspaceCheckoutRoot(params) = &request.command else {
        panic!(
            "expected a checkout root request, got {:?}",
            request.command
        );
    };
    let Some(ClientShellOverlay::Rename(ClientRenameOverlay {
        target:
            ClientRenameTarget::NewWorkspace {
                label_lookup_id: Some(lookup_id),
                ..
            },
        ..
    })) = state.overlay.as_ref()
    else {
        panic!("the overlay awaits a label lookup");
    };
    (request.id.clone(), *lookup_id, params.cwd.clone())
}

fn checkout_root_answer(root: Option<&str>) -> EndpointReply {
    EndpointReply::WorkspaceCheckoutRoot {
        root: root.map(str::to_owned),
        home: None,
    }
}

#[test]
fn new_workspace_label_comes_from_the_endpoint_and_stale_answers_are_ignored() {
    let mut state = shell(0);
    let boot_id = state.snapshot.as_deref().expect("snapshot").boot_id.clone();
    let (stale_request, stale_id, cwd) = open_new_workspace(&mut state);
    assert_eq!(cwd, "/repo");

    let (current_request, current_id, _) = open_new_workspace(&mut state);
    assert_ne!(stale_id, current_id);
    state.handle_endpoint_result_at(
        &boot_id,
        &stale_request,
        Ok(checkout_root_answer(Some("/elsewhere/stale-label"))),
        state.now,
    );
    assert_eq!(editor(&mut state).as_str(), "repo");

    let outcome = state.handle_endpoint_result_at(
        &boot_id,
        &current_request,
        Ok(checkout_root_answer(Some("/srv/checkout-label"))),
        state.now,
    );
    assert!(outcome.repaint);
    assert_eq!(editor(&mut state).as_str(), "checkout-label");
}

#[test]
fn new_workspace_label_answer_keeps_a_user_edit_and_a_failure_keeps_the_suggestion() {
    let mut state = shell(0);
    let boot_id = state.snapshot.as_deref().expect("snapshot").boot_id.clone();
    let (request, _, _) = open_new_workspace(&mut state);
    *editor(&mut state) = TextEditor::from("mine");
    state.handle_endpoint_result_at(
        &boot_id,
        &request,
        Ok(checkout_root_answer(Some("/srv/checkout-label"))),
        state.now,
    );
    assert_eq!(editor(&mut state).as_str(), "mine");

    let (request, _, _) = open_new_workspace(&mut state);
    state.handle_endpoint_result_at(
        &boot_id,
        &request,
        Err(ClientShellEndpointError {
            code: "internal_error".into(),
            message: "git failed".into(),
        }),
        state.now,
    );
    assert_eq!(editor(&mut state).as_str(), "repo");
}

#[test]
fn text_delivery_paths_insert_at_the_cursor() {
    for delivery in 0..3 {
        let mut state = shell(0);
        *editor(&mut state) = TextEditor::from("ab");
        press(&mut state, KeyCode::Left, KeyModifiers::NONE);
        let result = match delivery {
            0 => state.handle_raw_events(vec![RawInputEvent::Key(
                TerminalKey::new(KeyCode::Char('x'), KeyModifiers::NONE)
                    .with_generated_text(Some("X".into())),
            )]),
            1 => state.handle_raw_events(vec![RawInputEvent::Paste("X".into())]),
            _ => {
                let mut result = ClientShellInput::default();
                assert!(state.handle_modal_paste_shortcut_with(
                    &TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                    &mut result,
                    || Some("X".into())
                ));
                result
            }
        };
        assert!(result.repaint, "delivery {delivery}");
        assert!(result.requests.is_empty() && result.actions.is_empty());
        assert_eq!(editor(&mut state).as_str(), "aXb");
    }
}

#[test]
fn rename_clear_exceptions_remain_local() {
    for field in 0..3 {
        for (code, modifiers) in [
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (KeyCode::Backspace, KeyModifiers::SUPER),
        ] {
            let mut state = shell(field);
            *editor(&mut state) = TextEditor::from("name");
            press(&mut state, code, modifiers);
            assert!(state.overlay.is_some());
            assert!(editor(&mut state).is_empty());
        }
    }
}

#[test]
fn cursor_movement_preserves_filter_selection_and_scroll() {
    for field in [3, 4] {
        let mut state = shell(field);
        *editor(&mut state) = TextEditor::from("ab");
        match state.overlay.as_mut().expect("overlay") {
            ClientShellOverlay::Navigator(v) => {
                v.scroll = 3;
                v.selected = Some(ClientNavigatorTarget::Pane {
                    endpoint_id: ClientEndpointId::Local,
                    pane_id: test_pane_id("w1:p1"),
                });
            }
            ClientShellOverlay::Help(v) => v.scroll = 3,
            _ => unreachable!(),
        }
        press(&mut state, KeyCode::Home, KeyModifiers::NONE);
        press(&mut state, KeyCode::Char('u'), KeyModifiers::CONTROL); // Empty kill must not refresh results.
        match state.overlay.as_ref().expect("overlay") {
            ClientShellOverlay::Navigator(v) => {
                assert_eq!(v.scroll, 3);
                assert!(v.selected.is_some());
            }
            ClientShellOverlay::Help(v) => assert_eq!(v.scroll, 3),
            _ => unreachable!(),
        }
        press(&mut state, KeyCode::Char('k'), KeyModifiers::CONTROL);
        match state.overlay.as_ref().expect("overlay") {
            ClientShellOverlay::Navigator(v) => assert!(v.selected.is_none()),
            ClientShellOverlay::Help(v) => assert_eq!(v.scroll, 0),
            _ => unreachable!(),
        }
    }
}

#[test]
fn escape_preserves_help_overlay_with_generated_text() {
    let mut state = shell(4);
    *editor(&mut state) = TextEditor::from("feature");
    let result = state.handle_raw_events(vec![RawInputEvent::Key(
        TerminalKey::new(KeyCode::Esc, KeyModifiers::NONE)
            .with_generated_text(Some("printable".into())),
    )]);
    assert!(result.repaint);
    assert!(result.requests.is_empty());
    assert!(result.actions.is_empty());
    let Some(ClientShellOverlay::Help(help)) = &state.overlay else {
        panic!("Escape should leave help open");
    };
    assert!(!help.search_focused);
    assert!(help.query.is_empty());
    assert_eq!(help.scroll, 0);
}

#[test]
fn focused_filters_keep_ctrl_n_p_navigation_and_literal_commands() {
    for field in [3, 4] {
        let mut state = shell(field);
        if let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() {
            navigator.selected = None;
        }
        state.compose(106, 30).expect("filter frame");
        for ch in ['j', 'k', '?'] {
            press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(editor(&mut state).as_str(), "jk?");
    }
}

#[test]
fn all_naming_targets_preserve_submission_and_empty_semantics() {
    for field in 0..3 {
        for empty in [false, true] {
            let mut state = shell(field);
            *editor(&mut state) = TextEditor::from(if empty { "  " } else { "  ab " });
            if !empty {
                press(&mut state, KeyCode::Home, KeyModifiers::NONE);
                press(&mut state, KeyCode::Char('X'), KeyModifiers::NONE);
            }
            let result = press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
            assert!(state.overlay.is_none());
            if empty && field == 1 {
                assert!(result.actions.is_empty());
                continue;
            }
            let [ClientShellAction::Endpoint { request, .. }] = &result.actions[..] else {
                panic!("naming target {field}");
            };
            let expected = if empty { "" } else { "X  ab" };
            match &request.command {
                EndpointCommand::WorkspaceCreate(v) => {
                    assert_eq!(v.label.as_deref(), (!empty).then_some(expected));
                }
                EndpointCommand::WorkspaceRename(v) => assert_eq!(v.label, expected),
                EndpointCommand::PaneRename(v) => assert_eq!(v.label.as_deref(), Some(expected)),
                _ => panic!("wrong command"),
            }
        }
    }
}

#[test]
fn copy_search_owns_prefix_but_parked_prompt_does_not_steal_input() {
    let mut state = shell(5);
    *editor(&mut state) = TextEditor::from("ab");
    press(&mut state, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert_eq!(state.mode, ClientShellMode::Copy);
    state.handle_raw_events(vec![RawInputEvent::Paste("X".into())]);
    assert_eq!(editor(&mut state).as_str(), "aXb");
    state.open_rename_pane_overlay();
    assert!(state.modal_paste_target_active());
    state.handle_raw_events(vec![RawInputEvent::Paste("name".into())]);
    assert_eq!(editor(&mut state).as_str(), "name");
    state.overlay = None;
    assert_eq!(editor(&mut state).as_str(), "aXb");
    state.mode = ClientShellMode::Terminal;
    assert!(!state.modal_paste_target_active());
    let input = state.handle_raw_events(vec![RawInputEvent::Paste("terminal".into())]);
    assert!(
        matches!(&input.requests[..], [ClientMessage::ClientShellPaneInput { events, .. }] if matches!(&events[..], [ClientPaneInputEvent::Paste(text)] if text == "terminal"))
    );
    assert_eq!(editor(&mut state).as_str(), "aXb");
    state.mode = ClientShellMode::Copy;
    press(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    press(&mut state, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert_eq!(state.mode, ClientShellMode::Prefix);
}

#[test]
fn every_field_renders_long_unicode_across_resize_without_mutation() {
    for field in 0..6 {
        let mut state = shell(field);
        *editor(&mut state) =
            TextEditor::new(&"e\u{301}中\u{1F469}\u{200D}\u{1F4BB}".repeat(40), false);
        for position in [KeyCode::Home, KeyCode::End, KeyCode::Left] {
            press(&mut state, position, KeyModifiers::NONE);
            for (width, height) in [(120, 40), (60, 20), (12, 6), (1, 1), (120, 40)] {
                let before = editor(&mut state).clone();
                if let Some(frame) = state.compose(width, height)
                    && let Some(cursor) = frame.cursor.as_ref().filter(|cursor| cursor.visible)
                {
                    assert!(cursor.x < width && cursor.y < height, "field {field}");
                }
                assert_eq!(editor(&mut state), &before);
            }
        }
    }
}
