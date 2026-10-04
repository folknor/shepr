use crate::endpoint::ClientEndpointId;
use crate::shell::ledger::Ticket;
use crate::shell::overlays::Overlay;
use crate::shell::overlays::rename::RenameTarget;
use crate::shell::state::{ClientShellAction, ClientShellEndpointError};
use shepr_protocol::command::EndpointCommand;
use shepr_term::key::TerminalKey;
use shepr_termio::input::raw_input::RawInputEvent;

use crate::shell::state::{ClientShellInput, ClientShellState};

use crossterm::event::{KeyCode, KeyModifiers};
use shepr_protocol::command::EndpointReply;

use crate::shell::tests::{
    fill_prompt, help_overlay, press, prompt_shell as shell, prompt_text as editor, rename_target,
};
use crate::tests::test_pane_id;

#[test]
fn all_six_fields_route_shared_text_editing() {
    for field in 0..6 {
        let mut state = shell(field);
        fill_prompt(&mut state, "ab");
        press(&mut state, KeyCode::Left, KeyModifiers::NONE);
        let result = press(&mut state, KeyCode::Char('X'), KeyModifiers::NONE);
        assert!(result.repaint, "field {field}");
        assert!(result.requests.is_empty() && result.actions.is_empty());
        assert_eq!(editor(&state).as_str(), "aXb");
    }
}

/// Opens the new-workspace prompt and returns the checkout-root request it
/// sent, with its request id and the overlay's matching id.
fn open_new_workspace(state: &mut ClientShellState) -> (shepr_protocol::RequestId, Ticket, String) {
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
    let Some(RenameTarget::NewWorkspace {
        label_lookup: Some(lookup),
        ..
    }) = rename_target(state)
    else {
        panic!("the overlay awaits a label lookup");
    };
    (
        request.id.clone(),
        *lookup,
        params.cwd.display_text().into_owned(),
    )
}

fn checkout_root_answer(root: Option<&str>) -> EndpointReply {
    EndpointReply::WorkspaceCheckoutRoot {
        root: root.map(Into::into),
        home: None,
    }
}

#[test]
fn new_workspace_label_comes_from_the_endpoint_and_stale_answers_are_ignored() {
    let mut state = shell(0);
    let boot_id = state
        .endpoints
        .active
        .snapshot()
        .expect("snapshot")
        .boot_id
        .clone();
    let (stale_request, stale_id, cwd) = open_new_workspace(&mut state);
    assert_eq!(cwd, "/repo");

    let (current_request, current_id, _) = open_new_workspace(&mut state);
    assert_ne!(stale_id, current_id);
    state.answer_request(
        &boot_id,
        &stale_request,
        Ok(checkout_root_answer(Some("/elsewhere/stale-label"))),
        state.now,
    );
    assert_eq!(editor(&state).as_str(), "repo");

    let outcome = state.answer_request(
        &boot_id,
        &current_request,
        Ok(checkout_root_answer(Some("/srv/checkout-label"))),
        state.now,
    );
    assert!(outcome.repaint);
    assert_eq!(editor(&state).as_str(), "checkout-label");
}

#[test]
fn new_workspace_label_answer_keeps_a_user_edit_and_a_failure_keeps_the_suggestion() {
    let mut state = shell(0);
    let boot_id = state
        .endpoints
        .active
        .snapshot()
        .expect("snapshot")
        .boot_id
        .clone();
    let (request, _, _) = open_new_workspace(&mut state);
    fill_prompt(&mut state, "mine");
    state.answer_request(
        &boot_id,
        &request,
        Ok(checkout_root_answer(Some("/srv/checkout-label"))),
        state.now,
    );
    assert_eq!(editor(&state).as_str(), "mine");

    let (request, _, _) = open_new_workspace(&mut state);
    state.answer_request(
        &boot_id,
        &request,
        Err(ClientShellEndpointError::Server(
            shepr_protocol::command::EndpointError::ResourceFailure("git failed".into()),
        )),
        state.now,
    );
    assert_eq!(editor(&state).as_str(), "repo");
}

#[test]
fn rename_clear_exceptions_remain_local() {
    for field in 0..3 {
        for (code, modifiers) in [
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (KeyCode::Backspace, KeyModifiers::SUPER),
        ] {
            let mut state = shell(field);
            fill_prompt(&mut state, "name");
            press(&mut state, code, modifiers);
            assert!(state.overlay.is_some());
            assert!(editor(&state).is_empty());
        }
    }
}

#[test]
fn cursor_movement_preserves_filter_selection_and_scroll() {
    for field in [3, 4] {
        let mut state = shell(field);
        fill_prompt(&mut state, "ab");
        let help = match state.overlay.as_mut().expect("overlay") {
            Overlay::Navigator(v) => {
                // This one-pane shell lists two rows at most, which no key scrolls three
                // rows down; the scroll and a selection are set directly to show that
                // moving the cursor leaves them alone.
                v.scroll = 3;
                v.selected = Some(crate::shell::navigation::location::Location::pane(
                    ClientEndpointId::Local,
                    test_pane_id("w1:p1"),
                ));
                false
            }
            Overlay::Help(_) => true,
            _ => unreachable!(),
        };
        if help {
            // No Help is drawn yet, so nothing clamps the scroll the keys ask for.
            for _ in 0..3 {
                press(&mut state, KeyCode::Down, KeyModifiers::NONE);
            }
        }
        press(&mut state, KeyCode::Home, KeyModifiers::NONE);
        press(&mut state, KeyCode::Char('u'), KeyModifiers::CONTROL); // Empty kill must not refresh results.
        match state.overlay.as_ref().expect("overlay") {
            Overlay::Navigator(v) => {
                assert_eq!(v.scroll, 3);
                assert!(v.selected.is_some());
            }
            Overlay::Help(v) => assert_eq!(v.scroll(), 3),
            _ => unreachable!(),
        }
        press(&mut state, KeyCode::Char('k'), KeyModifiers::CONTROL);
        match state.overlay.as_ref().expect("overlay") {
            Overlay::Navigator(v) => assert!(v.selected.is_none()),
            Overlay::Help(v) => assert_eq!(v.scroll(), 0),
            _ => unreachable!(),
        }
    }
}

#[test]
fn escape_preserves_help_overlay_with_generated_text() {
    let mut state = shell(4);
    fill_prompt(&mut state, "feature");
    let result = state.handle_raw_events(vec![RawInputEvent::Key(
        TerminalKey::new(KeyCode::Esc, KeyModifiers::NONE)
            .with_generated_text(Some("printable".into())),
    )]);
    assert!(result.repaint);
    assert!(result.requests.is_empty());
    assert!(result.actions.is_empty());
    let Some(help) = help_overlay(&state) else {
        panic!("Escape should leave help open");
    };
    assert!(!help.search_focused());
    assert!(help.query().is_empty());
    assert_eq!(help.scroll(), 0);
}

#[test]
fn focused_filters_keep_ctrl_n_p_navigation_and_literal_commands() {
    for field in [3, 4] {
        let mut state = shell(field);
        if field == 3 {
            // Editing the search drops the navigator's selection.
            press(&mut state, KeyCode::Char('x'), KeyModifiers::NONE);
            press(&mut state, KeyCode::Char('u'), KeyModifiers::CONTROL);
            let Some(Overlay::Navigator(navigator)) = state.overlay.as_ref() else {
                panic!("expected navigator");
            };
            assert!(navigator.selected.is_none());
        }
        state.compose(106, 30).expect("filter frame");
        for ch in ['j', 'k', '?'] {
            press(&mut state, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(editor(&state).as_str(), "jk?");
    }
}

#[test]
fn all_naming_targets_preserve_submission_and_empty_semantics() {
    for field in 0..3 {
        for empty in [false, true] {
            let mut state = shell(field);
            fill_prompt(&mut state, if empty { "  " } else { "  ab " });
            if !empty {
                press(&mut state, KeyCode::Home, KeyModifiers::NONE);
                press(&mut state, KeyCode::Char('X'), KeyModifiers::NONE);
            }
            let result = press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
            assert!(state.overlay.is_none());
            let [ClientShellAction::Endpoint { request, .. }] = &result.actions[..] else {
                panic!("naming target {field}");
            };
            // An empty name is sent as no label, which clears a rename.
            let expected = (!empty).then_some("X  ab");
            match &request.command {
                EndpointCommand::WorkspaceCreate(v) => assert_eq!(v.label.as_deref(), expected),
                EndpointCommand::WorkspaceRename(v) => assert_eq!(v.label.as_deref(), expected),
                EndpointCommand::PaneRename(v) => assert_eq!(v.label.as_deref(), expected),
                _ => panic!("wrong command"),
            }
        }
    }
}

#[test]
fn every_field_renders_long_unicode_across_resize_without_mutation() {
    for field in 0..6 {
        let mut state = shell(field);
        fill_prompt(
            &mut state,
            &"e\u{301}中\u{1F469}\u{200D}\u{1F4BB}".repeat(40),
        );
        for position in [KeyCode::Home, KeyCode::End, KeyCode::Left] {
            press(&mut state, position, KeyModifiers::NONE);
            for (width, height) in [(120, 40), (60, 20), (12, 6), (1, 1), (120, 40)] {
                let before = editor(&state).clone();
                if let Some(frame) = state.compose(width, height)
                    && let Some(cursor) = frame.cursor().filter(|cursor| cursor.visible)
                {
                    assert!(cursor.x < width && cursor.y < height, "field {field}");
                }
                assert_eq!(editor(&state), &before);
            }
        }
    }
}
