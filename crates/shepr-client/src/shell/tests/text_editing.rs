use crate::shell::config::ClientShellConfig;
use crate::shell::overlays::Overlay;
use crate::shell::overlays::rename::RenameTarget;
use crate::shell::state::ClientShellAction;
use shepr_protocol::command::EndpointCommand;
use shepr_term::key::TerminalKey;
use shepr_termio::input::raw_input::RawInputEvent;

use crate::shell::state::ClientShellState;

use crossterm::event::{KeyCode, KeyModifiers};
use shepr_config::ClientConfig;
use shepr_protocol::ClientShellPane;

use crate::shell::tests::{
    fill_prompt, help_overlay, press, prompt_shell as shell, prompt_text as editor, rename_target,
    snapshot, surface,
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

#[test]
fn the_new_workspace_prompt_suggests_the_directory_and_sends_a_blank_name_as_none() {
    let mut state = shell(0);
    state.open_new_workspace_overlay();
    assert_eq!(editor(&state).as_str(), "repo");
    // The prompt names the presented machine, here the local server by its label.
    assert!(matches!(
        rename_target(&state),
        Some(RenameTarget::NewWorkspace { cwd: Some(_), machine })
            if machine == shepr_test_fixtures::FIXTURE_LOCAL_LABEL
    ));

    press(&mut state, KeyCode::Char('c'), KeyModifiers::CONTROL);
    let outcome = state.handle_input_bytes(b"\r");
    let [ClientShellAction::Endpoint { request, .. }] = outcome.actions.as_slice() else {
        panic!("expected one endpoint request, got {:?}", outcome.actions);
    };
    let EndpointCommand::WorkspaceCreate(params) = &request.command else {
        panic!("expected a workspace create, got {:?}", request.command);
    };
    assert_eq!(params.label, None);
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

/// Panes in the one workspace of `long_navigator_shell`, more than its navigator shows.
const LONG_NAVIGATOR_PANES: usize = 40;

/// A presented shell drawn once, then the navigator opened with its search focused, the
/// way `prompt_shell(3)` opens it. Its one workspace, "lab", holds `LONG_NAVIGATOR_PANES`
/// panes, so the query "ab" matches every row through the workspace label and the list
/// is longer than the navigator body.
fn long_navigator_shell() -> ClientShellState {
    let mut projection = snapshot();
    projection.workspaces[0].label = "lab".into();
    let first = projection.panes[0].clone();
    projection
        .panes
        .extend((2..=LONG_NAVIGATOR_PANES).map(|n| ClientShellPane {
            pane_id: test_pane_id(&format!("w1:p{n}")),
            ..first.clone()
        }));
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projection));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 30).expect("initial shell");
    state.open_navigator_overlay();
    state.handle_input_bytes(b"/");
    state
}

fn navigator_scroll(state: &ClientShellState) -> usize {
    match state.overlay.as_ref() {
        Some(Overlay::Navigator(v)) => v.scroll,
        _ => panic!("expected navigator"),
    }
}

#[test]
fn cursor_movement_preserves_filter_selection_and_scroll() {
    for field in [3, 4] {
        let mut state = if field == 3 {
            long_navigator_shell()
        } else {
            shell(field)
        };
        fill_prompt(&mut state, "ab");
        if matches!(state.overlay, Some(Overlay::Help(_))) {
            // No Help is drawn yet, so nothing clamps the scroll the keys ask for.
            for _ in 0..3 {
                press(&mut state, KeyCode::Down, KeyModifiers::NONE);
            }
        } else {
            // The navigator scrolls when a frame draws its selection below the body, one
            // row per Down once the selection reaches the bottom; stop three rows down.
            for _ in 0..LONG_NAVIGATOR_PANES {
                if navigator_scroll(&state) == 3 {
                    break;
                }
                press(&mut state, KeyCode::Down, KeyModifiers::NONE);
                state.compose(106, 30).expect("navigator frame");
            }
            assert_eq!(navigator_scroll(&state), 3, "the list scrolled");
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
