use super::*;

#[test]
fn selection_repaint_cadence_keeps_one_deadline_and_flushes_when_input_stops() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let now = std::time::Instant::now();
    let ms = std::time::Duration::from_millis;
    state.last_composed_at = Some(now);
    for elapsed in [1, 4, 8, 12, 15] {
        assert!(!state.request_selection_drag_repaint(now + ms(elapsed)));
        assert_eq!(state.selection_repaint_deadline, Some(now + ms(16)));
    }
    assert_eq!(state.timer_delay(now + ms(8)), ms(8));
    assert!(!state.tick_selection_autoscroll(now + ms(15)).repaint);
    assert!(state.tick_selection_autoscroll(now + ms(16)).repaint);
    assert!(state.selection_repaint_deadline.is_none());
    assert!(!state.tick_selection_autoscroll(now + ms(17)).repaint);
}

#[test]
fn selection_repaint_cadence_allows_immediate_paint_when_due() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let now = std::time::Instant::now();
    assert!(state.request_selection_drag_repaint(now));
    state.last_composed_at = Some(now);
    assert!(state.request_selection_drag_repaint(now + std::time::Duration::from_millis(16)));
    assert!(state.selection_repaint_deadline.is_none());
}

#[test]
fn selection_repaint_cadence_does_not_leave_work_after_another_composition() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let now = std::time::Instant::now();
    state.selection_repaint_deadline = Some(now);
    state.compose(106, 20).expect("frame");
    assert!(state.selection_repaint_deadline.is_none());
    assert!(!state.tick_selection_autoscroll(now).repaint);
}

#[test]
fn a_pane_without_scroll_metrics_takes_no_selection() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = None;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("pane frame");
    let pane = state.hits.panes[0].clone();
    let mut mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y + 1,
        modifiers: KeyModifiers::empty(),
    };
    let press = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(
        state.selection.is_none(),
        "viewport rows are not absolute rows without the scroll origin"
    );
    assert!(
        press.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.command, EndpointCommand::PaneFocus(_))
        )),
        "the click still focuses the pane"
    );
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(state.selection.is_none());
}

#[test]
fn selection_release_copies_latest_position_before_deferred_paint() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("pane frame");
    let pane = state.hits.panes[0].clone();
    let mut mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    };
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    state.selection_repaint_deadline = Some(std::time::Instant::now());
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    let release = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(release.repaint);
    assert!(matches!(
        &release.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.command,
                EndpointCommand::PaneSelectionRead(params)
                    if params.cursor == shepr_protocol::command::PaneTextPoint {
                        row: shepr_vt::AbsRow(0),
                        col: 2,
                    })
    ));
    state.compose(106, 20).expect("release frame");
    assert!(state.selection_repaint_deadline.is_none());
}

#[test]
fn pane_split_drag_uses_projected_handle_and_stable_workspace_path() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.splits.push(PaneSurfaceSplit {
        direction: PaneSurfaceSplitDirection::Horizontal,
        pos: 40,
        area: SurfaceRect {
            x: 0,
            y: 0,
            width: 80,
            height: 19,
        },
        hit_rect: SurfaceRect {
            x: 40,
            y: 0,
            width: 1,
            height: 19,
        },
        path: vec![
            shepr_core::geometry::SplitBranch::First,
            shepr_core::geometry::SplitBranch::Second,
        ],
    });
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("split pane surface");
    let split = state.hits.pane_splits[0].clone();

    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: split.hit_rect.x,
        row: split.hit_rect.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.chrome_drag,
        Some(ClientChromeDrag::PaneSplit { .. })
    ));
    let mut replacement = snapshot();
    replacement.revision = shepr_protocol::ProjectionRevision::new(2);
    replacement.workspaces[0].label = "updated".into();
    let mut replacement_surface = surface();
    replacement_surface.projection_revision = shepr_protocol::ProjectionRevision::new(2);
    replacement_surface.splits.push(PaneSurfaceSplit {
        direction: PaneSurfaceSplitDirection::Horizontal,
        pos: 40,
        area: SurfaceRect {
            x: 0,
            y: 0,
            width: 80,
            height: 19,
        },
        hit_rect: SurfaceRect {
            x: 40,
            y: 0,
            width: 1,
            height: 19,
        },
        path: vec![
            shepr_core::geometry::SplitBranch::First,
            shepr_core::geometry::SplitBranch::Second,
        ],
    });
    state.set_snapshot(Box::new(replacement));
    state.set_pane_surface(replacement_surface);
    let drag = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: split.area.x + 48,
        row: split.hit_rect.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);
    let [ClientShellAction::Endpoint { request, .. }] = &drag.actions[..] else {
        panic!("pane split drag should use endpoint API");
    };
    assert!(matches!(
        &request.command,
        EndpointCommand::LayoutSetSplitRatio(params)
            if params.workspace_id == "w1"
                && params.path
                    == vec![
                        shepr_core::geometry::SplitBranch::First,
                        shepr_core::geometry::SplitBranch::Second,
                    ]
                && (params.ratio - 0.6).abs() < f32::EPSILON
    ));
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: split.area.x + 48,
            row: split.hit_rect.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(release.actions.is_empty());
    assert!(state.chrome_drag.is_none());
}

#[test]
fn disabled_mouse_chrome_removes_split_drag_hits() {
    let mut config = Config::default();
    config.ui.mouse_capture = false;
    let projected = snapshot();
    let mut pane_surface = surface();
    pane_surface.splits.push(PaneSurfaceSplit {
        direction: PaneSurfaceSplitDirection::Horizontal,
        pos: 40,
        area: SurfaceRect {
            x: 0,
            y: 0,
            width: 80,
            height: 19,
        },
        hit_rect: SurfaceRect {
            x: 40,
            y: 0,
            width: 1,
            height: 19,
        },
        path: Vec::new(),
    });
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("mouse-disabled shell");
    assert!(state.hits.pane_splits.is_empty());
}

#[test]
fn client_double_click_selects_word_and_copies_only_after_release() {
    for (copy_on_select, release_before_response) in [(false, true), (true, false)] {
        let mut state = word_drag_state(copy_on_select);
        let initial = start_word_drag(&mut state);
        let release = MouseEventKind::Up(MouseButton::Left);
        if release_before_response {
            assert!(
                word_drag_mouse(&mut state, release, 0, 8)
                    .actions
                    .is_empty()
            );
        }
        let mut actions = word_row_reply(&mut state, &initial, "alpha bravo charlie");
        if !release_before_response {
            assert!(actions.is_empty(), "holding the second press must not copy");
            assert!(
                state
                    .selection
                    .as_ref()
                    .expect("test precondition")
                    .is_in_progress()
            );
            state.tick_selection_highlight(
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            );
            assert!(
                state
                    .selection
                    .as_ref()
                    .expect("test precondition")
                    .is_visible()
            );
            actions = word_drag_mouse(&mut state, release, 0, 8).actions;
        }
        assert!(
            state
                .selection
                .as_ref()
                .expect("test precondition")
                .is_finalized()
        );
        assert_eq!(
            state
                .selection
                .as_ref()
                .expect("test precondition")
                .ordered_cells(),
            ((shepr_vt::AbsRow(0), 6), (shepr_vt::AbsRow(0), 10))
        );
        assert!(
            word_drag_mouse(&mut state, release, 0, 8)
                .actions
                .is_empty(),
            "copy only once"
        );
        if copy_on_select {
            assert!(
                matches!(&actions[..], [ClientShellAction::Endpoint { request, .. }]
                if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
                    if params.anchor.col == 6 && params.cursor.col == 10))
            );
            let copied = word_row_reply(&mut state, &word_read_id(&actions), "bravo");
            assert!(
                matches!(&copied[..], [ClientShellAction::ClipboardWrite(bytes)] if bytes == b"bravo")
            );
            assert!(
                state.tick_selection_highlight(
                    state
                        .selection_highlight_clear_deadline
                        .expect("test precondition")
                )
            );
            assert!(state.selection.is_none());
        } else {
            assert!(actions.is_empty(), "manual selection must not auto-copy");
            state.tick_selection_highlight(
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            );
            assert!(
                state.selection.is_some(),
                "manual selection must not expire"
            );
        }
    }
}

fn word_drag_state(copy_on_select: bool) -> ClientShellState {
    let mut config = Config::default();
    config.ui.copy_on_select = copy_on_select;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    let buffer = Buffer::with_lines([
        "alpha bravo charlie",
        "delta echo foxtrot ",
        "golf hotel india   ",
    ]);
    pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    pane_surface.panes[0].rect.width = 19;
    pane_surface.panes[0].rect.height = 3;
    pane_surface.panes[0].inner_rect = pane_surface.panes[0].rect;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    state
}

fn word_drag_mouse(
    state: &mut ClientShellState,
    kind: MouseEventKind,
    row: u16,
    col: u16,
) -> ClientShellInput {
    let pane = state.hits.panes[0].clone();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind,
        column: pane.inner_rect.x + col,
        row: pane.inner_rect.y + row,
        modifiers: KeyModifiers::empty(),
    })])
}

fn word_read_id(actions: &[ClientShellAction]) -> String {
    actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.command, EndpointCommand::PaneSelectionRead(_)) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        })
        .expect("selection read")
}

fn word_row_reply(state: &mut ClientShellState, id: &str, text: &str) -> Vec<ClientShellAction> {
    state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            id,
            Ok(EndpointReply::PaneSelection {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                text: text.into(),
            }),
        )
        .actions
}

fn start_word_drag(state: &mut ClientShellState) -> String {
    word_drag_mouse(state, MouseEventKind::Down(MouseButton::Left), 0, 8);
    word_drag_mouse(state, MouseEventKind::Up(MouseButton::Left), 0, 8);
    assert!(state.selection.is_none(), "plain clicks must not select");
    let second = word_drag_mouse(state, MouseEventKind::Down(MouseButton::Left), 0, 8);
    assert!(second.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
            if params.anchor.col == 0 && params.cursor.col == state.hits.panes[0].inner_rect.width - 1))));
    word_read_id(&second.actions)
}

#[test]
fn double_click_drag_selects_whole_words_in_both_directions() {
    let mut state = word_drag_state(false);
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    for (col, expected) in [
        (14, ((shepr_vt::AbsRow(0), 6), (shepr_vt::AbsRow(0), 18))),
        (2, ((shepr_vt::AbsRow(0), 0), (shepr_vt::AbsRow(0), 10))),
        (8, ((shepr_vt::AbsRow(0), 6), (shepr_vt::AbsRow(0), 10))),
        (11, ((shepr_vt::AbsRow(0), 6), (shepr_vt::AbsRow(0), 11))),
        (16, ((shepr_vt::AbsRow(0), 6), (shepr_vt::AbsRow(0), 18))),
    ] {
        let motion = word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, col);
        assert!(
            motion.actions.is_empty(),
            "reuse the row while dragging within it"
        );
        assert_eq!(
            state
                .selection
                .as_ref()
                .expect("test precondition")
                .ordered_cells(),
            expected
        );
    }
    assert!(
        word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 16)
            .actions
            .is_empty()
    );
    assert!(
        state
            .selection
            .as_ref()
            .expect("test precondition")
            .is_finalized()
    );
}

#[test]
fn double_click_drag_waits_for_latest_row_before_copying() {
    for release_before_anchor in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        if !release_before_anchor {
            assert!(word_row_reply(&mut state, &initial, "alpha bravo charlie").is_empty());
        }
        let first_motion =
            word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 1, 8);
        for col in [1, 3, 7] {
            assert!(
                word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 2, col)
                    .actions
                    .is_empty()
            );
        }
        assert!(
            word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 7)
                .actions
                .is_empty()
        );
        let final_read = if release_before_anchor {
            word_row_reply(&mut state, &initial, "alpha bravo charlie")
        } else {
            word_row_reply(
                &mut state,
                &word_read_id(&first_motion.actions),
                "delta echo foxtrot",
            )
        };
        assert!(
            matches!(&final_read[..], [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
                if params.anchor.row == shepr_vt::AbsRow(2)
                    && params.cursor.row == shepr_vt::AbsRow(2)))
        );
        let copy = word_row_reply(&mut state, &word_read_id(&final_read), "golf hotel india");
        assert!(
            matches!(&copy[..], [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
                if params.anchor == shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 6,
                }
                    && params.cursor == shepr_protocol::command::PaneTextPoint {
                        row: shepr_vt::AbsRow(2),
                        col: 9,
                    }))
        );
        let copied = word_row_reply(
            &mut state,
            &word_read_id(&copy),
            "bravo charlie\ndelta echo foxtrot\ngolf hotel",
        );
        assert!(
            matches!(&copied[..], [ClientShellAction::ClipboardWrite(bytes)]
            if bytes == b"bravo charlie\ndelta echo foxtrot\ngolf hotel")
        );
    }
}

#[test]
fn double_click_drag_ignores_row_reply_after_typing_or_new_click() {
    for typing in [false, true] {
        let mut state = word_drag_state(false);
        let initial = start_word_drag(&mut state);
        word_row_reply(&mut state, &initial, "alpha bravo charlie");
        let drag = word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 1, 8);
        let row_id = word_read_id(&drag.actions);
        if typing {
            state.handle_input_bytes(b"x");
        } else {
            word_drag_mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 0, 0);
            word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 0);
        }
        assert!(word_row_reply(&mut state, &row_id, "delta echo foxtrot").is_empty());
        assert!(state.selection.is_none());
    }
}

#[test]
fn double_click_drag_survives_focus_lag_after_anchor_reply() {
    let mut state = word_drag_state(true);
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    let mut lagging = snapshot();
    lagging.focused_pane_id = None;
    lagging.panes[0].focused = false;
    state.set_snapshot(Box::new(lagging));
    assert!(state.selection.is_some());
    word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 14);
    assert_eq!(
        state
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_cells(),
        ((shepr_vt::AbsRow(0), 6), (shepr_vt::AbsRow(0), 18))
    );
    let released = word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 14);
    assert_eq!(released.actions.len(), 1);
}

#[test]
fn drag_in_unfocused_pane_survives_snapshots_until_focus_moves_after_landing() {
    let focused_on = |pane_id: &str| {
        let mut projected = snapshot();
        let mut other = projected.panes[0].clone();
        other.pane_id = test_pane_id("w1:p2");
        projected.panes.push(other);
        for pane in &mut projected.panes {
            pane.focused = pane.pane_id == pane_id;
        }
        projected.focused_pane_id = Some(test_pane_id(pane_id));
        projected
    };
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(focused_on("w1:p2")));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("pane frame");
    let pane = state.hits.panes[0].clone();
    assert_eq!(pane.pane_id, "w1:p1");
    let mouse = |kind, column| {
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row: pane.inner_rect.y,
            modifiers: KeyModifiers::empty(),
        })
    };

    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        pane.inner_rect.x,
    )]);
    // Snapshots produced before the click's PaneFocus lands (a title spinner,
    // say) still name the old pane; they must not cancel the drag.
    state.set_snapshot(Box::new(focused_on("w1:p2")));
    assert!(state.selection.is_some(), "focus lag cancelled the drag");
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        pane.inner_rect.x + 2,
    )]);
    assert_eq!(
        state
            .selection
            .as_ref()
            .expect("drag continues")
            .ordered_cells(),
        ((shepr_vt::AbsRow(0), 0), (shepr_vt::AbsRow(0), 2))
    );

    state.set_snapshot(Box::new(focused_on("w1:p1")));
    assert!(state.selection.is_some());
    assert!(state.selection_focus_pending.is_none());
    // Once focus has landed, moving it away again ends the selection.
    state.set_snapshot(Box::new(focused_on("w1:p2")));
    assert!(state.selection.is_none());
}

#[test]
fn selection_in_focused_pane_still_ends_when_focus_moves() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("pane frame");
    let pane = state.hits.panes[0].clone();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(state.selection_focus_pending.is_none());
    let mut moved = snapshot();
    let mut other = moved.panes[0].clone();
    other.pane_id = test_pane_id("w1:p2");
    moved.panes[0].focused = false;
    moved.panes.push(other);
    moved.focused_pane_id = Some(test_pane_id("w1:p2"));
    state.set_snapshot(Box::new(moved));
    assert!(state.selection.is_none());
}

#[test]
fn double_click_drag_invalidates_cached_boundaries_outside_selected_cells() {
    for copy_on_select in [false, true] {
        let mut state = word_drag_state(copy_on_select);
        let initial = start_word_drag(&mut state);
        word_row_reply(&mut state, &initial, "alpha bravo charlie");
        let mut changed = state
            .pane_surface
            .as_ref()
            .expect("test precondition")
            .clone();
        changed.surface_revision = changed
            .surface_revision
            .checked_next()
            .expect("test precondition");
        changed.panes[0].content_revision += 2;
        changed.frame.cells[14].symbol = " ".into();
        state.set_pane_surface(changed);
        assert!(
            state.selection.is_none(),
            "unchanged selected cells do not validate cached boundaries outside the selection"
        );
        assert!(
            word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 14)
                .actions
                .is_empty()
        );
        assert!(
            word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 14)
                .actions
                .is_empty()
        );
        assert!(state.selection.is_none());
    }
}

#[test]
fn reconnect_word_selection_tracks_content_changes() {
    for content_changed in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        word_row_reply(&mut state, &initial, "alpha bravo charlie");
        let mut next_surface = state
            .pane_surface
            .as_ref()
            .expect("test precondition")
            .clone();
        if content_changed {
            next_surface.panes[0].content_revision += 2;
            next_surface.frame.cells[14].symbol = " ".into();
        }
        let endpoint_id = state.active_endpoint_id.clone();
        let snapshot = state.snapshot.as_ref().expect("test precondition").clone();
        state.mark_endpoint_disconnected(&endpoint_id);
        state.cache_endpoint_snapshot_for_generation(&endpoint_id, 1, snapshot);
        state.set_endpoint_status(&endpoint_id, crate::endpoint::ClientEndpointStatus::Online);
        assert!(state.activate_endpoint_projection(&endpoint_id));
        state.set_pane_surface(next_surface);

        assert_eq!(state.selection.is_some(), !content_changed);
        assert_eq!(state.word_selection_gesture.is_some(), !content_changed);
    }
}

#[test]
fn double_click_release_ignores_reply_after_focus_or_content_changes() {
    for focus_changed in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 8);
        if focus_changed {
            let mut lagging = snapshot();
            lagging.focused_pane_id = None;
            lagging.panes[0].focused = false;
            state.set_snapshot(Box::new(lagging));
            let mut unfocused = snapshot();
            unfocused.focused_pane_id = Some(test_pane_id("w1:p2"));
            unfocused.panes[0].focused = false;
            let mut other = unfocused.panes[0].clone();
            other.pane_id = test_pane_id("w1:p2");
            other.focused = true;
            unfocused.panes.push(other);
            state.set_snapshot(Box::new(unfocused));
        } else {
            let mut changed = state
                .pane_surface
                .as_ref()
                .expect("test precondition")
                .clone();
            changed.surface_revision = changed
                .surface_revision
                .checked_next()
                .expect("test precondition");
            changed.panes[0].content_revision += 2;
            state.set_pane_surface(changed);
        }
        assert!(
            word_row_reply(&mut state, &initial, "alpha bravo charlie").is_empty(),
            "a stale released gesture must not copy"
        );
        assert!(state.selection.is_none());
    }
}

#[test]
fn double_click_drag_resize_cancels_pending_word_lookup() {
    for anchor_ready in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        let pending = if anchor_ready {
            word_row_reply(&mut state, &initial, "alpha bravo charlie");
            let motion = word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 1, 8);
            word_read_id(&motion.actions)
        } else {
            initial
        };
        word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 8);
        let mut resized = state
            .pane_surface
            .as_ref()
            .expect("test precondition")
            .clone();
        resized.surface_revision = resized
            .surface_revision
            .checked_next()
            .expect("test precondition");
        resized.panes[0].rect.width += 5;
        resized.panes[0].inner_rect.width += 5;
        state.set_pane_surface(resized);
        assert!(word_row_reply(&mut state, &pending, "alpha bravo charlie extra").is_empty());
        assert!(
            state.selection.is_none(),
            "a late reply must not restore a resized selection"
        );
        assert!(state.selection_autoscroll.is_none());
    }
}

#[test]
fn double_click_drag_autoscroll_keeps_absolute_word_anchor() {
    let mut state = word_drag_state(false);
    state.hits.panes[0].scroll = Some(shepr_termio::ScrollMetrics {
        max_offset_from_bottom: 10,
        offset_from_bottom: 5,
        viewport_rows: 3,
        history_origin: shepr_vt::AbsRow(0),
    });
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 14);
    let tick = state.tick_selection_autoscroll(
        state
            .selection_autoscroll_deadline
            .expect("test precondition"),
    );
    word_row_reply(
        &mut state,
        &word_read_id(&tick.actions),
        "delta echo foxtrot",
    );
    assert_eq!(
        state
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_cells(),
        ((shepr_vt::AbsRow(4), 11), (shepr_vt::AbsRow(5), 10))
    );
    word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 14);
    assert!(
        state
            .selection
            .as_ref()
            .expect("test precondition")
            .is_finalized()
    );
    assert!(state.selection_autoscroll.is_none());
}

#[test]
fn pane_content_updates_preserve_live_ranges_until_geometry_or_screen_changes() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let surface_at = |surface_revision, content_revision, alternate_screen_active| {
        let mut pane_surface = surface();
        pane_surface.surface_revision = shepr_protocol::SurfaceRevision::new(surface_revision);
        pane_surface.panes[0].content_revision = content_revision;
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 11,
            viewport_rows: 2,
            history_origin: shepr_vt::AbsRow(0),
        });
        pane_surface.panes[0].alternate_screen_active = alternate_screen_active;
        pane_surface
    };
    state.set_pane_surface(surface_at(1, 0, true));
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();
    let mouse = |kind, column, row| {
        RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        })
    };

    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        pane.inner_rect.x,
        pane.inner_rect.y + 1,
    )]);
    let mut updated_surface = surface_at(2, 2, true);
    updated_surface.frame.cells[0].symbol = "W".into();
    state.set_pane_surface(updated_surface);
    state.compose(106, 20).expect("updated frame");

    let drag = state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        pane.inner_rect.x + 1,
        pane.inner_rect.y + 1,
    )]);

    assert!(drag.repaint || state.selection_repaint_deadline.is_some());
    let selection = state.selection.as_ref().expect("visible selection");
    assert!(selection.is_visible());
    assert_eq!(
        selection.ordered_cells(),
        ((shepr_vt::AbsRow(12), 0), (shepr_vt::AbsRow(12), 1))
    );

    let mut replaced_surface = surface_at(3, 4, true);
    replaced_surface.frame.cells[4].symbol = "X".into();
    state.set_pane_surface(replaced_surface);
    assert_eq!(
        state
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_cells(),
        ((shepr_vt::AbsRow(12), 0), (shepr_vt::AbsRow(12), 1))
    );

    // The selected row can leave the viewport during a drag. A later patch,
    // including an in-flight content revision, must keep that absolute range.
    let mut scrolled = surface_at(4, 5, true);
    scrolled.panes[0]
        .scroll
        .as_mut()
        .expect("test precondition")
        .offset_from_bottom = 2;
    assert!(matches!(
        state.apply_pane_surface_patch(&shepr_protocol::PaneSurfacePatch {
            boot_id: scrolled.boot_id,
            projection_revision: scrolled.projection_revision,
            base_surface_revision: shepr_protocol::SurfaceRevision::new(3),
            surface_revision: shepr_protocol::SurfaceRevision::new(4),
            panes: scrolled.panes,
            rows: vec![],
            cursor: scrolled.frame.cursor,
        }),
        super::super::surface_patch::ClientPaneSurfacePatchOutcome::Applied(_)
    ));
    assert!(
        state
            .selection
            .as_ref()
            .expect("test precondition")
            .is_in_progress()
    );
    assert_eq!(
        state
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_cells(),
        ((shepr_vt::AbsRow(12), 0), (shepr_vt::AbsRow(12), 1))
    );

    for (surface_revision, content_revision, width, alternate_screen_active) in
        [(5, 6, 4, false), (6, 8, 3, false)]
    {
        state.selection = Some(shepr_vt::selection::Selection::anchor(
            test_pane_id("w1:p1"),
            shepr_vt::Point::new(shepr_vt::AbsRow(12), 0),
        ));
        let mut changed_surface =
            surface_at(surface_revision, content_revision, alternate_screen_active);
        changed_surface.panes[0].inner_rect.width = width;
        changed_surface.panes[0].alternate_screen_active = alternate_screen_active;
        state.set_pane_surface(changed_surface);
        assert!(state.selection.is_none());
    }
}

#[test]
fn pane_mouse_input_keeps_stable_target_and_endpoint_encoding() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();

    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x + 2,
        row: pane.inner_rect.y + 1,
        modifiers: KeyModifiers::ALT,
    })]);
    let [ClientMessage::ClientShellPaneInput { pane_id, events }] = &click.requests[..] else {
        panic!("pane application click should use targeted canonical input");
    };
    assert_eq!(pane_id, "w1:p1");
    assert!(matches!(
        &events[..],
        [ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Down(
                shepr_protocol::ClientMouseButton::Left
            ),
            position: ClientMousePosition::Cell { column: 2, row: 1 },
            modifiers,
            ..
        }] if *modifiers == shepr_protocol::WireModifiers::ALT
    ));
    let moved = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::ALT,
    })]);
    assert!(moved.requests.is_empty());
    assert!(state.pane_mouse_gesture.is_some());
    state.hits.panes.clear();
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::ALT,
        })]);
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
                        ..
                    }]
                )
    ));
    assert!(state.pane_mouse_gesture.is_none());
}

#[test]
fn pane_pixel_mouse_preserves_pane_relative_pixel_coordinates() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    pane_surface.panes[0].sgr_pixel_mouse = true;
    pane_surface.panes[0].pixel_width = 39;
    pane_surface.panes[0].pixel_height = 38;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();
    let geometry = shepr_termio::input::mouse::HostPixelExtent::new(106, 20, 1060, 400)
        .expect("host geometry");
    let x = u32::from(pane.inner_rect.x) * 10 + 21;
    let y = u32::from(pane.inner_rect.y) * 20 + 21;
    let report = format!("\x1b[<0;{x};{y}M");
    let mut framer = shepr_termio::input::raw_input::RawInputFramer::<
        shepr_termio::input::raw_input::NoHostReplies,
    >::default();
    let mut framed = framer.push_framed(report.as_bytes());
    framed.extend(framer.flush_timeout_framed());
    assert_eq!(framed.len(), 1);
    let framed = framed.pop().expect("one framed pixel mouse");
    let outcome = state.handle_host_input(
        vec![crate::ParsedHostInput {
            event: framed.event,
            pixel_mouse: Some(shepr_termio::input::mouse::HostPixels { x, y, geometry }),
        }],
        false,
        std::time::Instant::now(),
    );
    assert!(matches!(
        &outcome.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "w1:p1"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        kind: shepr_protocol::ClientMouseKind::Down(
                            shepr_protocol::ClientMouseButton::Left
                        ),
                        position: ClientMousePosition::Pixels { x: 20, y: 20, .. },
                        ..
                    }]
                )
    ));

    let lost = state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    assert!(matches!(
        &lost.requests[..],
        [
            ClientMessage::ClientShellPaneInput { pane_id, events },
            ClientMessage::ClientShellFocus { focused: false }
        ] if pane_id == "w1:p1" && matches!(
            &events[..],
            [ClientPaneInputEvent::Mouse {
                kind: shepr_protocol::ClientMouseKind::Up(
                    shepr_protocol::ClientMouseButton::Left
                ),
                position: ClientMousePosition::Pixels { x: 20, y: 20, .. },
                ..
            }]
        )
    ));
}

#[test]
fn pane_owned_right_click_forwards_the_complete_gesture() {
    let mut snapshot = snapshot();
    snapshot.panes[0].right_click_passthrough = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();

    let down = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: pane.inner_rect.x + 1,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &down.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, .. }] if pane_id == "w1:p1"
    ));
    assert!(state.overlay.is_none());
    assert!(state.pane_mouse_gesture.is_some());

    let up = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Right),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &up.requests[..],
        [ClientMessage::ClientShellPaneInput { pane_id, events }]
            if pane_id == "w1:p1"
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        kind: shepr_protocol::ClientMouseKind::Up(
                            shepr_protocol::ClientMouseButton::Right
                        ),
                        ..
                    }]
                )
    ));
    assert!(state.pane_mouse_gesture.is_none());
}

#[test]
fn context_menu_keyboard_and_outside_click_are_client_owned() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");
    let workspace = state.hits.workspaces[0].rect;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: workspace.x + 1,
        row: workspace.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.compose(106, 20).expect("workspace context menu");
    let moved = state.handle_input_bytes(b"\x1b[B");
    assert!(moved.repaint);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            highlighted: 1,
            ..
        }))
    ));
    let paste = state.handle_raw_events(vec![RawInputEvent::Paste("not pane input".into())]);
    assert!(paste.requests.is_empty());
    let outside =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 105,
            row: 19,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(outside.repaint);
    assert!(state.overlay.is_none());
}
