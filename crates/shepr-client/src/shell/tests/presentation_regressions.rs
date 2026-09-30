use super::*;

#[test]
fn client_presentation_regression_server_notice_titles_follow_the_notice_kind() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let notices = [
        (
            shepr_protocol::NoticeKind::PaneInputDropped {
                pane_id: test_pane_id("w1:p1"),
                events: 2,
            },
            "Pane input dropped",
        ),
        (
            shepr_protocol::NoticeKind::PasteRejected { size: 20, max: 10 },
            "Paste rejected",
        ),
        (
            shepr_protocol::NoticeKind::OversizedSurface {
                claimed: 20,
                max: 10,
            },
            "Screen too large",
        ),
    ];

    for (kind, expected_title) in notices {
        assert!(state.receive_server_notice(&kind));
        let notice = state
            .visible_endpoint_notice
            .as_ref()
            .expect("notice shown");
        assert_eq!(notice.title, expected_title);
        assert_eq!(notice.body, kind.to_string());
    }
}

#[test]
fn unavailable_view_respects_a_collapsed_single_endpoint_sidebar() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.config.sidebar_collapsed_mode = SidebarCollapsedModeConfig::Compact;
    state.sidebar_collapsed = true;
    state.set_snapshot(Box::new(snapshot()));

    let frame = state.compose(100, 28).expect("unavailable view");
    let rows = frame_rows(&frame);
    let text = rows.join("\n");

    assert!(text.contains("Local: online."));
    assert!(!text.contains("Select a connected machine."));
    assert!(
        state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.endpoint_id.is_local() && hit.workspace_id == "w1")
    );
    assert!(state.hits.machines.is_empty());
}

#[test]
fn client_presentation_regression_removed_navigator_target_accepts_visible_fallback() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.open_navigator_overlay();
    let removed_target = ClientNavigatorTarget::Pane {
        endpoint_id: state.active_endpoint_id.clone(),
        pane_id: test_pane_id("w1:p1"),
    };
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("expected navigator");
    };
    navigator.selected = Some(removed_target);

    let mut changed = snapshot();
    changed.panes[0].pane_id = test_pane_id("w1:p2");
    changed.focused_pane_id = Some(test_pane_id("w1:p2"));
    state.set_snapshot(Box::new(changed));

    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    let rows = super::super::render::client_navigator_rows(
        &state.endpoints,
        &state.active_endpoint_id,
        navigator,
    );
    assert_eq!(
        super::super::aggregate_navigation::navigator_selected_index(&rows, navigator),
        Some(0)
    );
    let expected = rows[0].target.clone();

    let mut outcome = ClientShellInput::default();
    state.accept_navigator_selection(&mut outcome);

    assert!(state.overlay.is_none());
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local,
            target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)),
        }] if workspace_id == "w1"
    ));
    assert_eq!(
        expected,
        ClientNavigatorTarget::Workspace {
            endpoint_id: state.active_endpoint_id.clone(),
            workspace_id: test_workspace_id("w1"),
        }
    );
}

#[test]
fn client_presentation_regression_help_scrolls_to_its_last_entry_in_a_narrow_terminal() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let groups = shepr_termio::input::keybind_help_groups(
        &state.config.keybinds.keybinds,
        state.config.keybinds.prefix,
    );
    let (_, last_entries) = groups.last().expect("help groups");
    let (_, last_label) = last_entries.last().expect("help entries");
    let last_word = last_label
        .split_whitespace()
        .last()
        .expect("label text")
        .to_owned();
    state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
        query: TextEditor::default(),
        search_focused: false,
        scroll: usize::MAX,
    }));

    // Narrow enough that word wrapping produces more rows than character division predicts;
    // composition clamps the scroll to the computed maximum on the first frame.
    state.compose(26, 24).expect("help frame");
    let frame = state.compose(26, 24).expect("help frame");
    let rows = frame_rows(&frame);
    let popup = state.hits.help_popup;
    let text_row = |y: u16| {
        rows[usize::from(y)]
            .chars()
            .skip(usize::from(popup.x + 1))
            .take(usize::from(popup.width.saturating_sub(3)))
            .collect::<String>()
    };
    // The help text ends with its last entry and one blank line. At the maximum scroll those
    // are the body's last two rows exactly when the range counts the rows the wrapper draws:
    // an undercount cuts the end off, an overcount leaves blank rows below it.
    let body_bottom = popup.bottom() - 4;

    assert!(
        text_row(body_bottom).trim().is_empty(),
        "the help text must scroll to its end: {rows:#?}"
    );
    assert!(
        text_row(body_bottom - 1).contains(&last_word),
        "the last help entry must sit just above the trailing blank line: \
         {last_word:?} {rows:#?}"
    );
}

#[test]
fn client_presentation_regression_notice_card_keeps_diagnostic_lines_visible() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    assert!(state.push_endpoint_notice(
        ClientEndpointNoticeKind::Unavailable,
        "machine-diagnostic:buildbox",
        "Buildbox: restart shepr to authenticate",
        "first diagnostic line\nsecond diagnostic line\nthird diagnostic line",
    ));

    let frame = state.compose(80, 12).expect("notice frame");
    let rows = frame_rows(&frame);
    assert!(
        rows.iter()
            .any(|row| row.contains("second diagnostic line"))
    );
    assert!(rows.iter().any(|row| row.contains("third diagnostic line")));
}
