//! Mouse input through the whole shell: pane selection and copy, word selection,
//! split and sidebar drags, pane mouse reporting and the right-click menu.

use super::{Instant, MOUSE_DRAG_SEND_INTERVAL};
use crate::shell::config::ClientShellConfig;
use crate::shell::input::pointer::{ClientChromeDrag, Throttle};
use crate::shell::overlays::Overlay;
use crate::shell::overlays::context_menu::ContextMenuOverlay;
use crate::shell::state::{
    ClientShellAction, ClientShellEndpointError, ClientShellInput, ClientShellRequest,
    ClientShellState,
};
use crate::shell::tests::{pane_scroll_result, snapshot, surface};
use crate::shell::view::PaneSplitHit;
use crate::tests::test_pane_id;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_config::ClientConfig;
use shepr_config::theme::Palette;
use shepr_core::layout::SplitBranch;
use shepr_protocol::command::{EndpointCommand, EndpointReply};
use shepr_protocol::{
    ClientMessage, ClientMousePosition, ClientPaneInputEvent, FrameData, PaneSurfaceFrame,
    PaneSurfaceSplit, PaneSurfaceSplitDirection, SurfaceRect,
};
use shepr_surface::ratatui_conversion::{FrameDataExt as _, WireColorExt as _};
use shepr_term::host::{DefaultColorKind, HostAppearance};
use shepr_termio::input::raw_input::RawInputEvent;

fn split_surface(
    boot_id: shepr_protocol::BootId,
    revision: u64,
    epoch: shepr_core::layout::LayoutEpoch,
) -> PaneSurfaceFrame {
    let buffer = Buffer::with_lines(["x"]);
    PaneSurfaceFrame {
        boot_id,
        projection_revision: shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(
            revision,
        ),
        surface_revision: shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(1),
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
            .expect("test buffer is a valid frame"),
        panes: Vec::new(),
        splits: vec![PaneSurfaceSplit {
            direction: shepr_protocol::PaneSurfaceSplitDirection::Horizontal,
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
            path: vec![SplitBranch::First],
            epoch,
        }],
    }
}

fn split_drag_state(with_changed_pending_topology: bool) -> ClientShellState {
    let mut snapshot = crate::shell::tests::snapshot();
    snapshot.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1);
    let boot_id = snapshot.boot_id.clone();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot));

    let epoch = shepr_core::layout::LayoutEpoch::default();
    let surface = split_surface(boot_id.clone(), 1, epoch);
    state.receive_pane_surface_from(
        surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let mut next = crate::shell::tests::snapshot();
    next.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    state.set_snapshot(Box::new(next));
    if with_changed_pending_topology {
        state.receive_pane_surface_from(
            split_surface(boot_id, 3, epoch.next()),
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
    }
    state.pointer.chrome_drag = Some(ClientChromeDrag::PaneSplit {
        hit: PaneSplitHit {
            direction: shepr_protocol::PaneSurfaceSplitDirection::Horizontal,
            pos: 40,
            area: Rect::new(0, 0, 80, 19),
            hit_rect: Rect::new(40, 0, 1, 19),
            path: vec![SplitBranch::First],
            epoch,
        },
        workspace_id: shepr_protocol::WorkspaceId::from_number(1)
            .expect("one-based workspace number"),
        grab_offset: 0,
        last_sent_ratio: Some(shepr_core::layout::SplitRatio::clamped(0.5)),
        throttle: Throttle::new(MOUSE_DRAG_SEND_INTERVAL),
    });
    state
}

fn release_split_drag(state: &mut ClientShellState) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 60,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        },
        Instant::now(),
        &mut outcome,
    );
    outcome
}

#[test]
fn split_release_sends_final_ratio_during_projection_gap() {
    let mut state = split_drag_state(false);

    let outcome = release_split_drag(&mut state);

    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.command,
                shepr_protocol::command::EndpointCommand::LayoutSetSplitRatio(params)
                    if (params.ratio.get() - 0.75).abs() < f32::EPSILON
                        && params.path == vec![SplitBranch::First]
                        && params.epoch == shepr_core::layout::LayoutEpoch::default()
            )
    ));
}

#[test]
fn split_release_is_rejected_when_a_received_future_surface_changed_topology() {
    let mut state = split_drag_state(true);

    let outcome = release_split_drag(&mut state);

    assert!(outcome.actions.is_empty());
}

#[test]
fn selection_repaint_cadence_keeps_one_deadline_and_flushes_when_input_stops() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let now = std::time::Instant::now();
    let ms = std::time::Duration::from_millis;
    state.presentation.set_composed_at(now);
    for elapsed in [1, 4, 8, 12, 15] {
        assert!(!state.request_selection_drag_repaint(now + ms(elapsed)));
        assert_eq!(state.mouse_selection.repaint_deadline, Some(now + ms(16)));
    }
    assert_eq!(state.next_timer_deadline(), Some(now + ms(16)));
    assert!(!state.tick_selection_autoscroll(now + ms(15)).repaint);
    assert!(state.tick_selection_autoscroll(now + ms(16)).repaint);
    assert!(state.mouse_selection.repaint_deadline.is_none());
    assert!(!state.tick_selection_autoscroll(now + ms(17)).repaint);
}

#[test]
fn idle_shell_has_no_timer_deadline() {
    let state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    assert_eq!(state.next_timer_deadline(), None);
}

#[test]
fn selection_repaint_cadence_allows_immediate_paint_when_due() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let now = std::time::Instant::now();
    assert!(state.request_selection_drag_repaint(now));
    state.presentation.set_composed_at(now);
    assert!(state.request_selection_drag_repaint(now + std::time::Duration::from_millis(16)));
    assert!(state.mouse_selection.repaint_deadline.is_none());
}

#[test]
fn selection_repaint_cadence_does_not_leave_work_after_another_composition() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let now = std::time::Instant::now();
    state.mouse_selection.repaint_deadline = Some(now);
    state.compose(106, 20).expect("frame");
    assert!(state.mouse_selection.repaint_deadline.is_none());
    assert!(!state.tick_selection_autoscroll(now).repaint);
}

#[test]
fn a_pane_without_scroll_metrics_takes_no_selection() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = None;
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("pane frame");
    let pane = state.pane_hits()[0].clone();
    let mut mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y + 1,
        modifiers: KeyModifiers::empty(),
    };
    let press = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(
        state.mouse_selection.selection.is_none(),
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
    assert!(state.mouse_selection.selection.is_none());
}

#[test]
fn selection_release_copies_latest_position_before_deferred_paint() {
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
    state.compose(106, 20).expect("pane frame");
    let pane = state.pane_hits()[0].clone();
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
    state.mouse_selection.repaint_deadline = Some(std::time::Instant::now());
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    let release = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(release.repaint);
    assert!(matches!(
        &release.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.command,
                EndpointCommand::PaneSelectionRead(params)
                    if params.cursor == shepr_protocol::command::PaneTextPoint {
                        row: shepr_term::AbsRow(0),
                        col: 2,
                    })
    ));
    state.compose(106, 20).expect("release frame");
    assert!(state.mouse_selection.repaint_deadline.is_none());
}

#[test]
fn pane_split_drag_uses_projected_handle_and_stable_child_identities() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    let mut second = pane_surface.panes[0].clone();
    second.pane_id = test_pane_id("w1:p2");
    second.rect.x = 40;
    second.inner_rect.x = 40;
    pane_surface.panes.push(second);
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
            shepr_core::layout::SplitBranch::First,
            shepr_core::layout::SplitBranch::Second,
        ],
        epoch: shepr_core::layout::LayoutEpoch::default(),
    });
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("split pane surface");
    let split = state.drawn().pane_splits()[0].clone();

    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: split.hit_rect.x,
        row: split.hit_rect.y + 2,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.pointer.chrome_drag,
        Some(ClientChromeDrag::PaneSplit { .. })
    ));
    let mut replacement = snapshot();
    replacement.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    replacement.workspaces[0].label = "updated".into();
    let mut replacement_surface = surface();
    replacement_surface.projection_revision =
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(2);
    let mut second = replacement_surface.panes[0].clone();
    second.pane_id = test_pane_id("w1:p2");
    second.rect.x = 40;
    second.inner_rect.x = 40;
    replacement_surface.panes.push(second);
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
            shepr_core::layout::SplitBranch::First,
            shepr_core::layout::SplitBranch::Second,
        ],
        epoch: shepr_core::layout::LayoutEpoch::default(),
    });
    state.set_snapshot(Box::new(replacement));
    state.receive_pane_surface_from(
        replacement_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
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
            if params.workspace_id == crate::tests::test_workspace_id("w1")
                && params.path
                    == vec![
                        shepr_core::layout::SplitBranch::First,
                        shepr_core::layout::SplitBranch::Second,
                    ]
                && params.epoch == shepr_core::layout::LayoutEpoch::default()
                && (params.ratio.get() - 0.6).abs() < f32::EPSILON
    ));
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: split.area.x + 48,
            row: split.hit_rect.y + 2,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(release.actions.is_empty());
    assert!(state.pointer.chrome_drag.is_none());
}

#[test]
fn disabled_mouse_chrome_removes_split_drag_hits() {
    let mut config = ClientConfig::default();
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
        epoch: shepr_core::layout::LayoutEpoch::default(),
    });
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("mouse-disabled shell");
    assert!(state.drawn().pane_splits().is_empty());
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
                    .mouse_selection
                    .selection
                    .as_ref()
                    .expect("test precondition")
                    .is_in_progress()
            );
            state.tick_timers(std::time::Instant::now() + std::time::Duration::from_secs(1));
            assert!(
                state
                    .mouse_selection
                    .selection
                    .as_ref()
                    .expect("test precondition")
                    .is_visible()
            );
            actions = word_drag_mouse(&mut state, release, 0, 8).actions;
        }
        assert!(
            state
                .mouse_selection
                .selection
                .as_ref()
                .expect("test precondition")
                .is_finalized()
        );
        assert_eq!(
            state
                .mouse_selection
                .selection
                .as_ref()
                .expect("test precondition")
                .ordered_rows(),
            (
                shepr_term::Point::new(shepr_term::AbsRow(0), 6),
                shepr_term::Point::new(shepr_term::AbsRow(0), 10)
            )
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
                state
                    .tick_timers(
                        state
                            .mouse_selection
                            .highlight_clear_deadline
                            .expect("test precondition")
                    )
                    .repaint
            );
            assert!(state.mouse_selection.selection.is_none());
        } else {
            assert!(actions.is_empty(), "manual selection must not auto-copy");
            state.tick_timers(std::time::Instant::now() + std::time::Duration::from_secs(1));
            assert!(
                state.mouse_selection.selection.is_some(),
                "manual selection must not expire"
            );
        }
    }
}

fn word_drag_state(copy_on_select: bool) -> ClientShellState {
    let mut config = ClientConfig::default();
    config.ui.copy_on_select = copy_on_select;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    let buffer = Buffer::with_lines([
        "alpha bravo charlie",
        "delta echo foxtrot ",
        "golf hotel india   ",
    ]);
    pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
        .expect("test buffer is a valid frame");
    pane_surface.panes[0].rect.width = 19;
    pane_surface.panes[0].rect.height = 3;
    pane_surface.panes[0].inner_rect = pane_surface.panes[0].rect;
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("composed frame");
    state
}

fn word_drag_mouse(
    state: &mut ClientShellState,
    kind: MouseEventKind,
    row: u16,
    col: u16,
) -> ClientShellInput {
    let pane = state.pane_hits()[0].clone();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind,
        column: pane.inner_rect.x + col,
        row: pane.inner_rect.y + row,
        modifiers: KeyModifiers::empty(),
    })])
}

fn word_read_id(actions: &[ClientShellAction]) -> shepr_protocol::RequestId {
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

fn word_row_reply(
    state: &mut ClientShellState,
    id: &shepr_protocol::RequestId,
    text: &str,
) -> Vec<ClientShellAction> {
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

fn start_word_drag(state: &mut ClientShellState) -> shepr_protocol::RequestId {
    word_drag_mouse(state, MouseEventKind::Down(MouseButton::Left), 0, 8);
    word_drag_mouse(state, MouseEventKind::Up(MouseButton::Left), 0, 8);
    assert!(
        state.mouse_selection.selection.is_none(),
        "plain clicks must not select"
    );
    let second = word_drag_mouse(state, MouseEventKind::Down(MouseButton::Left), 0, 8);
    assert!(second.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
            if params.anchor.col == 0 && params.cursor.col == state.pane_hits()[0].inner_rect.width - 1))));
    word_read_id(&second.actions)
}

#[test]
fn mismatched_boot_word_row_result_cancels_the_pending_gesture() {
    let mut state = word_drag_state(false);
    let request_id = start_word_drag(&mut state);
    assert!(state.mouse_selection.word_gesture.is_some());
    // The first click also asked to focus the pane; that unrelated request
    // stays pending and is not part of this assertion.
    state.ledger.retain(|id| id == &request_id);

    let outcome = state.answer_request(
        &crate::tests::test_boot_id("replacement-boot"),
        &request_id,
        Err(ClientShellEndpointError::Server(
            shepr_protocol::command::EndpointError::StaleBoot,
        )),
        std::time::Instant::now(),
    );

    assert!(outcome.repaint);
    assert!(state.ledger.is_empty());
    assert!(state.mouse_selection.word_gesture.is_none());
    assert!(state.notices.visible().is_none());
}

#[test]
fn disconnecting_a_pending_word_row_read_does_not_show_an_interrupted_action_notice() {
    let mut state = word_drag_state(false);
    let request_id = start_word_drag(&mut state);
    // The first click also asked to focus the pane; a generic action does show
    // the interrupted notice, so leave only the word read pending.
    state.ledger.retain(|id| id == &request_id);

    state.mark_endpoint_disconnected(&crate::endpoint::ClientEndpointId::Local);

    assert!(state.ledger.is_empty());
    assert!(state.mouse_selection.word_gesture.is_none());
    assert!(state.notices.visible().is_none());
}

#[test]
fn double_click_drag_selects_whole_words_in_both_directions() {
    let mut state = word_drag_state(false);
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    for (col, expected) in [
        (
            14,
            ((shepr_term::AbsRow(0), 6), (shepr_term::AbsRow(0), 18)),
        ),
        (2, ((shepr_term::AbsRow(0), 0), (shepr_term::AbsRow(0), 10))),
        (8, ((shepr_term::AbsRow(0), 6), (shepr_term::AbsRow(0), 10))),
        (
            11,
            ((shepr_term::AbsRow(0), 6), (shepr_term::AbsRow(0), 11)),
        ),
        (
            16,
            ((shepr_term::AbsRow(0), 6), (shepr_term::AbsRow(0), 18)),
        ),
    ] {
        let motion = word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, col);
        assert!(
            motion.actions.is_empty(),
            "reuse the row while dragging within it"
        );
        let (start, end) = state
            .mouse_selection
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_rows();
        assert_eq!(((start.row, start.col), (end.row, end.col)), expected);
    }
    assert!(
        word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 16)
            .actions
            .is_empty()
    );
    assert!(
        state
            .mouse_selection
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
                if params.anchor.row == shepr_term::AbsRow(2)
                    && params.cursor.row == shepr_term::AbsRow(2)))
        );
        let copy = word_row_reply(&mut state, &word_read_id(&final_read), "golf hotel india");
        assert!(
            matches!(&copy[..], [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
                if params.anchor == shepr_protocol::command::PaneTextPoint {
                    row: shepr_term::AbsRow(0),
                    col: 6,
                }
                    && params.cursor == shepr_protocol::command::PaneTextPoint {
                        row: shepr_term::AbsRow(2),
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
        assert!(state.mouse_selection.selection.is_none());
    }
}

#[test]
fn double_click_drag_survives_focus_lag_after_anchor_reply() {
    let mut state = word_drag_state(true);
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    let mut lagging = snapshot();
    lagging.focused_pane_id = None;
    state.set_snapshot(Box::new(lagging));
    assert!(state.mouse_selection.selection.is_some());
    word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 14);
    assert_eq!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(0), 6),
            shepr_term::Point::new(shepr_term::AbsRow(0), 18)
        )
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
        projected.focused_pane_id = Some(test_pane_id(pane_id));
        projected
    };
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(focused_on("w1:p2")));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("pane frame");
    let pane = state.pane_hits()[0].clone();
    assert_eq!(pane.pane_id.to_string(), "w1:p1");
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
    assert!(
        state.mouse_selection.selection.is_some(),
        "focus lag cancelled the drag"
    );
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        pane.inner_rect.x + 2,
    )]);
    assert_eq!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("drag continues")
            .ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(0), 0),
            shepr_term::Point::new(shepr_term::AbsRow(0), 2)
        )
    );

    state.set_snapshot(Box::new(focused_on("w1:p1")));
    assert!(state.mouse_selection.selection.is_some());
    assert!(state.mouse_selection.focus_pending.is_none());
    // Once focus has landed, moving it away again ends the selection.
    state.set_snapshot(Box::new(focused_on("w1:p2")));
    assert!(state.mouse_selection.selection.is_none());
}

#[test]
fn selection_in_focused_pane_still_ends_when_focus_moves() {
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
    state.compose(106, 20).expect("pane frame");
    let pane = state.pane_hits()[0].clone();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(state.mouse_selection.focus_pending.is_none());
    let mut moved = snapshot();
    let mut other = moved.panes[0].clone();
    other.pane_id = test_pane_id("w1:p2");
    moved.panes.push(other);
    moved.focused_pane_id = Some(test_pane_id("w1:p2"));
    state.set_snapshot(Box::new(moved));
    assert!(state.mouse_selection.selection.is_none());
}

#[test]
fn double_click_drag_invalidates_cached_boundaries_outside_selected_cells() {
    for copy_on_select in [false, true] {
        let mut state = word_drag_state(copy_on_select);
        let initial = start_word_drag(&mut state);
        word_row_reply(&mut state, &initial, "alpha bravo charlie");
        let mut changed = state.pane_surface().expect("test precondition").clone();
        changed.surface_revision = changed
            .surface_revision
            .checked_next()
            .expect("test precondition");
        changed.panes[0].content_revision.advance();
        changed.frame.cells_mut()[14].symbol = " ".into();
        state.receive_pane_surface_from(
            changed,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        assert!(
            state.mouse_selection.selection.is_none(),
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
        assert!(state.mouse_selection.selection.is_none());
    }
}

#[test]
fn reconnect_word_selection_tracks_content_changes() {
    for content_changed in [false, true] {
        let mut state = word_drag_state(true);
        let initial = start_word_drag(&mut state);
        word_row_reply(&mut state, &initial, "alpha bravo charlie");
        let mut next_surface = state.pane_surface().expect("test precondition").clone();
        if content_changed {
            next_surface.panes[0].content_revision.advance();
            next_surface.frame.cells_mut()[14].symbol = " ".into();
        }
        let endpoint_id = state.active_endpoint_id().clone();
        let snapshot = std::sync::Arc::clone(
            state
                .endpoints
                .active
                .shared_snapshot()
                .expect("test precondition"),
        );
        state.mark_endpoint_disconnected(&endpoint_id);
        state.endpoint_connected(&endpoint_id, crate::tests::test_generation(1));
        state.cache_endpoint_snapshot_for_generation(
            &endpoint_id,
            crate::tests::test_generation(1),
            snapshot,
        );
        assert!(state.activate_endpoint_projection(&endpoint_id));
        state.receive_pane_surface_from(
            next_surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );

        assert_eq!(state.mouse_selection.selection.is_some(), !content_changed);
        assert_eq!(
            state.mouse_selection.word_gesture.is_some(),
            !content_changed
        );
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
            state.set_snapshot(Box::new(lagging));
            let mut unfocused = snapshot();
            unfocused.focused_pane_id = Some(test_pane_id("w1:p2"));
            let mut other = unfocused.panes[0].clone();
            other.pane_id = test_pane_id("w1:p2");
            unfocused.panes.push(other);
            state.set_snapshot(Box::new(unfocused));
        } else {
            let mut changed = state.pane_surface().expect("test precondition").clone();
            changed.surface_revision = changed
                .surface_revision
                .checked_next()
                .expect("test precondition");
            changed.panes[0].content_revision.advance();
            state.receive_pane_surface_from(
                changed,
                state
                    .endpoints
                    .active
                    .generation()
                    .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
            );
        }
        assert!(
            word_row_reply(&mut state, &initial, "alpha bravo charlie").is_empty(),
            "a stale released gesture must not copy"
        );
        assert!(state.mouse_selection.selection.is_none());
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
        let mut resized = state.pane_surface().expect("test precondition").clone();
        resized.surface_revision = resized
            .surface_revision
            .checked_next()
            .expect("test precondition");
        resized.panes[0].rect.width += 5;
        resized.panes[0].inner_rect.width += 5;
        state.receive_pane_surface_from(
            resized,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        assert!(word_row_reply(&mut state, &pending, "alpha bravo charlie extra").is_empty());
        assert!(
            state.mouse_selection.selection.is_none(),
            "a late reply must not restore a resized selection"
        );
        assert!(state.mouse_selection.autoscroll.is_none());
    }
}

#[test]
fn double_click_drag_autoscroll_keeps_absolute_word_anchor() {
    let mut state = word_drag_state(false);
    state.pane_hits_mut()[0].scroll = Some(shepr_term::ScrollMetrics::new(
        5,
        10,
        3,
        shepr_term::AbsRow(0),
    ));
    let initial = start_word_drag(&mut state);
    word_row_reply(&mut state, &initial, "alpha bravo charlie");
    word_drag_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 0, 14);
    let tick = state.tick_selection_autoscroll(
        state
            .mouse_selection
            .autoscroll_deadline
            .expect("test precondition"),
    );
    word_row_reply(
        &mut state,
        &word_read_id(&tick.actions),
        "delta echo foxtrot",
    );
    assert_eq!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(4), 11),
            shepr_term::Point::new(shepr_term::AbsRow(5), 10)
        )
    );
    word_drag_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 0, 14);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("test precondition")
            .is_finalized()
    );
    assert!(state.mouse_selection.autoscroll.is_none());
}

#[test]
fn pane_content_updates_preserve_live_ranges_until_geometry_or_screen_changes() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let surface_at = |surface_revision, content_revision, alternate_screen_active| {
        let mut pane_surface = surface();
        pane_surface.surface_revision =
            shepr_test_fixtures::counter_at::<shepr_protocol::SurfaceRevision>(surface_revision);
        pane_surface.panes[0].content_revision = shepr_test_fixtures::counter_at(content_revision);
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
            0,
            11,
            2,
            shepr_term::AbsRow(0),
        ));
        pane_surface.panes[0].alternate_screen_active = alternate_screen_active;
        pane_surface
    };
    state.receive_pane_surface_from(
        surface_at(1, 0, true),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("composed frame");
    let pane = state.pane_hits()[0].clone();
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
    updated_surface.frame.cells_mut()[0].symbol = "W".into();
    state.receive_pane_surface_from(
        updated_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("updated frame");

    let drag = state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        pane.inner_rect.x + 1,
        pane.inner_rect.y + 1,
    )]);

    assert!(drag.repaint || state.mouse_selection.repaint_deadline.is_some());
    let selection = state
        .mouse_selection
        .selection
        .as_ref()
        .expect("visible selection");
    assert!(selection.is_visible());
    assert_eq!(
        selection.ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(12), 0),
            shepr_term::Point::new(shepr_term::AbsRow(12), 1)
        )
    );

    let mut replaced_surface = surface_at(3, 4, true);
    replaced_surface.frame.cells_mut()[4].symbol = "X".into();
    state.receive_pane_surface_from(
        replaced_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert_eq!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(12), 0),
            shepr_term::Point::new(shepr_term::AbsRow(12), 1)
        )
    );

    // The selected row can leave the viewport during a drag. A later patch,
    // including an in-flight content revision, must keep that absolute range.
    let mut scrolled = surface_at(4, 5, true);
    {
        let metrics = scrolled.panes[0]
            .scroll
            .as_mut()
            .expect("test precondition");
        *metrics = metrics.with_offset(2);
    }
    assert!(matches!(
            state.apply_pane_surface_patch_from(
                &shepr_protocol::PaneSurfacePatch {
                    boot_id: scrolled.boot_id,
                    projection_revision: scrolled.projection_revision,
                    base_surface_revision: shepr_test_fixtures::counter_at::<
                        shepr_protocol::SurfaceRevision,
                    >(3),
                    surface_revision: shepr_test_fixtures::counter_at::<
                        shepr_protocol::SurfaceRevision,
                    >(4),
                    panes: scrolled.panes,
                    rows: vec![],
                    cursor: scrolled.frame.cursor().cloned(),
                },
                state
                    .endpoints
                    .active
                    .generation()
                    .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
            ),
            crate::shell::presentation::surface_patch::ClientPaneSurfacePatchOutcome::Applied(_)
        ));
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("test precondition")
            .is_in_progress()
    );
    assert_eq!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("test precondition")
            .ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(12), 0),
            shepr_term::Point::new(shepr_term::AbsRow(12), 1)
        )
    );

    for (surface_revision, content_revision, width, alternate_screen_active) in
        [(5, 6, 4, false), (6, 8, 3, false)]
    {
        state.mouse_selection.selection = Some(shepr_term::selection::Selection::anchor(
            test_pane_id("w1:p1"),
            shepr_term::Point::new(shepr_term::AbsRow(12), 0),
        ));
        let mut changed_surface =
            surface_at(surface_revision, content_revision, alternate_screen_active);
        changed_surface.panes[0].inner_rect.width = width;
        changed_surface.panes[0].alternate_screen_active = alternate_screen_active;
        state.receive_pane_surface_from(
            changed_surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        assert!(state.mouse_selection.selection.is_none());
    }
}

#[test]
fn pane_mouse_input_keeps_stable_target_and_endpoint_encoding() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("composed frame");
    let pane = state.pane_hits()[0].clone();

    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x + 2,
        row: pane.inner_rect.y + 1,
        modifiers: KeyModifiers::ALT,
    })]);
    let [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events })] =
        &click.requests[..]
    else {
        panic!("pane application click should use targeted canonical input");
    };
    assert_eq!(pane_id.to_string(), "w1:p1");
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
    assert!(state.pointer.pane_mouse_gesture.is_some());
    state.pane_hits_mut().clear();
    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::ALT,
        })]);
    assert!(matches!(
        &release.requests[..],
        [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events })]
            if pane_id == &crate::tests::test_pane_id("w1:p1")
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
    assert!(state.pointer.pane_mouse_gesture.is_none());
}

#[test]
fn pane_pixel_mouse_preserves_pane_relative_pixel_coordinates() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    let inner = pane_surface.panes[0].inner_rect;
    let extent = shepr_core::geometry::PanePixelExtent::new(
        shepr_core::geometry::GridSize::new(inner.width, inner.height).expect("inner grid"),
        39,
        38,
    )
    .expect("nonzero extent");
    pane_surface.panes[0].pixel_mouse = shepr_term::mouse::PanePixelMouse::new(true, Some(extent));
    state.set_host_cell(shepr_core::geometry::HostCell::Exact(
        shepr_core::geometry::CellPx::new(10, 20).expect("valid cell"),
    ));
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("composed frame");
    let pane = state.pane_hits()[0].clone();
    let geometry = shepr_termio::input::mouse::HostPixelExtent::new(106, 20, 1060, 400)
        .expect("host geometry");
    let x = u32::from(pane.inner_rect.x) * 10 + 21;
    let y = u32::from(pane.inner_rect.y) * 20 + 21;
    let report = format!("\x1b[<0;{x};{y}M");
    let mut framer = shepr_termio::input::raw_input::RawInputFramer::default();
    let mut framed = framer.push_framed(report.as_bytes());
    framed.extend(framer.flush_timeout_framed());
    assert_eq!(framed.len(), 1);
    let framed = framed.pop().expect("one framed pixel mouse");
    let outcome = state.handle_host_input(
        vec![crate::events::ParsedHostInput {
            event: framed.event,
            pixel_mouse: Some(shepr_termio::input::mouse::HostPixels { x, y, geometry }),
        }],
        false,
        std::time::Instant::now(),
    );
    assert!(matches!(
        &outcome.requests[..],
        [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events })]
            if pane_id == &crate::tests::test_pane_id("w1:p1")
                && matches!(
                    &events[..],
                    [ClientPaneInputEvent::Mouse {
                        kind: shepr_protocol::ClientMouseKind::Down(
                            shepr_protocol::ClientMouseButton::Left
                        ),
                        position: ClientMousePosition::Pixels { report, .. },
                        ..
                    }] if (report.x(), report.y()) == (20, 20) && report.extent() == extent
                )
    ));

    let lost = state.handle_raw_events(vec![RawInputEvent::OuterFocusLost]);
    assert!(matches!(
        &lost.requests[..],
        [
            ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events }),
            ClientShellRequest::Shown(ClientMessage::ClientShellFocus { focused: false })
        ] if pane_id == &crate::tests::test_pane_id("w1:p1") && matches!(
            &events[..],
            [ClientPaneInputEvent::Mouse {
                kind: shepr_protocol::ClientMouseKind::Up(
                    shepr_protocol::ClientMouseButton::Left
                ),
                position: ClientMousePosition::Pixels { report, .. },
                ..
            }] if (report.x(), report.y()) == (20, 20)
        )
    ));
}

/// The host's pixel report for the cell at `column`, `row` of a 106x20 host
/// of 10x20 cells, offset `dx`, `dy` pixels inside it, as one host input.
fn pixel_mouse_down(
    state: &mut ClientShellState,
    pane: &crate::shell::view::PaneHit,
    cell: (u16, u16),
    offset: (u32, u32),
) -> ClientShellInput {
    let geometry = shepr_termio::input::mouse::HostPixelExtent::new(106, 20, 1060, 400)
        .expect("host geometry");
    let x = u32::from(pane.inner_rect.x + cell.0) * 10 + 1 + offset.0;
    let y = u32::from(pane.inner_rect.y + cell.1) * 20 + 1 + offset.1;
    let report = format!("\x1b[<0;{x};{y}M");
    let mut framer = shepr_termio::input::raw_input::RawInputFramer::default();
    let mut framed = framer.push_framed(report.as_bytes());
    framed.extend(framer.flush_timeout_framed());
    let framed = framed.pop().expect("one framed pixel mouse");
    state.handle_host_input(
        vec![crate::events::ParsedHostInput {
            event: framed.event,
            pixel_mouse: Some(shepr_termio::input::mouse::HostPixels { x, y, geometry }),
        }],
        false,
        std::time::Instant::now(),
    )
}

fn pixel_pane_state(
    pane_pixel_mouse: shepr_term::mouse::PanePixelMouse,
) -> (ClientShellState, crate::shell::view::PaneHit) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    pane_surface.panes[0].pixel_mouse = pane_pixel_mouse;
    state.set_host_cell(shepr_core::geometry::HostCell::Exact(
        shepr_core::geometry::CellPx::new(10, 20).expect("valid cell"),
    ));
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("composed frame");
    let pane = state.pane_hits()[0].clone();
    (state, pane)
}

#[test]
fn pane_pixel_mouse_rescales_into_a_foreign_pane_extent() {
    // The child believes in 8x16 cells while this host's cell is 10x20.
    let extent = shepr_core::geometry::PanePixelExtent::new(
        shepr_core::geometry::GridSize::new(4, 2).expect("inner grid"),
        32,
        32,
    )
    .expect("nonzero extent");
    let (mut state, pane) =
        pixel_pane_state(shepr_term::mouse::PanePixelMouse::new(true, Some(extent)));
    // Host cell (2, 1), 5 pixels right and 10 down inside it: half way across
    // the cell, so half way across the child's 8x16 cell (3rd column, 2nd row).
    let outcome = pixel_mouse_down(&mut state, &pane, (2, 1), (5, 10));
    let [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { events, .. })] =
        &outcome.requests[..]
    else {
        panic!(
            "one targeted pixel mouse input, got {:?}",
            outcome.requests.len()
        );
    };
    let [
        ClientPaneInputEvent::Mouse {
            position:
                ClientMousePosition::Pixels {
                    column: 2,
                    row: 1,
                    report,
                },
            ..
        },
    ] = &events[..]
    else {
        panic!("a pixel position, got {events:?}");
    };
    assert_eq!((report.x(), report.y()), (2 * 8 + 4 + 1, 16 + 8 + 1));
    assert_eq!(report.extent(), extent);
}

#[test]
fn pane_presented_at_another_grid_reports_cells() {
    // The extent belongs to a grid other than the one this pane is shown at.
    let extent = shepr_core::geometry::PanePixelExtent::new(
        shepr_core::geometry::GridSize::new(40, 20).expect("other grid"),
        320,
        320,
    )
    .expect("nonzero extent");
    let (mut state, pane) =
        pixel_pane_state(shepr_term::mouse::PanePixelMouse::new(true, Some(extent)));
    let outcome = pixel_mouse_down(&mut state, &pane, (2, 1), (5, 10));
    let [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { events, .. })] =
        &outcome.requests[..]
    else {
        panic!("one targeted mouse input");
    };
    assert!(matches!(
        &events[..],
        [ClientPaneInputEvent::Mouse {
            position: ClientMousePosition::Cell { column: 2, row: 1 },
            ..
        }]
    ));
}

#[test]
fn pane_owned_right_click_forwards_the_complete_gesture() {
    let mut snapshot = snapshot();
    snapshot.panes[0].right_click_passthrough = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("composed frame");
    let pane = state.pane_hits()[0].clone();

    let down = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: pane.inner_rect.x + 1,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &down.requests[..],
        [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, .. })]
            if pane_id == &crate::tests::test_pane_id("w1:p1")
    ));
    assert!(state.overlay.is_none());
    assert!(state.pointer.pane_mouse_gesture.is_some());

    let up = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Right),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &up.requests[..],
        [ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events })]
            if pane_id == &crate::tests::test_pane_id("w1:p1")
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
    assert!(state.pointer.pane_mouse_gesture.is_none());
}

#[test]
fn context_menu_keyboard_and_outside_click_are_client_owned() {
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
        Some(Overlay::ContextMenu(ContextMenuOverlay {
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

#[test]
fn client_selection_uses_host_background_and_repaints_when_it_changes() {
    use ratatui::style::Color;
    use shepr_term::host::RgbColor;

    for explicit_appearance in [false, true] {
        let mut values = ClientConfig::default();
        values.theme.name = Some("terminal".into());
        let config = ClientShellConfig::from_config(&values);
        assert_eq!(config.palette, Palette::terminal());
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
        state.compose(106, 20).expect("composed frame");
        let pane = state.pane_hits()[0].clone();
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), pane.inner_rect.x),
            (
                MouseEventKind::Drag(MouseButton::Left),
                pane.inner_rect.x + 2,
            ),
        ] {
            state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row: pane.inner_rect.y,
                modifiers: KeyModifiers::empty(),
            })]);
        }
        let cell_index = usize::from(pane.inner_rect.y) * 106 + usize::from(pane.inner_rect.x);
        let fallback = state.compose(106, 20).expect("fallback frame");
        assert_eq!(
            fallback.cells()[cell_index].bg,
            shepr_protocol::WireColor::from_ratatui(Color::DarkGray)
        );
        if explicit_appearance {
            state.handle_raw_events(vec![RawInputEvent::HostColorSchemeChanged(
                HostAppearance::Light,
            )]);
        }
        for (background, selected_bg, selected_fg) in [
            ((237, 237, 234), (171, 171, 168), (0, 0, 0)),
            ((26, 27, 38), (90, 91, 99), (255, 255, 255)),
        ] {
            let (r, g, b) = background;
            let outcome = state.handle_raw_events(vec![RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor { r, g, b },
            }]);
            assert!(
                outcome
                    .requests
                    .iter()
                    .any(|request| matches!(request, ClientShellRequest::HostTheme(_)))
            );
            let frame = state.compose(106, 20).expect("host-colored selection");
            let cell = &frame.cells()[cell_index];
            assert_eq!(
                cell.bg,
                shepr_protocol::WireColor::from_ratatui(Color::Rgb(
                    selected_bg.0,
                    selected_bg.1,
                    selected_bg.2
                ))
            );
            assert_eq!(
                cell.fg,
                shepr_protocol::WireColor::from_ratatui(Color::Rgb(
                    selected_fg.0,
                    selected_fg.1,
                    selected_fg.2
                ))
            );
            assert!(
                outcome.repaint,
                "host background changes must repaint selection"
            );
        }
    }
}

#[test]
fn client_mouse_selection_highlights_and_copies_through_endpoint_extraction() {
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
    let pane = state.pane_hits()[0].clone();

    let down = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &down.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.command,
                EndpointCommand::PaneFocus(target) if target.pane_id == crate::tests::test_pane_id("w1:p1")
            )
    ));
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.is_visible())
    );

    let drag = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: pane.inner_rect.x + 2,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(drag.repaint || state.mouse_selection.repaint_deadline.is_some());
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_term::selection::Selection::is_visible)
    );
    let selected = state.compose(106, 20).expect("selected frame");
    let selected_cell =
        &selected.cells()[usize::from(pane.inner_rect.y) * 106 + usize::from(pane.inner_rect.x)];
    assert_ne!(
        selected_cell.bg,
        shepr_protocol::WireColor::from_ratatui(ratatui::style::Color::Reset)
    );

    let release =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: pane.inner_rect.x + 2,
            row: pane.inner_rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(state.mouse_selection.selection.is_none());
    let [ClientShellAction::Endpoint { request, .. }] = &release.actions[..] else {
        panic!("selection release should request endpoint extraction");
    };
    let request_id = request.id.clone();
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneSelectionRead(params)
            if params.pane_id == crate::tests::test_pane_id("w1:p1")
                && params.anchor == shepr_protocol::command::PaneTextPoint {
                    row: shepr_term::AbsRow(0),
                    col: 0,
                }
                && params.cursor == shepr_protocol::command::PaneTextPoint {
                    row: shepr_term::AbsRow(0),
                    col: 2,
                }
    ));

    let (_repaint, actions) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(EndpointReply::PaneSelection {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                text: "LIV".into(),
            }),
        )
        .into_parts();
    assert!(matches!(
        &actions[..],
        [ClientShellAction::ClipboardWrite(bytes)] if bytes == b"LIV"
    ));
}

#[test]
fn retained_mouse_selection_survives_output_and_copies_without_terminal_input() {
    let mut config = ClientConfig::default();
    config.ui.copy_on_select = false;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
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
    let pane = state.pane_hits()[0].clone();
    for event in [
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: pane.inner_rect.x,
            row: pane.inner_rect.y,
            modifiers: KeyModifiers::empty(),
        },
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: pane.inner_rect.x + 2,
            row: pane.inner_rect.y,
            modifiers: KeyModifiers::empty(),
        },
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: pane.inner_rect.x + 2,
            row: pane.inner_rect.y,
            modifiers: KeyModifiers::empty(),
        },
    ] {
        state.handle_raw_events(vec![RawInputEvent::Mouse(event)]);
        // Output can arrive between drag and release, including an in-flight revision.
        let mut updated = state.pane_surface().cloned().expect("pane surface");
        updated.panes[0].content_revision = shepr_test_fixtures::counter_at(1);
        updated.frame.cells_mut()[0].symbol = "x".into();
        state.receive_pane_surface_from(
            updated,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
    }
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_term::selection::Selection::is_finalized)
    );

    // A patch that redraws selected text must retain the same live terminal range.
    let mut updated = state.pane_surface().cloned().expect("pane surface");
    updated.panes[0].content_revision = shepr_test_fixtures::counter_at(1);
    let mut cell = updated.frame.cells()[0].clone();
    cell.symbol = "y".into();
    assert!(matches!(
        state.apply_pane_surface_patch_from(
            &shepr_protocol::PaneSurfacePatch {
                boot_id: updated.boot_id,
                projection_revision: updated.projection_revision,
                base_surface_revision: updated.surface_revision,
                surface_revision: updated
                    .surface_revision
                    .checked_next()
                    .expect("test precondition"),
                panes: updated.panes,
                rows: vec![shepr_protocol::PaneSurfacePatchRow {
                    x: 0,
                    y: 0,
                    cells: vec![cell]
                }],
                cursor: updated.frame.cursor().cloned(),
            },
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        crate::shell::presentation::surface_patch::ClientPaneSurfacePatchOutcome::Applied(_)
    ));
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_term::selection::Selection::is_finalized)
    );

    let highlighted = state.compose(106, 20).expect("highlighted frame");
    let cell_index = usize::from(pane.inner_rect.y) * 106 + usize::from(pane.inner_rect.x);
    let selected_cell = highlighted.cells()[cell_index].clone();
    let selection = state.mouse_selection.selection.take();
    let unselected = state.compose(106, 20).expect("unselected frame");
    assert_ne!(selected_cell.bg, unselected.cells()[cell_index].bg);
    state.mouse_selection.selection = selection;

    let copy = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )]);
    assert!(state.mouse_selection.selection.is_none());
    assert!(matches!(
        &copy.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(request.command, EndpointCommand::PaneSelectionRead(
                shepr_protocol::command::PaneSelectionReadParams { .. }
            ))
    ));
    assert!(copy.requests.is_empty());
    let request_id = match &copy.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };
    let (_, actions) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(EndpointReply::PaneSelection {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                text: "yIV".into(),
            }),
        )
        .into_parts();
    assert!(matches!(&actions[..], [ClientShellAction::ClipboardWrite(bytes)] if bytes == b"yIV"));
}

#[test]
fn selection_edge_drag_requests_scroll_and_timer_continues_it() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    // The pane starts one row down, as the lower pane of a split does, so the
    // drag has a row above it to leave through.
    pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &Buffer::with_lines(["    ", "LIVE", "PANE"]),
        None,
        &[],
    )
    .expect("test buffer is a valid frame");
    pane_surface.panes[0].rect.y = 1;
    pane_surface.panes[0].inner_rect.y = 1;
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        20,
        2,
        shepr_term::AbsRow(0),
    ));
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("composed frame");
    let pane = state.pane_hits()[0].clone();
    assert_eq!(pane.inner_rect.y, 1);
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y + 1,
        modifiers: KeyModifiers::empty(),
    })]);
    let drag = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y.saturating_sub(1),
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        &drag.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.command,
                EndpointCommand::PaneScroll(params)
                    if params.offset_from_bottom == 3
            )
    ));
    let drag_request_id = match &drag.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };
    let now = std::time::Instant::now();
    state.mouse_selection.autoscroll_deadline = Some(now);
    let tick = state.tick_selection_autoscroll(now);
    assert!(tick.actions.is_empty());
    let (_, next_scroll) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &drag_request_id,
            Ok(pane_scroll_result(3, 20, 3)),
        )
        .into_parts();
    assert!(matches!(
        &next_scroll[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.command,
                EndpointCommand::PaneScroll(params)
                    if params.offset_from_bottom == 4
            )
    ));
}

/// A split drag whose target is the presented surface, with `last_sent` as the ratio the
/// endpoint was last sent.
fn current_split_drag_state(last_sent: f32) -> ClientShellState {
    let mut snapshot = crate::shell::tests::snapshot();
    snapshot.revision = shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1);
    let boot_id = snapshot.boot_id.clone();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot));
    let epoch = shepr_core::layout::LayoutEpoch::default();
    state.receive_pane_surface_from(
        split_surface(boot_id, 1, epoch),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.pointer.chrome_drag = Some(ClientChromeDrag::PaneSplit {
        hit: PaneSplitHit {
            direction: shepr_protocol::PaneSurfaceSplitDirection::Horizontal,
            pos: 40,
            area: Rect::new(0, 0, 80, 19),
            hit_rect: Rect::new(40, 0, 1, 19),
            path: vec![SplitBranch::First],
            epoch,
        },
        workspace_id: shepr_protocol::WorkspaceId::from_number(1)
            .expect("one-based workspace number"),
        grab_offset: 0,
        last_sent_ratio: Some(shepr_core::layout::SplitRatio::clamped(last_sent)),
        throttle: Throttle::new(MOUSE_DRAG_SEND_INTERVAL),
    });
    state
}

fn drag_split_to(state: &mut ClientShellState, column: u16) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::empty(),
        },
        Instant::now(),
        &mut outcome,
    );
    outcome
}

#[test]
fn a_split_drag_does_not_resend_the_ratio_last_sent() {
    let mut state = current_split_drag_state(0.5);

    // Column 40 of an 80-column area is the ratio the endpoint already has.
    assert!(drag_split_to(&mut state, 40).actions.is_empty());

    let moved = drag_split_to(&mut state, 60);
    assert!(matches!(
        moved.actions.as_slice(),
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.command,
                EndpointCommand::LayoutSetSplitRatio(params)
                    if (params.ratio.get() - 0.75).abs() < f32::EPSILON
            )
    ));
    assert!(matches!(
        state.pointer.chrome_drag,
        Some(ClientChromeDrag::PaneSplit { last_sent_ratio: Some(ratio), .. })
            if (ratio.get() - 0.75).abs() < f32::EPSILON
    ));
}

#[test]
fn a_projection_reset_settles_an_owed_sidebar_width_resize() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    assert_eq!(state.next_timer_deadline(), None);
    state.pointer.chrome_drag = Some(ClientChromeDrag::SidebarWidth {
        resize_pending: true,
    });

    state.pointer.reset_for_projection();

    assert!(state.pointer.chrome_drag.is_none());
    // Due at once: the next loop pass settles it.
    assert_eq!(state.next_timer_deadline(), Some(state.now));
    let now = state.now;
    let settled = state.tick_timers(now);
    assert!(
        settled.resize,
        "the endpoint is still owed the dragged width"
    );
    assert_eq!(state.next_timer_deadline(), None);
    assert!(!state.tick_timers(now).resize, "settled once");
}

#[test]
fn a_projection_reset_owes_nothing_for_an_unmoved_sidebar_or_another_drag() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.pointer.chrome_drag = Some(ClientChromeDrag::SidebarWidth {
        resize_pending: false,
    });
    state.pointer.reset_for_projection();
    // The preference save is still due, but no resize.
    assert!(state.next_timer_deadline().is_some());
    let now = state.now;
    assert!(!state.tick_timers(now).resize);
    assert_eq!(state.next_timer_deadline(), None);

    state.pointer.chrome_drag = Some(ClientChromeDrag::WorkspaceScrollbar { grab_row_offset: 0 });
    state.pointer.reset_for_projection();
    assert!(state.pointer.chrome_drag.is_none());
    assert_eq!(state.next_timer_deadline(), None);
}

/// The splits of a layout with pane 1 on top and panes 2 | 3 below, in a 100 by 40
/// pane area: the root top/bottom split's divider on row 20 and the bottom row's left/right
/// divider on column 50, laid out the way the server publishes them (`split_hit_rect`), with
/// pane borders and, when `gaps`, pane gaps.
fn junction_layout_splits(gaps: bool) -> Vec<PaneSplitHit> {
    let epoch = shepr_core::layout::LayoutEpoch::default();
    let widen = u16::from(gaps);
    vec![
        PaneSplitHit {
            direction: PaneSurfaceSplitDirection::Horizontal,
            pos: 50,
            area: Rect::new(0, 20, 100, 20),
            hit_rect: Rect::new(50 - widen, 20, 1 + widen, 20),
            path: vec![SplitBranch::Second],
            epoch,
        },
        PaneSplitHit {
            direction: PaneSurfaceSplitDirection::Vertical,
            pos: 20,
            area: Rect::new(0, 0, 100, 40),
            hit_rect: Rect::new(0, 20 - widen, 100, 1 + widen),
            path: Vec::new(),
            epoch,
        },
    ]
}

fn grabbed(splits: &[PaneSplitHit], point: (u16, u16)) -> Option<PaneSurfaceSplitDirection> {
    super::split_hit_at(splits, point).map(|hit| hit.direction)
}

#[test]
fn a_press_at_a_border_junction_grabs_the_outer_split() {
    for gaps in [false, true] {
        let splits = junction_layout_splits(gaps);
        // The junction cell lies on both divider lines: the outer split wins, whichever
        // order the surface lists them in.
        assert_eq!(
            grabbed(&splits, (50, 20)),
            Some(PaneSurfaceSplitDirection::Vertical),
            "gaps: {gaps}"
        );
        let reversed = splits.iter().rev().cloned().collect::<Vec<_>>();
        assert_eq!(
            grabbed(&reversed, (50, 20)),
            Some(PaneSurfaceSplitDirection::Vertical),
            "gaps: {gaps}"
        );
        // The top/bottom border away from the junction.
        assert_eq!(
            grabbed(&splits, (10, 20)),
            Some(PaneSurfaceSplitDirection::Vertical),
            "gaps: {gaps}"
        );
        assert_eq!(
            grabbed(&splits, (80, 20)),
            Some(PaneSurfaceSplitDirection::Vertical),
            "gaps: {gaps}"
        );
        // The left/right border below the junction.
        assert_eq!(
            grabbed(&splits, (50, 30)),
            Some(PaneSurfaceSplitDirection::Horizontal),
            "gaps: {gaps}"
        );
        // Inside a pane, no split.
        assert_eq!(grabbed(&splits, (25, 30)), None, "gaps: {gaps}");
    }
}

#[test]
fn a_press_in_a_gap_beside_the_junction_grabs_the_border_whose_line_it_is_on() {
    let splits = junction_layout_splits(true);
    // Column 49 is the left/right split's gap column, but row 20 is the top/bottom
    // divider's own line.
    assert_eq!(
        grabbed(&splits, (49, 20)),
        Some(PaneSurfaceSplitDirection::Vertical)
    );
    // Row 19 is the top/bottom split's gap row, above the left/right split's area.
    assert_eq!(
        grabbed(&splits, (50, 19)),
        Some(PaneSurfaceSplitDirection::Vertical)
    );
    // The left/right gap column below the junction belongs to the left/right split.
    assert_eq!(
        grabbed(&splits, (49, 30)),
        Some(PaneSurfaceSplitDirection::Horizontal)
    );
}
