use crate::endpoint::{ClientEndpointId, ClientEndpointStatus};
use crate::shell::endpoints::ClientEndpointFocusTarget;
use crate::shell::ledger::DropReason;
use crate::shell::overlays::text_editor::TextEditor;
use crate::shell::presentation::render;
use crate::shell::state::{
    ClientChromeDrag, ClientCopyOperation, ClientCopySelection, ClientNavigatorFilter,
    ClientNavigatorTarget, ClientShellAction, ClientShellConfig, ClientShellEndpointError,
    ClientShellInput, ClientShellMode, ClientShellOverlay,
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use shepr_config::ClientConfig;
use shepr_config::theme::Palette;
use shepr_protocol::command::{EndpointCommand, EndpointReply};
use shepr_protocol::{AgentStatus, ClientMessage, ClientPaneInputEvent, FrameData};
use shepr_protocol::{ClientShellAgent, ClientShellPane, ClientShellSnapshot, SurfaceRect};
use shepr_termio::host_term::theme::DefaultColorKind;
use shepr_termio::host_term::theme::HostAppearance;
use shepr_termio::input::raw_input::RawInputEvent;

use crate::shell::state::{
    ClientCopyModeState, ClientCopySearch, ClientCopySearchPrompt, ClientHelpOverlay,
    ClientNavigatorOverlay, ClientShellState,
};

use crossterm::event::MouseEvent;

use crate::shell::tests::{
    cell_bg, cell_fg, cell_is_bold, cell_symbol_position, frame_rows, snapshot, surface,
};

use crate::shell::tests::{copy_search_result, pane_scroll_result};
use crate::tests::{test_pane_id, test_workspace_id};

#[test]
fn pasted_help_and_copy_queries_normalize_single_line_text() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
        query: TextEditor::default(),
        search_focused: true,
        scroll: 0,
    }));

    assert!(state.insert_overlay_text("work\nspace"));
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Help(ClientHelpOverlay { ref query, .. }))
            if query.as_str() == "work space"
    ));

    state.overlay = None;
    state.mode = ClientShellMode::Copy;
    state.copy_mode = Some(ClientCopyModeState {
        pane_id: test_pane_id("w1:p1"),
        geometry: (80, 24),
        alternate_screen_active: false,
        cursor: shepr_protocol::command::PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 0,
        },
        history_origin: shepr_vt::AbsRow(0),
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        entry_offset_from_bottom: 0,
        selection: None,
        search: Some(ClientCopySearch {
            prompt: Some(ClientCopySearchPrompt {
                direction: shepr_protocol::command::PaneCopySearchDirection::Forward,
                query: TextEditor::default(),
            }),
            ..Default::default()
        }),
        operation_generation: 0,
    });

    assert!(state.insert_copy_search_text("needle\r\n"));
    assert_eq!(
        state
            .copy_mode
            .as_ref()
            .and_then(|copy_mode| copy_mode.search.as_ref())
            .and_then(|search| search.prompt.as_ref())
            .map(|prompt| prompt.query.as_str()),
        Some("needle ")
    );
}

#[test]
fn copy_cursor_is_never_left_under_the_mode_bar() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let area = state.layout(106, 20).pane_surface;
    let mut pane_surface = surface();
    let lines = (0..area.height)
        .map(|row| format!("{row:<width$}", width = usize::from(area.width)))
        .collect::<Vec<_>>();
    pane_surface.frame =
        FrameData::from_ratatui_buffer_with_hyperlinks(&Buffer::with_lines(lines), None, &[]);
    let rect = SurfaceRect {
        x: 0,
        y: 0,
        width: area.width,
        height: area.height,
    };
    pane_surface.panes[0].rect = rect;
    pane_surface.panes[0].inner_rect = rect;
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 50,
        viewport_rows: u64::from(area.height),
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("terminal frame");
    let mut outcome = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut outcome));

    // The cursor starts on the last line of history, which no scroll can lift above the
    // bar's usual row: the bar moves to the top row instead.
    let accent = shepr_protocol::WireColor::from_ratatui(state.config.palette.accent);
    let bottom = area.bottom() - 1;
    let frame = state.compose(106, 20).expect("copy frame");
    let rows = frame_rows(&frame);
    assert!(rows[usize::from(area.y)].contains("COPY"));
    assert!(!rows[usize::from(bottom)].contains("COPY"));
    assert_eq!(
        frame.cells[usize::from(bottom) * 106 + usize::from(area.x)].bg,
        accent
    );

    // One row up the bar is back at the bottom, clear of the cursor.
    state.handle_input_bytes(b"k");
    let frame = state.compose(106, 20).expect("copy frame");
    let rows = frame_rows(&frame);
    assert!(rows[usize::from(bottom)].contains("COPY"));
    assert_eq!(
        frame.cells[usize::from(bottom - 1) * 106 + usize::from(area.x)].bg,
        accent
    );

    // Scrolled back, a motion onto the covered row scrolls one line instead of hiding the
    // cursor under the bar.
    let height = u64::from(area.height);
    if let Some(copy_mode) = state.copy_mode.as_mut() {
        copy_mode.offset_from_bottom = 10;
        copy_mode.cursor.row = shepr_vt::AbsRow(40 + height - 2);
    }
    state.handle_input_bytes(b"j");
    let copy_mode = state.copy_mode.as_ref().expect("still in copy mode");
    assert_eq!(copy_mode.cursor.row, shepr_vt::AbsRow(40 + height - 1));
    assert_eq!(copy_mode.offset_from_bottom, 9);
}

#[test]
fn client_selection_uses_host_background_and_repaints_when_it_changes() {
    use ratatui::style::Color;
    use shepr_termio::host_term::theme::RgbColor;

    for explicit_appearance in [false, true] {
        let mut config = ClientShellConfig::from_config(&ClientConfig::default());
        config.palette = Palette::terminal();
        let mut state = ClientShellState::new(config);
        state.set_snapshot(Box::new(snapshot()));
        state.receive_pane_surface(surface());
        state.compose(106, 20).expect("composed frame");
        let pane = state.hits.panes[0].clone();
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
            fallback.cells[cell_index].bg,
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
                    .any(|request| matches!(request, ClientMessage::ClientShellHostTheme { .. }))
            );
            let frame = state.compose(106, 20).expect("host-colored selection");
            let cell = &frame.cells[cell_index];
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
    state.receive_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();

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
                EndpointCommand::PaneFocus(target) if target.pane_id == "w1:p1"
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
            .is_some_and(shepr_vt::selection::Selection::is_visible)
    );
    let selected = state.compose(106, 20).expect("selected frame");
    let selected_cell =
        &selected.cells[usize::from(pane.inner_rect.y) * 106 + usize::from(pane.inner_rect.x)];
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
            if params.pane_id == "w1:p1"
                && params.anchor == shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 0,
                }
                && params.cursor == shepr_protocol::command::PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
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
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.config.copy_on_select = false;
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();
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
        updated.panes[0].content_revision += 1;
        updated.frame.cells[0].symbol = "x".into();
        state.receive_pane_surface(updated);
    }
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_vt::selection::Selection::is_finalized)
    );

    // A patch that redraws selected text must retain the same live terminal range.
    let mut updated = state.pane_surface().cloned().expect("pane surface");
    updated.panes[0].content_revision += 1;
    let mut cell = updated.frame.cells[0].clone();
    cell.symbol = "y".into();
    assert!(matches!(
        state.apply_pane_surface_patch(&shepr_protocol::PaneSurfacePatch {
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
            cursor: updated.frame.cursor,
        }),
        crate::shell::presentation::surface_patch::ClientPaneSurfacePatchOutcome::Applied(_)
    ));
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_vt::selection::Selection::is_finalized)
    );

    let highlighted = state.compose(106, 20).expect("highlighted frame");
    let cell_index = usize::from(pane.inner_rect.y) * 106 + usize::from(pane.inner_rect.x);
    let selected_cell = highlighted.cells[cell_index].clone();
    let selection = state.mouse_selection.selection.take();
    let unselected = state.compose(106, 20).expect("unselected frame");
    assert_ne!(selected_cell.bg, unselected.cells[cell_index].bg);
    state.mouse_selection.selection = selection;

    let copy = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
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
    );
    pane_surface.panes[0].rect.y = 1;
    pane_surface.panes[0].inner_rect.y = 1;
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 20,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let pane = state.hits.panes[0].clone();
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

#[test]
fn keyboard_copy_mode_owns_cursor_selection_copy_and_scroll_restore() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.config.copy_on_select = false;
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
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert_eq!(
        state.copy_mode.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_vt::AbsRow(21))
    );
    assert!(enter.actions.is_empty());

    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
    )]);
    assert_eq!(state.mode, ClientShellMode::Prefix);
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode, ClientShellMode::Copy);

    let page = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::PageUp, KeyModifiers::empty()),
    )]);
    assert_eq!(
        state.copy_mode.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_vt::AbsRow(20))
    );
    assert!(matches!(
        &page.actions[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.command,
                EndpointCommand::PaneScroll(params)
                    if params.offset_from_bottom == 1
            )
    ));
    let page_request_id = match &page.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };

    let top = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('g'), KeyModifiers::empty()),
    )]);
    assert!(top.actions.is_empty());
    assert_eq!(
        state.copy_mode.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_vt::AbsRow(0))
    );
    let (_, top_actions) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &page_request_id,
            Ok(pane_scroll_result(1, 20, 2)),
        )
        .into_parts();
    let [ClientShellAction::Endpoint { request, .. }] = &top_actions[..] else {
        panic!("latest queued scroll should follow the completed request");
    };
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneScroll(params)
            if params.pane_id == "w1:p1" && params.offset_from_bottom == 20
    ));
    let top_request_id = request.id.clone();
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &top_request_id,
        Ok(pane_scroll_result(20, 20, 2)),
    );

    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::empty()),
    )]);
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('l'), KeyModifiers::empty()),
    )]);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_vt::selection::Selection::is_visible)
    );

    let copy = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(state.copy_mode.is_none());
    assert!(state.mouse_selection.selection.is_none());
    assert_eq!(copy.actions.len(), 2);
    assert!(copy.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(&request.command, EndpointCommand::PaneSelectionRead(_))
    )));
    assert!(copy.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(
                &request.command,
                EndpointCommand::PaneScroll(params)
                    if params.offset_from_bottom == 0
            )
    )));
}

#[test]
fn keyboard_selections_survive_output_and_copy_live_ranges() {
    // Character and linewise selections have distinct anchor/range projections.
    for selection_key in [b"v", b"V"] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows: 2,
            history_origin: shepr_vt::AbsRow(0),
        });
        state.receive_pane_surface(pane_surface.clone());
        state.compose(106, 20).expect("composed frame");
        state.handle_input_bytes(b"\x02[");
        state.handle_input_bytes(selection_key);
        state.handle_input_bytes(b"k");
        let range = state
            .mouse_selection
            .selection
            .as_ref()
            .expect("selected range")
            .ordered_cells();

        pane_surface.surface_revision = pane_surface
            .surface_revision
            .checked_next()
            .expect("test precondition");
        pane_surface.panes[0].content_revision += 2;
        pane_surface.frame.cells[0].symbol = "X".into();
        state.receive_pane_surface(pane_surface);
        assert_eq!(state.mode, ClientShellMode::Copy);
        assert!(
            state
                .copy_mode
                .as_ref()
                .expect("test precondition")
                .selection
                .is_some()
        );
        assert_eq!(
            state
                .mouse_selection
                .selection
                .as_ref()
                .expect("retained range")
                .ordered_cells(),
            range
        );

        // A linewise selection is requested across the pane's full width; a character
        // selection is requested as its own range.
        let expected = if selection_key == b"V" {
            let width = state.copy_hit().expect("copy hit").inner_rect.width;
            ((range.0.0, 0), (range.1.0, width.saturating_sub(1)))
        } else {
            range
        };
        let copied = state.handle_input_bytes(b"y");
        assert!(copied.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
                    if (params.anchor.row, params.anchor.col) == expected.0
                        && (params.cursor.row, params.cursor.col) == expected.1)
        )));
        assert_eq!(state.mode, ClientShellMode::Terminal);
        assert!(state.mouse_selection.selection.is_none());
        assert!(state.copy_mode.is_none());
    }
}

#[test]
fn empty_keyboard_anchor_keeps_search_fallback_revision_guard() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    state.handle_input_bytes(b"\x02[");
    let search = state.handle_input_bytes(b"/LIVE\r");
    let [ClientShellAction::Endpoint { request, .. }] = &search.actions[..] else {
        panic!("search request");
    };
    let found = shepr_protocol::command::PaneTextRange {
        start: shepr_protocol::command::PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 0,
        },
        end: shepr_protocol::command::PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 3,
        },
    };
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &request.id,
        Ok(copy_search_result(vec![found], Some(0))),
    );
    state.handle_input_bytes(b"v");
    assert!(
        !state
            .mouse_selection
            .selection
            .as_ref()
            .expect("test precondition")
            .is_visible()
    );
    let copy = state.handle_input_bytes(b"y");
    assert!(copy.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
                if params.anchor == found.start
                    && params.cursor == found.end)
    )));
}

#[test]
fn keyboard_selection_does_not_return_after_resize_or_screen_switch() {
    for screen_switch in [false, true] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows: 2,
            history_origin: shepr_vt::AbsRow(0),
        });
        state.receive_pane_surface(pane_surface.clone());
        state.compose(106, 20).expect("composed frame");
        state.handle_input_bytes(b"\x02[");
        state.handle_input_bytes(b"vk");
        assert!(state.mouse_selection.selection.is_some());
        pane_surface.surface_revision = pane_surface
            .surface_revision
            .checked_next()
            .expect("test precondition");
        pane_surface.panes[0].content_revision += 2;
        if screen_switch {
            pane_surface.panes[0].alternate_screen_active = true;
        } else {
            pane_surface.panes[0].inner_rect.width -= 1;
        }
        state.receive_pane_surface(pane_surface);
        assert!(state.mouse_selection.selection.is_none());
        assert!(
            state
                .copy_mode
                .as_ref()
                .expect("test precondition")
                .selection
                .is_none()
        );
        state.compose(106, 20).expect("changed frame");
        state.handle_input_bytes(b"l");
        assert!(
            state.mouse_selection.selection.is_none(),
            "movement must not resurrect the old anchor"
        );
    }
}

#[test]
fn keyboard_copy_mode_content_motion_is_endpoint_backed() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy_mode.as_ref().expect("copy mode").cursor;

    let motion = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('w'), KeyModifiers::empty()),
    )]);
    let [ClientShellAction::Endpoint { request, .. }] = &motion.actions[..] else {
        panic!("word motion should use endpoint semantics");
    };
    let request_id = request.id.clone();
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneCopyMotion(params)
            if params.cursor == origin
                && params.motion
                    == shepr_protocol::command::PaneCopyMotion::Word(
                        shepr_protocol::command::PaneWordMotion::NextStart,
                    )
    ));
    let (repaint, actions) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(EndpointReply::PaneCopyMotion {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: origin.row,
                    col: 3,
                },
            }),
        )
        .into_parts();
    assert!(repaint);
    assert!(actions.is_empty());
    assert_eq!(
        state.copy_mode.as_ref().map(|mode| mode.cursor.col),
        Some(3)
    );
}

#[test]
fn keys_after_an_exit_key_reach_the_pane_once_an_in_flight_copy_motion_replays() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy_mode.as_ref().expect("copy mode").cursor;
    let key = |code| {
        RawInputEvent::Key(shepr_termio::input::TerminalKey::new(
            code,
            KeyModifiers::empty(),
        ))
    };

    let motion = state.handle_raw_events(vec![key(KeyCode::Char('w'))]);
    let [ClientShellAction::Endpoint { request, .. }] = &motion.actions[..] else {
        panic!("word motion should use endpoint semantics");
    };
    let request_id = request.id.clone();
    // `q` waits behind the motion like every key typed in copy mode, so it
    // cannot run ahead of input typed before it, and `x` waits behind `q`.
    let typed = state.handle_raw_events(vec![key(KeyCode::Char('q')), key(KeyCode::Char('x'))]);
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert!(typed.requests.is_empty());

    // The reply applies, then `q` leaves copy mode and `x`, typed after the
    // exit, reaches the pane.
    let replayed = state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &request_id,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: shepr_test_fixtures::id("w1:p1"),
            cursor: shepr_protocol::command::PaneTextPoint {
                row: origin.row,
                col: 3,
            },
        }),
    );
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(state.copy_mode.is_none());
    assert!(state.copy_pipeline.keys_is_empty());
    assert!(
        replayed.requests.iter().any(|request| matches!(
            request,
            ClientMessage::ClientShellPaneInput { pane_id, events }
                if pane_id == "w1:p1"
                    && events.iter().any(|event| matches!(
                        event,
                        ClientPaneInputEvent::Key {
                            code: shepr_protocol::ClientKeyCode::Char('x'),
                            ..
                        }
                    ))
        )),
        "the keystroke after the exit key must reach the pane"
    );
}

#[test]
fn copy_search_owns_prompt_repeat_highlights_selection_and_restore() {
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
    let origin = state.copy_mode.as_ref().expect("copy mode").cursor;

    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
    )]);
    assert!(state.copy_mode.as_ref().is_some_and(|mode| {
        mode.search
            .as_ref()
            .and_then(|search| search.prompt.as_ref())
            .is_some_and(|prompt| {
                prompt.direction == shepr_protocol::command::PaneCopySearchDirection::Backward
            })
    }));
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    )]);
    assert!(state.copy_mode.as_ref().is_none_or(|mode| {
        mode.search
            .as_ref()
            .is_none_or(|search| search.prompt.is_none())
    }));

    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('/'), KeyModifiers::empty()),
    )]);
    state.handle_raw_events(vec![RawInputEvent::Paste("junk".into())]);
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    )]);
    state.handle_raw_events(vec![RawInputEvent::Paste("nee".into())]);
    state.handle_raw_events(vec![RawInputEvent::Paste("dleX".into())]);
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Backspace, KeyModifiers::empty()),
    )]);
    assert_eq!(
        state
            .copy_mode
            .as_ref()
            .and_then(|mode| mode.search.as_ref())
            .and_then(|search| search.prompt.as_ref())
            .map(|prompt| prompt.query.as_str()),
        Some("needle")
    );

    let search = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
    )]);
    let [ClientShellAction::Endpoint { request, .. }] = &search.actions[..] else {
        panic!("search should use endpoint terminal semantics");
    };
    let request_id = request.id.clone();
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneCopySearch(params)
            if params.pane_id == "w1:p1"
                && params.query == "needle"
                && params.direction == shepr_protocol::command::PaneCopySearchDirection::Forward
                && params.cursor == origin
                && params.previous.is_none()
    ));
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
    let (repaint, actions) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(copy_search_result(matches.clone(), Some(0))),
        )
        .into_parts();
    assert!(repaint);
    assert_eq!(
        state.copy_mode.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_vt::AbsRow(5))
    );
    assert!(actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(
                &request.command,
                EndpointCommand::PaneScroll(params)
                    if params.offset_from_bottom == 15
            )
    )));
    let initial_scroll_id = actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.command, EndpointCommand::PaneScroll(_)) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        })
        .expect("initial search scroll");
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &initial_scroll_id,
        Ok(pane_scroll_result(15, 20, 2)),
    );
    let mut scrolled_surface = state.pane_surface().cloned().expect("pane surface");
    scrolled_surface.panes[0]
        .scroll
        .as_mut()
        .expect("scroll metrics")
        .offset_from_bottom = 15;
    state.receive_pane_surface(scrolled_surface);
    let frame = state.compose(106, 20).expect("search frame");
    let hit = state.hits.panes[0].clone();
    let viewport_top = 5u16;
    assert_eq!(
        cell_bg(
            &frame,
            (hit.inner_rect.x + 2, hit.inner_rect.y + (5 - viewport_top))
        ),
        state.config.palette.accent
    );

    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::empty()),
    )]);
    let repeat = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('n'), KeyModifiers::empty()),
    )]);
    let [ClientShellAction::Endpoint { request, .. }] = &repeat.actions[..] else {
        panic!("repeat should use endpoint search");
    };
    let repeat_id = request.id.clone();
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneCopySearch(params)
            if params.direction == shepr_protocol::command::PaneCopySearchDirection::Forward
                && params.previous == Some(matches[0])
    ));
    let (_, repeat_actions) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &repeat_id,
            Ok(copy_search_result(matches.clone(), Some(1))),
        )
        .into_parts();
    if let Some(scroll_id) = repeat_actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint { request, .. }
            if matches!(request.command, EndpointCommand::PaneScroll(_)) =>
        {
            Some(request.id.clone())
        }
        _ => None,
    }) {
        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &scroll_id,
            Ok(pane_scroll_result(6, 20, 2)),
        );
    }
    assert_eq!(
        state.copy_mode.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_vt::AbsRow(15))
    );
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_vt::selection::Selection::is_visible)
    );

    let reverse = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
    )]);
    let [ClientShellAction::Endpoint { request, .. }] = &reverse.actions[..] else {
        panic!("reverse search should use endpoint search");
    };
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneCopySearch(params)
            if params.direction == shepr_protocol::command::PaneCopySearchDirection::Backward
                && params.previous == Some(matches[1])
    ));
    let (_, reverse_actions) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request.id,
            Ok(copy_search_result(matches.clone(), Some(0))),
        )
        .into_parts();
    if let Some(scroll_id) = reverse_actions.iter().find_map(|action| match action {
        ClientShellAction::Endpoint { request, .. }
            if matches!(request.command, EndpointCommand::PaneScroll(_)) =>
        {
            Some(request.id.clone())
        }
        _ => None,
    }) {
        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &scroll_id,
            Ok(pane_scroll_result(15, 20, 2)),
        );
    }

    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert!(state.copy_mode.as_ref().is_some_and(|mode| {
        mode.search
            .as_ref()
            .is_none_or(|search| search.query.is_empty())
            && mode.selection.is_none()
    }));
    let exit = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(exit.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(
                &request.command,
                EndpointCommand::PaneScroll(params)
                    if params.offset_from_bottom == 0
            )
    )));
}

#[test]
fn navigator_workspace_headings_use_the_active_themes_primary_text() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.open_navigator_overlay();
    for palette in [
        Palette::catppuccin(),
        Palette::catppuccin_latte(),
        Palette::terminal(),
    ] {
        state.config.palette = palette;
        let frame = state.compose(106, 30).expect("navigator");
        let (rect, _) = state
            .hits
            .navigator_rows
            .iter()
            .find(|(_, target)| matches!(target, ClientNavigatorTarget::Workspace { .. }))
            .expect("workspace heading");
        let position = cell_symbol_position(&frame, *rect, "client-shell");
        assert_eq!(cell_fg(&frame, position), state.config.palette.text);
        assert!(cell_is_bold(&frame, position));
    }
}

#[test]
fn navigator_renders_every_terminal_in_workspace_sections() {
    let mut snapshot = snapshot();
    snapshot.focused_pane_id = None;
    snapshot.panes[0].label = Some("agent".into());
    let mut shell = snapshot.panes[0].clone();
    shell.pane_id = test_pane_id("w1:p2");
    shell.label = Some("shell".into());
    snapshot.panes.push(shell);
    for label in ["notes", "logs"] {
        let mut pane = snapshot.panes[0].clone();
        pane.pane_id = shepr_protocol::PublicPaneId::new(
            &crate::tests::test_workspace_id("w1"),
            snapshot.panes.len() + 1,
        );
        pane.label = Some(label.into());
        snapshot.panes.push(pane);
    }
    let mut workspace = snapshot.workspaces[0].clone();
    workspace.workspace_id = test_workspace_id("w2");
    workspace.label = "second".into();
    workspace.number = 2;
    let mut pane = snapshot.panes[0].clone();
    pane.pane_id = test_pane_id("w2:p1");
    snapshot.workspaces.push(workspace);
    snapshot.panes.push(pane);
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot));
    state.receive_pane_surface(surface());
    state.open_navigator_overlay();
    let visible_rows = |state: &mut ClientShellState, height| {
        let frame = state.compose(106, height).expect("navigator frame");
        state
            .hits
            .navigator_rows
            .iter()
            .map(|(rect, _)| {
                frame.cells[rect.y as usize * frame.width as usize + rect.x as usize..]
                    .iter()
                    .take(rect.width as usize)
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    let visible = visible_rows(&mut state, 30);
    assert_eq!(visible.len(), 7);
    for (row, prefix) in visible
        .iter()
        .zip([" client", " ├─ ", " ├─ ", " ├─ ", " └─ ", " second", " └─ "])
    {
        assert!(
            row.starts_with(prefix),
            "{row:?} should start with {prefix:?}"
        );
    }
    for (row, label) in visible.iter().zip([
        "client-shell",
        "agent · 1",
        "shell · 2",
        "notes · 3",
        "logs · 4",
        "second",
        "agent",
    ]) {
        assert!(row.contains(label), "{row:?} should contain {label}");
        assert!(!row.contains("/repo"));
        assert!(!row.contains("──"));
    }

    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("expected navigator");
    };
    navigator.scroll = 2;
    navigator.selected = Some(ClientNavigatorTarget::Pane {
        endpoint_id: state.active_endpoint_id.clone(),
        pane_id: test_pane_id("w1:p2"),
    });
    let visible = visible_rows(&mut state, 11);
    assert_eq!(visible.len(), 2);
    assert!(visible.iter().any(|row| row.contains("shell · 2")));
    assert!(visible[0].starts_with(" ├─ "));
    assert!(visible[1].starts_with(" ├─ "));

    // Filtering by the pane's id retains the section and the exact split destination.
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("expected navigator");
    };
    navigator.query = "w1:p2".into();
    navigator.scroll = 0;
    let visible = visible_rows(&mut state, 30);
    assert_eq!(visible.len(), 2);
    assert!(visible[1].starts_with(" └─ "));
    assert!(visible.iter().any(|row| row.contains("shell · 2")));
    assert!(visible.iter().all(|row| !row.contains("second")));
}

#[test]
fn navigator_search_matches_non_adjacent_words_without_losing_the_pane_target() {
    let mut projected = snapshot();
    projected.panes[0].label = Some("alpha beta gamma".into());
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface(surface());
    state.open_navigator_overlay();
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    for (query, matches) in [
        ("alpha gamma", true),
        ("  ALP\tGAM  ", true),
        ("gamma alpha", true),
        ("beta gamma", true),
        ("alpha missing", false),
        ("alphagamma", false),
    ] {
        navigator.query = query.into();
        navigator.selected = None;
        let rows =
            render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator);
        let target = crate::shell::navigation::aggregate_navigation::selected_navigator_target(
            &rows, navigator,
        );
        assert_eq!(
            target,
            matches.then(|| ClientNavigatorTarget::Pane {
                endpoint_id: state.active_endpoint_id.clone(),
                pane_id: test_pane_id("w1:p1"),
            }),
            "query={query:?}"
        );
    }
}

#[test]
fn navigator_searches_ancestor_context_and_keeps_split_agents_individually_actionable() {
    let mut projected = snapshot();
    projected.panes[0].pane_id = test_pane_id("w1:p1");
    projected.focused_pane_id = Some(test_pane_id("w1:p1"));
    let mut second = projected.panes[0].clone();
    second.pane_id = test_pane_id("w1:p2");
    second.foreground_cwd = Some("/repo/subproject".into());
    projected.panes.push(second);
    let first_agent = ClientShellAgent {
        pane_id: test_pane_id("w1:p1"),
        agent: Some("pi".into()),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 1,
    };
    let mut second_agent = first_agent.clone();
    second_agent.pane_id = "w1:p2".parse().expect("test precondition");
    second_agent.agent = Some("claude".into());
    second_agent.terminal_title_stripped = Some("checking navigation".into());
    second_agent.agent_status = AgentStatus::Blocked;
    projected.agents = vec![first_agent, second_agent];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface(surface());
    state.open_navigator_overlay();
    for (query, filter, expected) in [
        ("", None, vec!["w1:p1", "w1:p2"]),
        ("client-shell", None, vec!["w1:p1", "w1:p2"]),
        ("main", None, vec!["w1:p1", "w1:p2"]),
        ("claude", None, vec!["w1:p2"]),
        ("checking navigation", None, vec!["w1:p2"]),
        ("/repo/subproject", None, vec!["w1:p2"]),
        (
            "client-shell",
            Some(ClientNavigatorFilter::Blocked),
            vec!["w1:p2"],
        ),
        ("", Some(ClientNavigatorFilter::Working), vec!["w1:p1"]),
        ("no such agent", None, vec![]),
    ] {
        let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
            panic!("navigator");
        };
        navigator.query = query.into();
        navigator.filter = filter;
        navigator.selected = None;
        let rows =
            render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator);
        let pane_ids = rows
            .iter()
            .filter_map(|row| match &row.target {
                ClientNavigatorTarget::Pane { pane_id, .. } => Some(pane_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(pane_ids, expected, "query={query:?} filter={filter:?}");
        assert_eq!(
            rows.len(),
            if expected.is_empty() {
                0
            } else {
                expected.len() + 1
            }
        );
        if !expected.is_empty() {
            let selected =
                crate::shell::navigation::aggregate_navigation::navigator_selected_index(
                    &rows, navigator,
                )
                .expect("search destination");
            assert!(matches!(
                rows[selected].target,
                ClientNavigatorTarget::Pane { .. }
            ));
        }
    }
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    navigator.query.clear();
    let frame = state.compose(160, 48).expect("navigator");
    assert_eq!(state.hits.navigator_popup.width, 116);
    let pane_rows = state
        .hits
        .navigator_rows
        .iter()
        .filter(|(_, target)| matches!(target, ClientNavigatorTarget::Pane { .. }))
        .collect::<Vec<_>>();
    assert_eq!(pane_rows.len(), 2);
    for ((rect, _), (name, kind, status)) in pane_rows.iter().zip([
        ("pi · 1", "pi", "working"),
        ("checking navigation", "claude", "blocked"),
    ]) {
        cell_symbol_position(&frame, *rect, name);
        cell_symbol_position(&frame, *rect, kind);
        cell_symbol_position(&frame, *rect, status);
    }
    let rect = pane_rows[1].0;
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.right() - 1,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    // An explicit pick goes through the runtime, which knows whether the
    // endpoint is shown.
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if pane_id == "w1:p2"
    ));
}

#[test]
fn navigator_distinguishes_unnamed_terminals_in_one_workspace() {
    let mut projected = snapshot();
    for number in [2, 3] {
        let mut pane = projected.panes[0].clone();
        pane.pane_id =
            shepr_protocol::PublicPaneId::new(&crate::tests::test_workspace_id("w1"), number);
        projected.panes.push(pane);
    }
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.open_navigator_overlay();
    let Some(ClientShellOverlay::Navigator(navigator)) = &state.overlay else {
        panic!("navigator");
    };
    let rows =
        render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator);
    let labels = rows
        .iter()
        .filter(|row| matches!(row.target, ClientNavigatorTarget::Pane { .. }))
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["terminal · 1", "terminal · 2", "terminal · 3"]);
}

#[test]
fn navigator_keeps_empty_workspaces_searchable_without_status_filters() {
    let mut projected = snapshot();
    projected.panes.clear();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.open_navigator_overlay();
    for (query, filter, expected) in [
        ("", None, true),
        ("client-shell", None, true),
        ("main", None, true),
        ("missing", None, false),
        ("main", Some(ClientNavigatorFilter::Idle), false),
        ("", Some(ClientNavigatorFilter::Working), false),
    ] {
        let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
            panic!("navigator");
        };
        navigator.query = query.into();
        navigator.filter = filter;
        let rows =
            render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator);
        assert_eq!(
            rows.len(),
            usize::from(expected),
            "query={query:?}, filter={filter:?}"
        );
        let target = crate::shell::navigation::aggregate_navigation::selected_navigator_target(
            &rows, navigator,
        );
        assert_eq!(
            target,
            expected.then(|| ClientNavigatorTarget::Workspace {
                endpoint_id: ClientEndpointId::Local,
                workspace_id: shepr_test_fixtures::id("w1"),
            })
        );
    }
}

#[test]
fn navigator_horizontal_arrows_jump_sections_but_edit_the_search_cursor() {
    let mut projected = snapshot();
    projected.panes[0].label = Some("needle-first".into());
    let mut sibling = projected.panes[0].clone();
    sibling.pane_id = test_pane_id("w1:p2");
    sibling.label = Some("other".into());
    projected.panes.push(sibling);
    let mut empty = projected.workspaces[0].clone();
    empty.workspace_id = test_workspace_id("w8");
    empty.label = "empty".into();
    projected.workspaces.push(empty);
    let mut last = projected.workspaces[0].clone();
    last.workspace_id = test_workspace_id("w9");
    last.label = "last".into();
    for (id, label) in [("w9:p1", "needle-last"), ("w9:p2", "other-last")] {
        let mut pane = projected.panes[0].clone();
        pane.pane_id = test_pane_id(id);
        pane.label = Some(label.into());
        projected.panes.push(pane);
    }
    projected.workspaces.push(last);
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(projected));
    state.receive_pane_surface(surface());
    state.open_navigator_overlay();
    let press = |state: &mut ClientShellState, code| {
        let outcome = state.handle_raw_events(vec![RawInputEvent::Key(
            shepr_termio::input::TerminalKey::new(code, KeyModifiers::empty()),
        )]);
        assert!(outcome.actions.is_empty());
    };
    let selected = |state: &ClientShellState| {
        let Some(ClientShellOverlay::Navigator(navigator)) = &state.overlay else {
            panic!("navigator");
        };
        navigator.selected.clone()
    };
    let target = |id: &str| {
        Some(ClientNavigatorTarget::Pane {
            endpoint_id: ClientEndpointId::Local,
            pane_id: test_pane_id(id),
        })
    };
    press(&mut state, KeyCode::Left);
    assert_eq!(selected(&state), target("w1:p1"));
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), target("w9:p1"));
    press(&mut state, KeyCode::Down);
    assert_eq!(selected(&state), target("w9:p2"));
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), target("w9:p2"));
    press(&mut state, KeyCode::Left);
    assert_eq!(selected(&state), target("w1:p1"));
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    navigator.query = "needle".into();
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), target("w9:p1"));
    press(&mut state, KeyCode::Left);
    assert_eq!(selected(&state), target("w1:p1"));
    press(&mut state, KeyCode::Char('/'));
    press(&mut state, KeyCode::Left);
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), target("w1:p1"));
    press(&mut state, KeyCode::Left);
    press(&mut state, KeyCode::Char('X'));
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    assert_eq!(navigator.query.as_str(), "needlXe");
    navigator.search_focused = false;
    navigator.selected = None;
    press(&mut state, KeyCode::Left);
    press(&mut state, KeyCode::Right);
    assert_eq!(selected(&state), None);
}

#[test]
fn navigator_scrollbar_click_and_drag_scroll_without_opening_a_destination() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.open_navigator_overlay();
    state.compose(106, 24).expect("small navigator");
    assert!(state.hits.navigator_scrollbar.is_empty());

    let mut projected = snapshot();
    for index in 2..=60 {
        let mut pane = projected.panes[0].clone();
        pane.pane_id =
            shepr_protocol::PublicPaneId::new(&crate::tests::test_workspace_id("w1"), index);
        pane.label = Some(format!("agent {index}"));
        projected.panes.push(pane);
    }
    state.set_snapshot(Box::new(projected));
    let frame = state.compose(106, 24).expect("overflowing navigator");
    let track = state.hits.navigator_scrollbar;
    let metrics = state.hits.navigator_scroll_metrics.expect("scroll metrics");
    assert!(!track.is_empty());
    assert_eq!(metrics.offset_from_bottom, metrics.max_offset_from_bottom);
    assert!(track.y > state.hits.navigator_search.y);
    assert!(track.bottom() < state.hits.navigator_popup.bottom() - 3);
    assert!(
        state
            .hits
            .navigator_rows
            .iter()
            .all(|(rect, _)| rect.right() == track.x)
    );
    assert_eq!(
        cell_fg(&frame, (track.x, track.y)),
        state.config.palette.overlay1
    );
    assert_eq!(
        cell_fg(&frame, (track.x, track.bottom() - 1)),
        state.config.palette.overlay0
    );
    let mouse = |state: &mut ClientShellState, kind, row| {
        let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind,
            column: track.x,
            row,
            modifiers: KeyModifiers::empty(),
        })]);
        assert!(outcome.actions.is_empty());
        assert!(matches!(
            state.overlay,
            Some(ClientShellOverlay::Navigator(_))
        ));
    };
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        track.bottom() - 1,
    );
    state.compose(106, 24).expect("track jump");
    assert_eq!(
        state
            .hits
            .navigator_scroll_metrics
            .expect("metrics")
            .offset_from_bottom,
        0
    );
    let last_pane = shepr_protocol::PublicPaneId::new(&crate::tests::test_workspace_id("w1"), 60);
    assert!(state.hits.navigator_rows.iter().any(|(_, target)| matches!(target, ClientNavigatorTarget::Pane { pane_id, .. } if *pane_id == last_pane)));
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), track.y);
    state.compose(106, 24).expect("jump back to top");
    assert_eq!(
        state
            .hits
            .navigator_scroll_metrics
            .expect("metrics")
            .offset_from_bottom,
        metrics.max_offset_from_bottom
    );
    let thumb = shepr_termio::scroll::scrollbar_thumb(metrics, track).expect("thumb");
    let grab = thumb.len - 1;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        thumb.top + grab,
    );
    assert!(
        matches!(state.chrome_drag, Some(ClientChromeDrag::NavigatorScrollbar { grab_row_offset }) if grab_row_offset == grab)
    );
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        thumb.top + grab,
    );
    state.compose(106, 24).expect("grab does not move viewport");
    assert_eq!(
        state
            .hits
            .navigator_scroll_metrics
            .expect("metrics")
            .offset_from_bottom,
        metrics.max_offset_from_bottom
    );
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        track.bottom() + 5,
    );
    state.compose(106, 24).expect("drag to bottom");
    assert_eq!(
        state
            .hits
            .navigator_scroll_metrics
            .expect("metrics")
            .offset_from_bottom,
        0
    );
    mouse(
        &mut state,
        MouseEventKind::Up(MouseButton::Left),
        track.bottom() + 5,
    );
    assert!(state.chrome_drag.is_none());
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Up, KeyModifiers::empty()),
    )]);
    state.compose(106, 24).expect("keyboard resumes after drag");
    assert_eq!(
        state
            .hits
            .navigator_scroll_metrics
            .expect("metrics")
            .offset_from_bottom,
        1
    );

    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("navigator");
    };
    // The last pane's id is the only text that matches it alone.
    navigator.query = last_pane.as_str().into();
    navigator.selected = None;
    state.compose(106, 24).expect("filtered navigator");
    assert!(state.hits.navigator_scrollbar.is_empty());
    assert_eq!(state.hits.navigator_rows.len(), 2);
    state.compose(106, 90).expect("tall filtered navigator");
    assert!(state.hits.navigator_scrollbar.is_empty());
}

#[test]
fn navigator_narrow_layout_and_long_search_stay_inside_the_popup() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.open_navigator_overlay();
    for (width, height) in [(24, 12), (50, 24), (106, 30)] {
        let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
            panic!("navigator");
        };
        navigator.search_focused = true;
        navigator.query = "界".repeat(100).as_str().into();
        let frame = state.compose(width, height).expect("navigator frame");
        let popup = state.hits.navigator_popup;
        let cursor = frame.cursor.as_ref().expect("search cursor");
        assert!(crate::shell::input::hit_test::contains(
            popup,
            (cursor.x, cursor.y)
        ));
        assert!(state.hits.navigator_rows.is_empty());
        assert!(popup.right() <= width && popup.bottom() <= height);
    }
}

fn navigator_scale_snapshot(workspaces: usize, panes: usize) -> ClientShellSnapshot {
    let mut result = snapshot();
    let workspace_template = result.workspaces[0].clone();
    let pane_template = result.panes[0].clone();
    result.workspaces.clear();
    result.panes.clear();
    for w in 0..workspaces {
        let mut workspace = workspace_template.clone();
        workspace.workspace_id =
            shepr_protocol::WorkspaceId::from_number(w + 1).expect("one-based workspace number");
        workspace.number = w + 1;
        workspace.label = format!("workspace {w}");
        for p in 0..panes {
            let mut pane = pane_template.clone();
            pane.pane_id = shepr_protocol::PublicPaneId::new(&workspace.workspace_id, p + 1);
            pane.label = Some(format!("terminal {p}"));
            result.panes.push(pane);
        }
        result.workspaces.push(workspace);
    }
    result.focused_workspace_id = Some(result.workspaces[0].workspace_id.clone());
    result.focused_pane_id = Some(result.panes[0].pane_id.clone());
    result
}

#[test]
fn navigator_grouping_keeps_snapshot_order_with_interleaved_panes() {
    let mut snapshot = navigator_scale_snapshot(2, 2);
    snapshot.panes.reverse();
    let expected = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| {
            snapshot
                .panes
                .iter()
                .filter(|pane| pane.pane_id.workspace_id() == &workspace.workspace_id)
                .map(|pane| pane.pane_id.clone())
        })
        .collect::<Vec<_>>();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    let remote = shepr_config::MachineConfig {
        label: shepr_config::MachineLabel::parse("Remote").expect("test precondition"),
        ssh: shepr_config::SshTarget::parse("dev@example.invalid").expect("test precondition"),
    };
    let remote_id = ClientEndpointId::Ssh(remote.label.clone());
    state.set_machines(&[remote]);
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
    state.set_endpoint_snapshot(&remote_id, Box::new(snapshot.clone()));
    state.set_snapshot(Box::new(snapshot));
    state.open_navigator_overlay();
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("navigator")
    };
    let rows =
        render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator);
    let actual = rows
        .iter()
        .filter_map(|row| match &row.target {
            ClientNavigatorTarget::Pane {
                endpoint_id,
                pane_id,
            } => {
                assert!(endpoint_id == &state.active_endpoint_id || endpoint_id == &remote_id);
                Some((endpoint_id.clone(), pane_id.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = [state.active_endpoint_id.clone(), remote_id]
        .into_iter()
        .flat_map(|endpoint| {
            expected
                .iter()
                .map(move |pane| (endpoint.clone(), pane.clone()))
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn navigator_owns_search_mouse_selection_and_stable_target_focus() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let mut open = ClientShellInput::default();
    state.record_binding(
        &shepr_termio::input::KeybindAction::OpenNavigator,
        &mut open,
    );
    let navigator = state.compose(106, 30).expect("navigator overlay");
    let navigator_text = navigator
        .cells
        .chunks(navigator.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(navigator_text.contains("client-shell"));
    assert!(navigator_text.contains("terminal"));
    assert!(!navigator_text.contains("pane 1"));

    let search = state.hits.navigator_search;
    let focus_search =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: search.x,
            row: search.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(focus_search.repaint);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Navigator(ClientNavigatorOverlay {
            search_focused: true,
            ..
        }))
    ));
    assert!(state.handle_input_bytes(b"client").actions.is_empty());
    let filtered = state.compose(106, 30).expect("filtered navigator");
    assert!(
        filtered
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.visible)
    );

    state.handle_input_bytes(b"\x1b");
    state.handle_input_bytes(b"a");
    state.compose(106, 30).expect("navigator rows");
    let pane_target = {
        let ClientShellOverlay::Navigator(navigator) = state.overlay.as_ref().expect("navigator")
        else {
            panic!("expected navigator");
        };
        render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator)
            .iter()
            .find(|row| matches!(row.target, ClientNavigatorTarget::Pane { .. }))
            .map(|row| row.target.clone())
            .expect("pane row")
    };
    let pane_rect = state
        .hits
        .navigator_rows
        .iter()
        .find(|(_, target)| *target == pane_target)
        .map(|(rect, _)| *rect)
        .expect("visible pane row");
    let select =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column: pane_rect.x + 6,
            row: pane_rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(select.repaint);
    let accept =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: pane_rect.x + 6,
            row: pane_rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
    // The pick goes through the runtime, which focuses an endpoint that owns
    // the presentation through the endpoint API.
    let [
        ClientShellAction::ActivateEndpoint {
            endpoint_id: ClientEndpointId::Local,
            target: Some(target),
        },
    ] = &accept.actions[..]
    else {
        panic!("navigator pane click should be an explicit local pick");
    };
    let focus = state.focus_endpoint_target(target.clone());
    let [ClientShellAction::Endpoint { request, .. }] = &focus[..] else {
        panic!("navigator pane click should use endpoint API");
    };
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneFocus(target) if target.pane_id == "w1:p1"
    ));
    assert!(state.overlay.is_none());
}

#[test]
fn copy_mode_survives_mouse_motion_and_parks_across_focus_changes() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.config.copy_on_select = false;
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);

    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert!(state.copy_mode.is_some());

    state.handle_input_bytes(b"v");
    assert!(
        state
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.selection.is_some())
    );

    let mut unfocused = snapshot();
    unfocused.focused_pane_id = Some(test_pane_id("w1:p2"));
    unfocused.panes.push(ClientShellPane {
        pane_id: test_pane_id("w1:p2"),
        label: None,
        cwd: Some("/repo".into()),
        foreground_cwd: Some("/repo".into()),
        right_click_passthrough: false,
    });
    state.set_snapshot(Box::new(unfocused.clone()));
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(
        state
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.selection.is_some())
    );

    let (prefix_key, prefix_modifiers) = state.config.keybinds.prefix;
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(prefix_key, prefix_modifiers),
    )]);
    state.set_snapshot(Box::new(unfocused.clone()));
    assert_eq!(state.mode, ClientShellMode::Prefix);
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode, ClientShellMode::Terminal);

    let mut other_selection = shepr_vt::selection::Selection::range(
        test_pane_id("w1:p2"),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 1),
    );
    assert!(other_selection.finish());
    state.mouse_selection.selection = Some(other_selection);
    state.set_snapshot(Box::new(unfocused));
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.pane_id == "w1:p2")
    );

    let mut other_surface = surface();
    other_surface.panes[0].pane_id = test_pane_id("w1:p2");
    state.receive_pane_surface(other_surface.clone());
    other_surface.surface_revision = other_surface
        .surface_revision
        .checked_next()
        .expect("test precondition");
    other_surface.panes[0].content_revision = 1;
    state.receive_pane_surface(other_surface);
    assert!(state.mouse_selection.selection.is_some());
    let copy = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )]);
    assert!(copy.requests.is_empty());
    assert!(
        matches!(&copy.actions[..], [ClientShellAction::Endpoint { request, .. }]
        if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
            if params.pane_id == "w1:p2"))
    );

    state.set_snapshot(Box::new(snapshot()));
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert!(state.copy_mode.is_some());
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.pane_id == "w1:p1")
    );
    state.handle_raw_events(vec![RawInputEvent::Paste("ignored".into())]);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.pane_id == "w1:p1")
    );

    state.mode = ClientShellMode::Navigate;
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.pane_id == "w1:p1")
    );
    state.mode = ClientShellMode::Resize;
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode, ClientShellMode::Copy);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.pane_id == "w1:p1")
    );
}

#[test]
fn clicking_the_pane_scrollbar_preserves_copy_mode_for_its_focused_pane() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    pane_surface.panes[0].scrollbar_rect = Some(SurfaceRect {
        x: 3,
        y: 0,
        width: 1,
        height: 2,
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));
    let track = state.hits.panes[0]
        .scrollbar_rect
        .expect("pane scrollbar hit");

    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: track.x,
        row: track.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert_eq!(state.mode, ClientShellMode::Copy);
    assert!(state.copy_mode.is_some());
}

#[test]
fn retained_selection_copy_suppresses_key_repeats() {
    let mut config = ClientConfig::default();
    config.ui.copy_on_select = false;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    let mut selection = shepr_vt::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
        shepr_vt::Point::new(shepr_vt::AbsRow(0), 1),
    );
    assert!(selection.finish());
    state.mouse_selection.selection = Some(selection);

    let key = shepr_termio::input::TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let press = state.handle_raw_events(vec![RawInputEvent::Key(key.clone())]);
    assert!(press.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(request.command, EndpointCommand::PaneSelectionRead(_))
    )));
    let repeat = state.handle_raw_events(vec![RawInputEvent::Key(
        key.clone()
            .with_kind(crossterm::event::KeyEventKind::Repeat),
    )]);
    assert!(repeat.actions.is_empty());
    assert!(repeat.requests.is_empty());
    let release = state.handle_raw_events(vec![RawInputEvent::Key(
        key.with_kind(crossterm::event::KeyEventKind::Release),
    )]);
    assert!(release.actions.is_empty());
    assert!(release.requests.is_empty());
}

#[test]
fn rapid_copy_motions_are_chained_from_the_previous_result() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy_mode.as_ref().expect("copy mode").cursor;

    let first = state.handle_input_bytes(b"w");
    let second = state.handle_input_bytes(b"w");
    assert_eq!(first.actions.len(), 1);
    assert!(second.actions.is_empty());
    let first_id = match &first.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };
    let intermediate = shepr_protocol::command::PaneTextPoint {
        row: origin.row,
        col: 2,
    };
    let (_, follow_up) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &first_id,
            Ok(EndpointReply::PaneCopyMotion {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                cursor: intermediate,
            }),
        )
        .into_parts();
    assert!(matches!(
        &follow_up[..],
        [ClientShellAction::Endpoint { request, .. }]
            if matches!(
                &request.command,
                EndpointCommand::PaneCopyMotion(params)
                    if params.cursor == intermediate
            )
    ));
}

#[test]
fn copy_prefix_and_detach_act_after_an_in_flight_copy_operation_replays() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy_mode.as_ref().expect("copy mode").cursor;
    let motion = state.handle_input_bytes(b"w");
    state.handle_input_bytes(b"l");
    let (prefix_key, prefix_modifiers) = state.config.keybinds.prefix;
    state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_termio::input::TerminalKey::new(prefix_key, prefix_modifiers),
    )]);
    // The prefix waits behind the motion and the key typed before it.
    assert_eq!(state.mode, ClientShellMode::Copy);

    let detach = state.handle_input_bytes(b"q");
    assert!(!detach.detach);
    assert_eq!(state.copy_pipeline.keys_len(), 3);

    let motion_id = match &motion.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };
    let replayed = state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &motion_id,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: shepr_test_fixtures::id("w1:p1"),
            cursor: origin,
        }),
    );
    // Replay runs `l`, then the prefix, then `q` as the prefix's detach.
    assert!(replayed.detach);
    assert!(state.copy_pipeline.keys_is_empty());
}

#[test]
fn copy_mode_exit_keys_act_after_earlier_queued_input() {
    for key in [
        shepr_termio::input::TerminalKey::new(KeyCode::Char('q'), KeyModifiers::empty()),
        shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    ] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 10,
            viewport_rows: 2,
            history_origin: shepr_vt::AbsRow(0),
        });
        state.receive_pane_surface(pane_surface);
        state.compose(106, 20).expect("composed frame");
        let mut enter = ClientShellInput::default();
        assert!(state.enter_copy_mode(&mut enter));
        let origin = state.copy_mode.as_ref().expect("copy mode").cursor;
        let motion = state.handle_input_bytes(b"w");
        let motion_id = match &motion.actions[0] {
            ClientShellAction::Endpoint { request, .. } => request.id.clone(),
            _ => unreachable!(),
        };
        state.handle_input_bytes(b"l");

        state.handle_raw_events(vec![RawInputEvent::Key(key)]);

        // The exit key queues behind `l` and the motion it follows.
        assert_eq!(state.mode, ClientShellMode::Copy);
        assert!(state.copy_pipeline.in_flight());
        assert_eq!(state.copy_pipeline.keys_len(), 2);

        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &motion_id,
            Ok(EndpointReply::PaneCopyMotion {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                cursor: origin,
            }),
        );

        assert_eq!(state.mode, ClientShellMode::Terminal);
        assert!(state.copy_mode.is_none());
        assert!(!state.copy_pipeline.in_flight());
        assert!(state.copy_pipeline.keys_is_empty());
    }
}

#[test]
fn an_interrupt_key_leaves_copy_mode_behind_a_full_queue_and_the_late_reply_is_ignored() {
    for exit_with_escape in [true, false] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 10,
            viewport_rows: 2,
            history_origin: shepr_vt::AbsRow(0),
        });
        state.receive_pane_surface(pane_surface);
        state.compose(106, 20).expect("composed frame");
        let mut enter = ClientShellInput::default();
        assert!(state.enter_copy_mode(&mut enter));
        let origin = state.copy_mode.as_ref().expect("copy mode").cursor;
        let motion = state.handle_input_bytes(b"w");
        let motion_id = match &motion.actions[0] {
            ClientShellAction::Endpoint { request, .. } => request.id.clone(),
            _ => unreachable!(),
        };
        for _ in 0..crate::limits::MAX_COPY_INPUT_QUEUE {
            state.handle_input_bytes(b"j");
        }
        assert_eq!(
            state.copy_pipeline.keys_len(),
            crate::limits::MAX_COPY_INPUT_QUEUE
        );

        let key = if exit_with_escape {
            shepr_termio::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty())
        } else {
            let (prefix_key, prefix_modifiers) = state.config.keybinds.prefix;
            shepr_termio::input::TerminalKey::new(prefix_key, prefix_modifiers)
        };
        state.handle_raw_events(vec![RawInputEvent::Key(key)]);

        // The request that stopped answering and the keys behind it are given up.
        assert!(!state.copy_pipeline.in_flight());
        assert!(state.copy_pipeline.keys_is_empty());
        if exit_with_escape {
            assert_eq!(state.mode, ClientShellMode::Terminal);
            assert!(state.copy_mode.is_none());
        } else {
            assert_eq!(state.mode, ClientShellMode::Prefix);
        }
        let cursor_before = state.copy_mode.as_ref().map(|copy_mode| copy_mode.cursor);

        let late = state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &motion_id,
            Ok(EndpointReply::PaneCopyMotion {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: origin.row,
                    col: origin.col.saturating_add(3),
                },
            }),
        );
        assert!(late.actions.is_empty());
        assert!(late.requests.is_empty());
        assert_eq!(
            state.copy_mode.as_ref().map(|copy_mode| copy_mode.cursor),
            cursor_before
        );
        assert!(state.copy_pipeline.keys_is_empty());
    }
}

#[test]
fn failed_copy_operation_replays_keys_while_the_copy_pane_still_owns_input() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));
    let origin = state.copy_mode.as_ref().expect("copy mode").cursor;

    let motion = state.handle_input_bytes(b"w");
    state.handle_input_bytes(b"l");
    let request_id = match &motion.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &request_id,
        Err(ClientShellEndpointError::Timeout),
    );

    assert_eq!(
        state
            .copy_mode
            .as_ref()
            .map(|copy_mode| copy_mode.cursor.col),
        Some(origin.col.saturating_add(1))
    );
    assert!(state.copy_pipeline.keys_is_empty());
}

#[test]
fn deferred_copy_input_is_bounded() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));
    state.handle_input_bytes(b"w");

    for _ in 0..crate::limits::MAX_COPY_INPUT_QUEUE + 8 {
        state.handle_input_bytes(b"j");
    }

    assert_eq!(
        state.copy_pipeline.keys_len(),
        crate::limits::MAX_COPY_INPUT_QUEUE
    );
    assert!(state.endpoint_error.message().is_some());
}

#[test]
fn cancelled_copy_requests_discard_dependent_input_without_starting_work() {
    for unsent in [false, true] {
        for operation in [b"w".as_slice(), b"/LIVE\r".as_slice()] {
            for queued in [b"w".as_slice(), b"yx".as_slice(), b"\rx".as_slice()] {
                let mut state =
                    ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
                state.set_snapshot(Box::new(snapshot()));
                let mut pane_surface = surface();
                pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
                    offset_from_bottom: 0,
                    max_offset_from_bottom: 10,
                    viewport_rows: 2,
                    history_origin: shepr_vt::AbsRow(0),
                });
                state.receive_pane_surface(pane_surface);
                state.compose(106, 20).expect("composed frame");
                let mut enter = ClientShellInput::default();
                assert!(state.enter_copy_mode(&mut enter));
                let operation = state.handle_input_bytes(operation);
                let [ClientShellAction::Endpoint { request, .. }] = &operation.actions[..] else {
                    panic!("expected one copy request");
                };
                let request_id = request.id.clone();
                let buffered = state.handle_input_bytes(queued);
                assert!(buffered.actions.is_empty());
                assert!(buffered.requests.is_empty());
                assert!(!state.copy_pipeline.keys_is_empty());

                assert!(if unsent {
                    state.drop_request(&request_id, DropReason::Unsent)
                } else {
                    state.drop_request(&request_id, DropReason::Interrupted)
                });

                assert!(state.ledger.is_empty());
                assert!(!state.copy_pipeline.in_flight());
                assert!(state.copy_pipeline.ops_is_empty());
                assert!(state.copy_pipeline.keys_is_empty());
                assert!(state.notices.visible().is_none());
                assert!(state.scroll_lanes.is_idle());
                assert_eq!(state.mode, ClientShellMode::Copy);
                assert!(
                    !state
                        .copy_mode
                        .as_ref()
                        .expect("copy mode")
                        .search
                        .as_ref()
                        .is_some_and(|search| search.copy_after_result)
                );
                // Cancellation leaves the copy session usable for newly typed input.
                let next = state.handle_input_bytes(b"w");
                assert!(matches!(
                    next.actions.as_slice(),
                    [ClientShellAction::Endpoint { .. }]
                ));
                assert!(state.copy_pipeline.in_flight());
                assert_eq!(state.ledger.len(), 1);
            }
        }
    }
}

#[test]
fn mismatched_boot_copy_result_rolls_back_the_old_pipeline() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));

    let started = state.handle_input_bytes(b"w");
    let [ClientShellAction::Endpoint { request, .. }] = started.actions.as_slice() else {
        panic!("copy motion should issue one endpoint request");
    };
    let request_id = request.id.clone();
    assert!(state.handle_input_bytes(b"w").actions.is_empty());
    assert!(!state.copy_pipeline.keys_is_empty());

    let outcome = state.answer_request(
        "replacement-boot",
        &request_id,
        Err(ClientShellEndpointError::Server(
            shepr_protocol::command::EndpointError::StaleBoot,
        )),
        std::time::Instant::now(),
    );

    assert!(outcome.repaint);
    assert!(state.ledger.is_empty());
    assert!(!state.copy_pipeline.in_flight());
    assert!(state.copy_pipeline.ops_is_empty());
    assert!(state.copy_pipeline.keys_is_empty());
    assert!(state.notices.visible().is_none());
}

#[test]
fn cancelling_an_old_copy_request_does_not_reset_a_new_session() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));
    let old = state.handle_input_bytes(b"w");
    let [ClientShellAction::Endpoint { request, .. }] = &old.actions[..] else {
        panic!("expected one copy request");
    };
    let old_id = request.id.clone();
    // An exit key only acts ahead of the old request once the queue behind it
    // is full; that abandons the old session with its request still pending.
    for _ in 0..crate::limits::MAX_COPY_INPUT_QUEUE {
        state.handle_input_bytes(b"j");
    }
    state.handle_input_bytes(b"q");
    assert!(state.copy_mode.is_none());
    assert!(state.enter_copy_mode(&mut enter));
    let current = state.handle_input_bytes(b"w");
    let [ClientShellAction::Endpoint { request, .. }] = &current.actions[..] else {
        panic!("expected one copy request");
    };
    let current_id = request.id.clone();
    state.handle_input_bytes(b"l");

    state.drop_request(&old_id, DropReason::Unsent);

    assert!(state.copy_pipeline.is_awaiting(&current_id.clone().into()));
    assert!(state.copy_pipeline.in_flight());
    assert_eq!(state.copy_pipeline.keys_len(), 1);
    assert!(state.ledger.contains(current_id.as_str()));
}

#[test]
fn copy_operation_does_not_capture_input_after_focus_moves() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));
    let motion = state.handle_input_bytes(b"w");
    let request_id = match &motion.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };
    state.handle_input_bytes(b"l");

    let mut unfocused = snapshot();
    unfocused.focused_pane_id = Some(test_pane_id("w1:p2"));
    unfocused.panes.push(ClientShellPane {
        pane_id: test_pane_id("w1:p2"),
        label: None,
        cwd: Some("/repo".into()),
        foreground_cwd: Some("/repo".into()),
        right_click_passthrough: false,
    });
    state.set_snapshot(Box::new(unfocused));
    let input = state.handle_input_bytes(b"x");

    assert!(input.requests.iter().any(|request| matches!(
        request,
        ClientMessage::ClientShellPaneInput { pane_id, .. } if pane_id == "w1:p2"
    )));

    let failed = state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &request_id,
        Err(ClientShellEndpointError::Timeout),
    );
    assert!(
        !failed
            .requests
            .iter()
            .any(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. }))
    );
    assert!(state.copy_pipeline.keys_is_empty());
}

#[test]
fn reentering_copy_mode_on_the_same_pane_is_a_no_op() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut first = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut first));
    state
        .copy_mode
        .as_mut()
        .expect("copy mode")
        .offset_from_bottom = 10;
    let mut reenter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut reenter));
    assert!(reenter.actions.is_empty());
    assert_eq!(
        state
            .copy_mode
            .as_ref()
            .map(|copy_mode| copy_mode.entry_offset_from_bottom),
        Some(0)
    );
}

#[test]
fn copy_waits_for_endpoint_motion_before_copying_selection() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    state.handle_input_bytes(b"v");
    let origin = state.copy_mode.as_ref().expect("copy mode").cursor;
    let motion = state.handle_input_bytes(b"w");
    let queued_copy = state.handle_input_bytes(b"y");
    assert!(queued_copy.actions.is_empty());
    let motion_id = match &motion.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };
    let target = shepr_protocol::command::PaneTextPoint {
        row: origin.row,
        col: 2,
    };
    let (_, actions) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &motion_id,
            Ok(EndpointReply::PaneCopyMotion {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                cursor: target,
            }),
        )
        .into_parts();
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(
                &request.command,
                EndpointCommand::PaneSelectionRead(params)
                    if params.anchor == origin && params.cursor == target
            )
    )));
}

/// Search matches name absolute rows, so output leaves the ones still in
/// history in place and drops only those whose rows were evicted; a resize
/// re-wraps the text under them and drops them all.
#[test]
fn copy_search_matches_survive_output_but_not_a_resize() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.receive_pane_surface(pane_surface.clone());
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let copy_mode = state.copy_mode.as_mut().expect("copy mode");
    let search = copy_mode
        .search
        .get_or_insert_with(ClientCopySearch::default);
    search.query = "needle".into();
    let found_on = |row| shepr_protocol::command::PaneTextRange {
        start: shepr_protocol::command::PaneTextPoint {
            row: shepr_vt::AbsRow(row),
            col: 0,
        },
        end: shepr_protocol::command::PaneTextPoint {
            row: shepr_vt::AbsRow(row),
            col: 1,
        },
    };
    search.matches = vec![found_on(1), found_on(6)];
    search.total = 2;
    search.current = Some(1);
    search.current_global = Some(1);
    copy_mode.cursor = shepr_protocol::command::PaneTextPoint {
        row: shepr_vt::AbsRow(2),
        col: 1,
    };
    copy_mode.selection = Some(ClientCopySelection::Character {
        anchor: shepr_vt::Point::new(shepr_vt::AbsRow(1), 2),
    });

    pane_surface.surface_revision = pane_surface
        .surface_revision
        .checked_next()
        .expect("test precondition");
    pane_surface.panes[0].content_revision = 2;
    pane_surface.panes[0]
        .scroll
        .as_mut()
        .expect("scroll metrics")
        .history_origin = shepr_vt::AbsRow(5);
    state.receive_pane_surface(pane_surface.clone());
    let copy_mode = state.copy_mode.as_ref().expect("copy mode retained");
    let search = copy_mode.search.as_ref().expect("search state retained");
    assert_eq!(search.matches, vec![found_on(6)]);
    assert_eq!(search.total, 1);
    assert_eq!(search.current, Some(0));
    assert_eq!(search.current_global, Some(0));
    assert_eq!(copy_mode.history_origin, shepr_vt::AbsRow(5));
    assert_eq!(copy_mode.cursor.row, shepr_vt::AbsRow(5));
    assert!(matches!(
        copy_mode.selection,
        Some(ClientCopySelection::Character { anchor })
            if anchor == shepr_vt::Point::new(shepr_vt::AbsRow(5), 2)
    ));
    assert_eq!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("clamped copy selection")
            .ordered_cells(),
        ((shepr_vt::AbsRow(5), 1), (shepr_vt::AbsRow(5), 2))
    );

    pane_surface.surface_revision = pane_surface
        .surface_revision
        .checked_next()
        .expect("test precondition");
    pane_surface.panes[0].inner_rect.width -= 1;
    state.receive_pane_surface(pane_surface);
    let copy_mode = state.copy_mode.as_ref().expect("copy mode retained");
    let search = copy_mode.search.as_ref().expect("search state retained");
    assert!(search.matches.is_empty());
    assert_eq!(search.total, 0);
    assert_eq!(search.current, None);
}

#[test]
fn word_selection_result_survives_focus_snapshot_lag() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");
    let hit = state.hits.panes[0].clone();
    let mut request = ClientShellInput::default();
    let metrics = shepr_termio::ScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    };
    state.request_word_selection(&hit, metrics, 0, 1, &mut request);
    let request_id = match &request.actions[0] {
        ClientShellAction::Endpoint { request, .. } => request.id.clone(),
        _ => unreachable!(),
    };
    let mut lagging = snapshot();
    lagging.focused_pane_id = None;
    state.set_snapshot(Box::new(lagging));
    let (repaint, _) = state
        .handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(EndpointReply::PaneSelection {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                text: "hello world".into(),
            }),
        )
        .into_parts();
    assert!(repaint);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_vt::selection::Selection::is_visible)
    );
}

#[test]
fn copy_mode_repeat_during_projection_gap_stays_active() {
    for selection_before_gap in [None, Some(true), Some(false)] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
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
        if selection_before_gap == Some(true) {
            state.handle_input_bytes(b"V");
        }
        state.handle_raw_events(vec![RawInputEvent::Key(
            shepr_termio::input::TerminalKey::new(KeyCode::Char('k'), KeyModifiers::empty()),
        )]);

        let mut next = snapshot();
        next.revision = next.revision.checked_next().expect("test precondition");
        state.set_snapshot(Box::new(next));
        // The last composed frame is still on screen, so its hit map stays valid until
        // the matching surface is composed.
        assert!(!state.hits.panes.is_empty());
        assert_eq!(state.mode, ClientShellMode::Copy);
        if selection_before_gap == Some(false) {
            state.handle_input_bytes(b"V");
            assert_eq!(
                state
                    .mouse_selection
                    .selection
                    .as_ref()
                    .expect("linewise selection")
                    .ordered_cells(),
                ((shepr_vt::AbsRow(20), 0), (shepr_vt::AbsRow(20), 0))
            );
            assert_eq!(
                state
                    .mouse_selection
                    .selection
                    .as_ref()
                    .expect("linewise selection")
                    .shape(),
                shepr_vt::selection::SelectionShape::Lines
            );
        }

        let kind = if selection_before_gap == Some(false) {
            crossterm::event::KeyEventKind::Press
        } else {
            crossterm::event::KeyEventKind::Repeat
        };
        let moved = state.handle_raw_events(vec![RawInputEvent::Key(
            shepr_termio::input::TerminalKey::new(KeyCode::Char('k'), KeyModifiers::empty())
                .with_kind(kind),
        )]);
        assert!(moved.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.command, EndpointCommand::PaneScroll(params)
                    if params.pane_id == "w1:p1" && params.offset_from_bottom == 1)
        )));
        if selection_before_gap.is_some() {
            assert_eq!(
                state
                    .mouse_selection
                    .selection
                    .as_ref()
                    .expect("linewise selection")
                    .ordered_cells(),
                (
                    (shepr_vt::AbsRow(19), 0),
                    (
                        shepr_vt::AbsRow(if selection_before_gap == Some(true) {
                            21
                        } else {
                            20
                        }),
                        0
                    )
                )
            );
        }

        assert_eq!(state.mode, ClientShellMode::Copy);
        assert!(state.copy_mode.is_some());
        assert_eq!(
            state
                .copy_mode
                .as_ref()
                .map(|copy_mode| copy_mode.cursor.row),
            Some(shepr_vt::AbsRow(19))
        );
    }
}

#[test]
fn a_failed_submit_drops_the_queued_operations_and_keeps_the_invariant() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.compose(106, 20).expect("compose");
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    let out = state.handle_input_bytes(b"w");
    let [ClientShellAction::Endpoint { request, .. }] = out.actions.as_slice() else {
        panic!("request")
    };
    let id = request.id.clone();
    for _ in 0..3 {
        state.copy_pipeline.push_op(ClientCopyOperation::Search {
            query: "needle".into(),
            direction: shepr_protocol::command::PaneCopySearchDirection::Forward,
            repeat: false,
        });
    }
    state
        .copy_mode
        .as_mut()
        .expect("copy")
        .search
        .get_or_insert_with(ClientCopySearch::default)
        .copy_after_result = true;
    state.handle_input_bytes(b"v");
    assert_eq!(state.copy_pipeline.keys_len(), 1);
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Reconnecting);
    let cursor = state.copy_mode.as_ref().expect("copy").cursor;
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &id,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: test_pane_id("w1:p1"),
            cursor,
        }),
    );
    assert!(!state.copy_pipeline.in_flight());
    assert!(state.copy_pipeline.ops_is_empty());
    assert!(state.copy_pipeline.keys_is_empty());
    let copy = state.copy_mode.as_ref().expect("copy");
    assert!(
        !copy
            .search
            .as_ref()
            .is_some_and(|search| search.copy_after_result)
    );
    assert!(copy.selection.is_some(), "the queued key replayed");
}
#[test]
fn a_search_that_exits_copy_mode_does_not_replay_or_dispatch() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface(surface());
    state.compose(106, 20).expect("compose");
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    let out = state.handle_input_bytes(b"/needle\r");
    let [ClientShellAction::Endpoint { request, .. }] = out.actions.as_slice() else {
        panic!("request")
    };
    let id = request.id.clone();
    state.handle_input_bytes(b"w");
    state
        .copy_mode
        .as_mut()
        .expect("copy")
        .search
        .get_or_insert_with(ClientCopySearch::default)
        .copy_after_result = true;
    let out = state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &id,
        Ok(EndpointReply::PaneCopySearch {
            pane_id: test_pane_id("w1:p1"),
            matches: vec![],
            total: 0,
            current: None,
            current_global: None,
        }),
    );
    assert!(state.copy_mode.is_none());
    assert!(!state.copy_pipeline.in_flight());
    assert!(state.copy_pipeline.keys_is_empty());
    assert!(state.copy_pipeline.ops_is_empty());
    assert!(out.requests.is_empty());
    assert!(out.actions.iter().all(|action| !matches!(
        action,
        ClientShellAction::Endpoint { request, .. } if matches!(
            request.command,
            EndpointCommand::PaneCopyMotion(_) | EndpointCommand::PaneCopySearch(_)
        )
    )));
}
