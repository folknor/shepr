//! Copy mode: the session on its own, and through the whole shell its cursor,
//! selections, search, queued input and requests.

use super::{ClientCopySelection, CopyEntry, CopySession};
use crate::endpoint::{ClientEndpointId, EndpointFailureStatus};
use crate::shell::config::ClientShellConfig;
use crate::shell::input::events::PaneInputBatchAccounting;
use crate::shell::ledger::{DropReason, Ticket};
use crate::shell::state::{
    ClientShellAction, ClientShellEndpointError, ClientShellInput, ClientShellMode,
    ClientShellRequest, ClientShellState,
};
use crate::shell::tests::{
    answer, cell_bg, copy_search, copy_search_result, copy_shell, frame_rows, help_overlay,
    open_help, pane_scroll_result, press_overlay_key, ready_shell, request_id, snapshot, surface,
};
use crate::tests::test_pane_id;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use shepr_config::ClientConfig;
use shepr_protocol::command::{EndpointCommand, EndpointReply};
use shepr_protocol::{
    ClientMessage, ClientPaneInputEvent, ClientShellPane, FrameData, SurfaceRect,
};
use shepr_surface::ratatui_conversion::{FrameDataExt as _, WireColorExt as _};
use shepr_term::selection::SelectionShape;
use shepr_termio::input::raw_input::RawInputEvent;

fn pane_id() -> shepr_protocol::PublicPaneId {
    let workspace =
        shepr_protocol::WorkspaceId::from_number(1).expect("one-based workspace number");
    shepr_protocol::PublicPaneId::new(
        &workspace,
        shepr_protocol::PanePublicNumber::new(1).expect("nonzero literal"),
    )
}

fn session() -> CopySession {
    CopySession::start(CopyEntry {
        pane_id: pane_id(),
        scroll: shepr_term::ScrollMetrics::new(0, 0, 2, shepr_term::AbsRow(0)),
        geometry: (10, 2),
        alternate_screen_active: false,
        cursor: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(0),
            col: 0,
        },
        rows: Ticket::fixture(1),
    })
}

#[test]
fn ending_a_session_discards_its_queue() {
    let mut state = copy_shell();
    // A word motion goes to the server, and `j` typed behind it waits for its answer.
    let motion = state.handle_input_bytes(b"w");
    assert!(matches!(
        &motion.actions[..],
        [ClientShellAction::Endpoint { .. }]
    ));
    state.handle_input_bytes(b"j");
    assert!(state.copy_in_flight());
    assert_eq!(state.copy_keys_len(), 1);

    // The presented server reboots, which resets the projection.
    let mut rebooted = snapshot();
    rebooted.boot_id = crate::tests::test_boot_id("rebooted");
    state.set_snapshot(Box::new(rebooted));
    assert!(state.copy.is_none());

    let next = session();
    assert!(!next.pipeline().in_flight());
    assert!(next.pipeline().keys_is_empty());
}

#[test]
fn projected_selection_follows_anchor_and_cursor() {
    let plain = session();
    assert!(plain.projected_selection().is_none());

    let anchor = shepr_term::Point::new(shepr_term::AbsRow(0), 1);
    let mut selecting = session().with_selection(ClientCopySelection::Character { anchor });
    selecting.cursor = shepr_protocol::command::PaneTextPoint {
        row: shepr_term::AbsRow(1),
        col: 3,
    };
    let projected = selecting
        .projected_selection()
        .expect("a character selection projects");
    assert!(projected.belongs_to(&pane_id()));
    assert_eq!(projected.shape(), SelectionShape::Range);
    assert_eq!(
        projected.ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(0), 1),
            shepr_term::Point::new(shepr_term::AbsRow(1), 3)
        )
    );

    let linewise = session()
        .with_selection(ClientCopySelection::Linewise {
            anchor_row: shepr_term::AbsRow(1),
        })
        .with_cursor(shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(0),
            col: 4,
        });
    let projected = linewise
        .projected_selection()
        .expect("a linewise selection projects");
    assert_eq!(projected.shape(), SelectionShape::Lines);
    assert_eq!(
        projected.ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(0), 0),
            shepr_term::Point::new(shepr_term::AbsRow(1), 0)
        )
    );
}

/// A ready shell in copy mode with a word motion in flight, after typing `before` in it,
/// and the motion's request id.
fn shell_awaiting_copy_operation(before: &[u8]) -> (ClientShellState, shepr_protocol::RequestId) {
    let mut state = copy_shell();
    if !before.is_empty() {
        state.handle_input_bytes(before);
    }
    let motion = state.handle_input_bytes(b"w");
    let id = request_id(&motion.actions).to_owned();
    assert!(state.copy_in_flight());
    (state, id)
}

/// Answers the copy motion `id` with the cursor where it already is.
fn answer_motion_in_place(state: &mut ClientShellState, id: &shepr_protocol::RequestId) {
    let cursor = state.copy.as_ref().expect("copy mode").cursor;
    answer(
        state,
        id,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: test_pane_id("w1:p1"),
            cursor,
        }),
    );
}

#[test]
fn copy_prefix_replays_after_keys_queued_behind_an_operation() {
    let (mut state, motion) = shell_awaiting_copy_operation(b"");
    let mut outcome = ClientShellInput::default();
    let mut accounting = PaneInputBatchAccounting::default();
    let prefix = state.config.keybinds.prefix;

    state.handle_key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::empty()),
        &mut outcome,
        &mut accounting,
    );
    state.handle_key(
        shepr_term::key::TerminalKey::new(prefix.code, prefix.modifiers),
        &mut outcome,
        &mut accounting,
    );

    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert_eq!(state.copy_keys_len(), 2);

    answer_motion_in_place(&mut state, &motion);

    assert_eq!(state.mode.kind(), ClientShellMode::Prefix);
    assert!(
        state
            .copy
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.selection.is_some())
    );
    assert!(state.copy_keys_empty());
}

#[test]
fn copy_escape_cancels_selection_started_by_prior_queued_key() {
    let (mut state, motion) = shell_awaiting_copy_operation(b"v");
    assert!(
        state
            .copy
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.selection.is_some())
    );
    assert!(state.mouse_selection.selection.is_some());
    let mut outcome = ClientShellInput::default();
    let mut accounting = PaneInputBatchAccounting::default();

    state.handle_key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::empty()),
        &mut outcome,
        &mut accounting,
    );
    state.handle_key(
        shepr_term::key::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
        &mut outcome,
        &mut accounting,
    );

    assert_eq!(state.copy_keys_len(), 2);
    answer_motion_in_place(&mut state, &motion);

    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert!(
        state
            .copy
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.selection.is_none())
    );
    assert!(state.mouse_selection.selection.is_none());
    assert!(state.copy_keys_empty());
}

#[test]
fn pasted_help_and_copy_queries_normalize_single_line_text() {
    let mut state = ready_shell();
    open_help(&mut state);
    press_overlay_key(&mut state, KeyCode::Char('/'));

    assert!(state.insert_overlay_text("work\nspace"));
    assert!(matches!(
        help_overlay(&state),
        Some(help) if help.query().as_str() == "work space"
    ));

    // The first Escape leaves the search, the second closes Help.
    press_overlay_key(&mut state, KeyCode::Esc);
    press_overlay_key(&mut state, KeyCode::Esc);
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    state.handle_input_bytes(b"/");

    assert!(state.insert_copy_search_text("needle\r\n"));
    assert_eq!(
        state
            .copy
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
        FrameData::from_ratatui_buffer_with_hyperlinks(&Buffer::with_lines(lines), None, &[])
            .expect("test buffer is a valid frame");
    let rect = SurfaceRect {
        x: 0,
        y: 0,
        width: area.width,
        height: area.height,
    };
    pane_surface.panes[0].rect = rect;
    pane_surface.panes[0].content_rect = rect;
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        50,
        usize::from(area.height),
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
    state.compose(106, 20).expect("terminal frame");
    let mut outcome = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut outcome));

    // The cursor starts on the last line of history, which no scroll can lift above the
    // bar's usual row: the bar moves to the top row instead.
    let accent = shepr_protocol::WireColor::from_ratatui(state.palette.accent);
    let bottom = area.bottom() - 1;
    let frame = state.compose(106, 20).expect("copy frame");
    let rows = frame_rows(&frame);
    assert!(rows[usize::from(area.y)].contains("COPY"));
    assert!(!rows[usize::from(bottom)].contains("COPY"));
    assert_eq!(
        frame.cells()[usize::from(bottom) * 106 + usize::from(area.x)].bg,
        accent
    );

    // One row up the bar is back at the bottom, clear of the cursor.
    state.handle_input_bytes(b"k");
    let frame = state.compose(106, 20).expect("copy frame");
    let rows = frame_rows(&frame);
    assert!(rows[usize::from(bottom)].contains("COPY"));
    assert_eq!(
        frame.cells()[usize::from(bottom - 1) * 106 + usize::from(area.x)].bg,
        accent
    );

    // Scrolled back, a motion onto the covered row scrolls one line instead of hiding the
    // cursor under the bar.
    let height = u64::from(area.height);
    // Up from row 48 + height to row 40 scrolls the viewport back ten lines; back down to
    // the row above the covered one leaves it there.
    for _ in 0..height + 8 {
        state.handle_input_bytes(b"k");
    }
    assert_eq!(
        state.copy.as_ref().map(|copy_mode| copy_mode.cursor.row),
        Some(shepr_term::AbsRow(40))
    );
    for _ in 0..height - 2 {
        state.handle_input_bytes(b"j");
    }
    let copy_mode = state.copy.as_ref().expect("still in copy mode");
    assert_eq!(copy_mode.cursor.row, shepr_term::AbsRow(40 + height - 2));
    assert_eq!(copy_mode.scroll.offset_from_bottom, 10);
    state.handle_input_bytes(b"j");
    let copy_mode = state.copy.as_ref().expect("still in copy mode");
    assert_eq!(copy_mode.cursor.row, shepr_term::AbsRow(40 + height - 1));
    assert_eq!(copy_mode.scroll.offset_from_bottom, 9);
}

/// A shell showing a 200 by 60 pane with 50 rows of history in a 106 by 30 screen, as after
/// a resize before the resized surface arrives: the pane is clipped to the client area.
/// Returns the shell, composed once, the surface and its generation.
fn clipped_pane_shell() -> (
    ClientShellState,
    shepr_protocol::PaneSurfaceFrame,
    shepr_protocol::ConnectionGeneration,
) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let lines = (0..60).map(|_| "x".repeat(200)).collect::<Vec<_>>();
    let mut oversized = surface();
    oversized.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &Buffer::with_lines(lines.iter().map(String::as_str)),
        None,
        &[],
    )
    .expect("test buffer is a valid frame");
    let full = SurfaceRect {
        x: 0,
        y: 0,
        width: 200,
        height: 60,
    };
    oversized.panes[0].rect = full;
    oversized.panes[0].content_rect = full;
    oversized.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        50,
        60,
        shepr_term::AbsRow(0),
    ));
    let generation = state
        .endpoints
        .active
        .generation()
        .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST);
    state.receive_pane_surface_from(oversized.clone(), generation);
    state.compose(106, 30).expect("clipped frame");
    (state, oversized, generation)
}

#[test]
fn copy_mode_in_a_clipped_pane_keeps_its_cursor_on_the_drawn_rows() {
    let (mut state, _, _) = clipped_pane_shell();
    let hit = state.pane_hits()[0].clone();
    assert!(
        hit.content_rect.height < 60 && hit.content_rect.width < 200,
        "the hit is clipped to the pane area"
    );
    let drawn_cursor = |state: &mut ClientShellState| {
        state.compose(106, 30).expect("copy frame");
        state.drawn().copy_cursor()
    };

    // With no terminal cursor to start from, copy mode starts on the last drawn row, not on
    // the pane's last row below the clip.
    let mut outcome = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut outcome));
    let (_, row) = drawn_cursor(&mut state).expect("the copy cursor starts on a drawn row");
    assert_eq!(row, hit.content_rect.bottom() - 1);

    // At the newest rows no scroll can bring the rows below the clip into view, so a motion
    // down stops on the last drawn row.
    for _ in 0..5 {
        state.handle_input_bytes(b"j");
        let (_, row) = drawn_cursor(&mut state).expect("moving down stays on a drawn row");
        assert_eq!(row, hit.content_rect.bottom() - 1);
    }

    // Paging and the ends of history keep the cursor within the drawn rows of the viewport
    // the session asks for (the surface for it has not arrived, so nothing is drawn yet).
    let visible_rows = u64::from(hit.content_rect.height);
    for keys in [&b"\x1b[5~"[..], b"\x1b[6~", b"g", b"G"] {
        state.handle_input_bytes(keys);
        let copy_mode = state.copy.as_ref().expect("still in copy mode");
        let from_top = copy_mode
            .cursor
            .row
            .0
            .checked_sub(copy_mode.viewport_top().0);
        assert!(
            from_top.is_some_and(|row| row < visible_rows),
            "{keys:?} leaves the copy cursor on a drawn row"
        );
    }
    // Back at the newest rows the viewport is the one on screen.
    let (_, row) = drawn_cursor(&mut state).expect("the end of history is drawn");
    assert_eq!(row, hit.content_rect.bottom() - 1);

    // Moving right stops at the last drawn column.
    for _ in 0..hit.content_rect.width {
        state.handle_input_bytes(b"l");
    }
    let (col, _) = drawn_cursor(&mut state).expect("moving right stays on a drawn column");
    assert_eq!(col, hit.content_rect.right() - 1);
    assert_eq!(
        state.copy.as_ref().map(|copy_mode| copy_mode.geometry),
        Some((200, 60)),
        "the session keeps the pane's full geometry"
    );
}

#[test]
fn copy_mode_in_a_clipped_pane_keeps_the_panes_full_geometry() {
    let (mut state, oversized, generation) = clipped_pane_shell();
    let area = state.layout(106, 30).pane_surface;
    assert!(
        state.pane_hits()[0].content_rect.width < 200
            && state.pane_hits()[0].content_rect.width <= area.width,
        "the hit is clipped to the pane area"
    );

    let mut outcome = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut outcome));
    assert_eq!(
        state.copy.as_ref().map(|copy_mode| copy_mode.geometry),
        Some((200, 60)),
        "copy mode works on the pane's rows, not on the rows the client area shows"
    );

    // The next surface refreshes the session from the wire pane; the copy cursor must stay
    // coherent with the clipped hit rather than read as a resize.
    state.receive_pane_surface_from(oversized, generation);
    state.compose(106, 30).expect("clipped frame");
    let hit = state.pane_hits()[0].clone();
    assert_eq!(
        state.copy.as_ref().map(|copy_mode| copy_mode.geometry),
        Some((200, 60))
    );
    assert!(crate::shell::view::resolve::client_copy_surface_coherent(
        state.copy.as_ref(),
        &hit
    ));
}

#[test]
fn keyboard_copy_mode_owns_cursor_selection_copy_and_scroll_restore() {
    let mut config = ClientConfig::default();
    config.ui.copy_on_select = false;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
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

    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert_eq!(
        state.copy.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_term::AbsRow(21))
    );
    assert!(enter.actions.is_empty());

    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Char('b'),
        KeyModifiers::CONTROL,
    ))]);
    assert_eq!(state.mode.kind(), ClientShellMode::Prefix);
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Esc,
        KeyModifiers::empty(),
    ))]);
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);

    let page = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::PageUp, KeyModifiers::empty()),
    )]);
    assert_eq!(
        state.copy.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_term::AbsRow(20))
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

    let top = state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Char('g'),
        KeyModifiers::empty(),
    ))]);
    assert!(top.actions.is_empty());
    assert_eq!(
        state.copy.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_term::AbsRow(0))
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
            if params.pane_id == crate::tests::test_pane_id("w1:p1") && params.offset_from_bottom == 20
    ));
    let top_request_id = request.id.clone();
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &top_request_id,
        Ok(pane_scroll_result(20, 20, 2)),
    );

    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Char('v'),
        KeyModifiers::empty(),
    ))]);
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Char('l'),
        KeyModifiers::empty(),
    ))]);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_term::selection::Selection::is_visible)
    );

    let copy = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    assert!(state.copy.is_none());
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
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
            0,
            0,
            2,
            shepr_term::AbsRow(0),
        ));
        state.receive_pane_surface_from(
            pane_surface.clone(),
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        state.compose(106, 20).expect("composed frame");
        state.handle_input_bytes(b"\x02[");
        state.handle_input_bytes(selection_key);
        state.handle_input_bytes(b"k");
        let range = state
            .mouse_selection
            .selection
            .as_ref()
            .expect("selected range")
            .ordered_rows();

        pane_surface.surface_revision = pane_surface
            .surface_revision
            .checked_next()
            .expect("test precondition");
        pane_surface.panes[0].content_revision.advance();
        pane_surface.frame.cells_mut()[0].symbol = "X".into();
        state.receive_pane_surface_from(
            pane_surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        assert_eq!(state.mode.kind(), ClientShellMode::Copy);
        assert!(
            state
                .copy
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
                .ordered_rows(),
            range
        );

        // A linewise selection is requested across the pane's full width; a character
        // selection is requested as its own range.
        let expected = if selection_key == b"V" {
            let width = state.copy_hit().expect("copy hit").content_rect.width;
            (
                shepr_term::Point::new(range.0.row, 0),
                shepr_term::Point::new(range.1.row, width.saturating_sub(1)),
            )
        } else {
            range
        };
        let copied = state.handle_input_bytes(b"y");
        assert!(copied.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
                    if (params.anchor.row, params.anchor.col) == (expected.0.row, expected.0.col)
                        && (params.cursor.row, params.cursor.col) == (expected.1.row, expected.1.col))
        )));
        assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
        assert!(state.mouse_selection.selection.is_none());
        assert!(state.copy.is_none());
    }
}

#[test]
fn empty_keyboard_anchor_keeps_search_fallback_revision_guard() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        0,
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
    state.handle_input_bytes(b"\x02[");
    let search = state.handle_input_bytes(b"/LIVE\r");
    let [ClientShellAction::Endpoint { request, .. }] = &search.actions[..] else {
        panic!("search request");
    };
    let found = shepr_protocol::command::PaneTextRange {
        start: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(0),
            col: 0,
        },
        end: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(0),
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
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
            0,
            0,
            2,
            shepr_term::AbsRow(0),
        ));
        state.receive_pane_surface_from(
            pane_surface.clone(),
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        state.compose(106, 20).expect("composed frame");
        state.handle_input_bytes(b"\x02[");
        state.handle_input_bytes(b"vk");
        assert!(state.mouse_selection.selection.is_some());
        pane_surface.surface_revision = pane_surface
            .surface_revision
            .checked_next()
            .expect("test precondition");
        pane_surface.panes[0].content_revision.advance();
        if screen_switch {
            pane_surface.panes[0].alternate_screen_active = true;
        } else {
            pane_surface.panes[0].content_rect.width -= 1;
        }
        state.receive_pane_surface_from(
            pane_surface,
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        assert!(state.mouse_selection.selection.is_none());
        assert!(
            state
                .copy
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
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        0,
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
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy.as_ref().expect("copy mode").cursor;

    let motion = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('w'), KeyModifiers::empty()),
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
    assert_eq!(state.copy.as_ref().map(|mode| mode.cursor.col), Some(3));
}

#[test]
fn keys_after_an_exit_key_reach_the_pane_once_an_in_flight_copy_motion_replays() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        0,
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
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy.as_ref().expect("copy mode").cursor;
    let key = |code| {
        RawInputEvent::Key(shepr_term::key::TerminalKey::new(
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
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
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
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    assert!(state.copy.is_none());
    assert!(state.copy_keys_empty());
    assert!(
        replayed.requests.iter().any(|request| matches!(
            request,
            ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events })
                if pane_id == &crate::tests::test_pane_id("w1:p1")
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
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy.as_ref().expect("copy mode").cursor;

    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Char('?'),
        KeyModifiers::SHIFT,
    ))]);
    assert!(state.copy.as_ref().is_some_and(|mode| {
        mode.search
            .as_ref()
            .and_then(|search| search.prompt.as_ref())
            .is_some_and(|prompt| {
                prompt.direction == shepr_protocol::command::PaneCopySearchDirection::Backward
            })
    }));
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Esc,
        KeyModifiers::empty(),
    ))]);
    assert!(state.copy.as_ref().is_none_or(|mode| {
        mode.search
            .as_ref()
            .is_none_or(|search| search.prompt.is_none())
    }));

    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Char('/'),
        KeyModifiers::empty(),
    ))]);
    state.handle_raw_events(vec![RawInputEvent::Paste("junk".into())]);
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    ))]);
    state.handle_raw_events(vec![RawInputEvent::Paste("nee".into())]);
    state.handle_raw_events(vec![RawInputEvent::Paste("dleX".into())]);
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Backspace,
        KeyModifiers::empty(),
    ))]);
    assert_eq!(
        state
            .copy
            .as_ref()
            .and_then(|mode| mode.search.as_ref())
            .and_then(|search| search.prompt.as_ref())
            .map(|prompt| prompt.query.as_str()),
        Some("needle")
    );

    let search = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
    )]);
    let [ClientShellAction::Endpoint { request, .. }] = &search.actions[..] else {
        panic!("search should use endpoint terminal semantics");
    };
    let request_id = request.id.clone();
    assert!(matches!(
        &request.command,
        EndpointCommand::PaneCopySearch(params)
            if params.pane_id == crate::tests::test_pane_id("w1:p1")
                && params.query == "needle"
                && params.direction == shepr_protocol::command::PaneCopySearchDirection::Forward
                && params.cursor == origin
                && params.previous.is_none()
    ));
    let matches = vec![
        shepr_protocol::command::PaneTextRange {
            start: shepr_protocol::command::PaneTextPoint {
                row: shepr_term::AbsRow(5),
                col: 2,
            },
            end: shepr_protocol::command::PaneTextPoint {
                row: shepr_term::AbsRow(5),
                col: 7,
            },
        },
        shepr_protocol::command::PaneTextRange {
            start: shepr_protocol::command::PaneTextPoint {
                row: shepr_term::AbsRow(15),
                col: 1,
            },
            end: shepr_protocol::command::PaneTextPoint {
                row: shepr_term::AbsRow(15),
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
        state.copy.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_term::AbsRow(5))
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
    {
        let metrics = scrolled_surface.panes[0]
            .scroll
            .as_mut()
            .expect("scroll metrics");
        *metrics = metrics.with_offset(15);
    }
    state.receive_pane_surface_from(
        scrolled_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let frame = state.compose(106, 20).expect("search frame");
    let hit = state.pane_hits()[0].clone();
    let viewport_top = 5u16;
    assert_eq!(
        cell_bg(
            &frame,
            (
                hit.content_rect.x + 2,
                hit.content_rect.y + (5 - viewport_top)
            )
        ),
        state.palette.accent
    );

    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Char('v'),
        KeyModifiers::empty(),
    ))]);
    let repeat = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('n'), KeyModifiers::empty()),
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
        state.copy.as_ref().map(|mode| mode.cursor.row),
        Some(shepr_term::AbsRow(15))
    );
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(shepr_term::selection::Selection::is_visible)
    );

    let reverse = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
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

    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Esc,
        KeyModifiers::empty(),
    ))]);
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert!(state.copy.as_ref().is_some_and(|mode| {
        mode.search
            .as_ref()
            .is_none_or(|search| search.query.is_empty())
            && mode.selection.is_none()
    }));
    let exit = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    )]);
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
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
fn copy_mode_survives_mouse_motion_and_parks_across_focus_changes() {
    let mut config = ClientConfig::default();
    config.ui.copy_on_select = false;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
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
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);

    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert!(state.copy.is_some());

    state.handle_input_bytes(b"v");
    assert!(
        state
            .copy
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
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    assert!(
        state
            .copy
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.selection.is_some())
    );

    let shepr_term::key::KeyChord {
        code: prefix_key,
        modifiers: prefix_modifiers,
    } = state.config.keybinds.prefix;
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        prefix_key,
        prefix_modifiers,
    ))]);
    state.set_snapshot(Box::new(unfocused.clone()));
    assert_eq!(state.mode.kind(), ClientShellMode::Prefix);
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Esc,
        KeyModifiers::empty(),
    ))]);
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);

    let mut other_selection = shepr_term::selection::Selection::range(
        test_pane_id("w1:p2"),
        shepr_term::Point::new(shepr_term::AbsRow(0), 0),
        shepr_term::Point::new(shepr_term::AbsRow(0), 1),
    );
    assert!(other_selection.finish());
    state.mouse_selection.selection = Some(other_selection);
    state.set_snapshot(Box::new(unfocused));
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.belongs_to(&crate::tests::test_pane_id("w1:p2")))
    );

    let mut other_surface = surface();
    other_surface.panes[0].pane_id = test_pane_id("w1:p2");
    state.receive_pane_surface_from(
        other_surface.clone(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    other_surface.surface_revision = other_surface
        .surface_revision
        .checked_next()
        .expect("test precondition");
    other_surface.panes[0].content_revision = shepr_test_fixtures::counter_at(1);
    state.receive_pane_surface_from(
        other_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert!(state.mouse_selection.selection.is_some());
    let copy = state.handle_raw_events(vec![RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )]);
    assert!(copy.requests.is_empty());
    assert!(
        matches!(&copy.actions[..], [ClientShellAction::Endpoint { request, .. }]
        if matches!(&request.command, EndpointCommand::PaneSelectionRead(params)
            if params.pane_id == crate::tests::test_pane_id("w1:p2")))
    );

    state.set_snapshot(Box::new(snapshot()));
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert!(state.copy.is_some());
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.belongs_to(&crate::tests::test_pane_id("w1:p1")))
    );
    state.handle_raw_events(vec![RawInputEvent::Paste("ignored".into())]);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.belongs_to(&crate::tests::test_pane_id("w1:p1")))
    );

    crate::shell::tests::enter_navigation(&mut state);
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Esc,
        KeyModifiers::empty(),
    ))]);
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.belongs_to(&crate::tests::test_pane_id("w1:p1")))
    );
    state.record_binding(
        &shepr_termio::input::KeybindAction::EnterResizeMode,
        &mut ClientShellInput::default(),
    );
    assert_eq!(state.mode.kind(), ClientShellMode::Resize);
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        KeyCode::Esc,
        KeyModifiers::empty(),
    ))]);
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .is_some_and(|selection| selection.belongs_to(&crate::tests::test_pane_id("w1:p1")))
    );
}

#[test]
fn clicking_the_pane_scrollbar_preserves_copy_mode_for_its_focused_pane() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
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
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));
    let track = state.pane_hits()[0]
        .scrollbar_rect
        .expect("pane scrollbar hit");

    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: track.x,
        row: track.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert!(state.copy.is_some());
}

#[test]
fn retained_selection_copy_suppresses_key_repeats() {
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
    let mut selection = shepr_term::selection::Selection::range(
        test_pane_id("w1:p1"),
        shepr_term::Point::new(shepr_term::AbsRow(0), 0),
        shepr_term::Point::new(shepr_term::AbsRow(0), 1),
    );
    assert!(selection.finish());
    state.mouse_selection.selection = Some(selection);

    let key = shepr_term::key::TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
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
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        0,
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
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy.as_ref().expect("copy mode").cursor;

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
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
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
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let origin = state.copy.as_ref().expect("copy mode").cursor;
    let motion = state.handle_input_bytes(b"w");
    state.handle_input_bytes(b"l");
    let shepr_term::key::KeyChord {
        code: prefix_key,
        modifiers: prefix_modifiers,
    } = state.config.keybinds.prefix;
    state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
        prefix_key,
        prefix_modifiers,
    ))]);
    // The prefix waits behind the motion and the key typed before it.
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);

    let detach = state.handle_input_bytes(b"q");
    assert!(!detach.detach);
    assert_eq!(state.copy_keys_len(), 3);

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
    assert!(state.copy_keys_empty());
}

#[test]
fn copy_mode_exit_keys_act_after_earlier_queued_input() {
    for key in [
        shepr_term::key::TerminalKey::new(KeyCode::Char('q'), KeyModifiers::empty()),
        shepr_term::key::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
    ] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
            0,
            10,
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
        let mut enter = ClientShellInput::default();
        assert!(state.enter_copy_mode(&mut enter));
        let origin = state.copy.as_ref().expect("copy mode").cursor;
        let motion = state.handle_input_bytes(b"w");
        let motion_id = match &motion.actions[0] {
            ClientShellAction::Endpoint { request, .. } => request.id.clone(),
            _ => unreachable!(),
        };
        state.handle_input_bytes(b"l");

        state.handle_raw_events(vec![RawInputEvent::Key(key)]);

        // The exit key queues behind `l` and the motion it follows.
        assert_eq!(state.mode.kind(), ClientShellMode::Copy);
        assert!(state.copy_in_flight());
        assert_eq!(state.copy_keys_len(), 2);

        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &motion_id,
            Ok(EndpointReply::PaneCopyMotion {
                pane_id: shepr_test_fixtures::id("w1:p1"),
                cursor: origin,
            }),
        );

        assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
        assert!(state.copy.is_none());
        assert!(!state.copy_in_flight());
        assert!(state.copy_keys_empty());
    }
}

#[test]
fn an_interrupt_key_leaves_copy_mode_behind_a_full_queue_and_the_late_reply_is_ignored() {
    for exit_with_escape in [true, false] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
        pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
            0,
            10,
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
        let mut enter = ClientShellInput::default();
        assert!(state.enter_copy_mode(&mut enter));
        let origin = state.copy.as_ref().expect("copy mode").cursor;
        let motion = state.handle_input_bytes(b"w");
        let motion_id = match &motion.actions[0] {
            ClientShellAction::Endpoint { request, .. } => request.id.clone(),
            _ => unreachable!(),
        };
        for _ in 0..crate::limits::MAX_COPY_INPUT_QUEUE {
            state.handle_input_bytes(b"j");
        }
        assert_eq!(state.copy_keys_len(), crate::limits::MAX_COPY_INPUT_QUEUE);

        let key = if exit_with_escape {
            shepr_term::key::TerminalKey::new(KeyCode::Esc, KeyModifiers::empty())
        } else {
            let shepr_term::key::KeyChord {
                code: prefix_key,
                modifiers: prefix_modifiers,
            } = state.config.keybinds.prefix;
            shepr_term::key::TerminalKey::new(prefix_key, prefix_modifiers)
        };
        state.handle_raw_events(vec![RawInputEvent::Key(key)]);

        // The request that stopped answering and the keys behind it are given up.
        assert!(!state.copy_in_flight());
        assert!(state.copy_keys_empty());
        if exit_with_escape {
            assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
            assert!(state.copy.is_none());
        } else {
            assert_eq!(state.mode.kind(), ClientShellMode::Prefix);
        }
        let cursor_before = state.copy.as_ref().map(|copy_mode| copy_mode.cursor);

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
            state.copy.as_ref().map(|copy_mode| copy_mode.cursor),
            cursor_before
        );
        assert!(state.copy_keys_empty());
    }
}

#[test]
fn failed_copy_operation_replays_keys_while_the_copy_pane_still_owns_input() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
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
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));
    let origin = state.copy.as_ref().expect("copy mode").cursor;

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
        state.copy.as_ref().map(|copy_mode| copy_mode.cursor.col),
        Some(origin.col.saturating_add(1))
    );
    assert!(state.copy_keys_empty());
}

#[test]
fn queued_copy_input_is_bounded() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
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
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));
    state.handle_input_bytes(b"w");

    for _ in 0..crate::limits::MAX_COPY_INPUT_QUEUE + 8 {
        state.handle_input_bytes(b"j");
    }

    assert_eq!(state.copy_keys_len(), crate::limits::MAX_COPY_INPUT_QUEUE);
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
                pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
                    0,
                    10,
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
                assert!(!state.copy_keys_empty());

                assert_eq!(
                    if unsent {
                        state.drop_request(&request_id, DropReason::Unsent)
                    } else {
                        state.drop_request(&request_id, DropReason::Interrupted)
                    },
                    crate::shell::state::Repaint::Needed
                );

                assert!(state.ledger.is_empty());
                assert!(!state.copy_in_flight());
                assert!(state.copy_keys_empty());
                assert!(state.notices.visible().is_none());
                assert!(state.scroll_lanes.is_idle());
                assert_eq!(state.mode.kind(), ClientShellMode::Copy);
                // Cancellation leaves the copy session usable for newly typed input.
                let next = state.handle_input_bytes(b"w");
                assert!(matches!(
                    next.actions.as_slice(),
                    [ClientShellAction::Endpoint { .. }]
                ));
                assert!(state.copy_in_flight());
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
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
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
    let mut enter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut enter));

    let started = state.handle_input_bytes(b"w");
    let [ClientShellAction::Endpoint { request, .. }] = started.actions.as_slice() else {
        panic!("copy motion should issue one endpoint request");
    };
    let request_id = request.id.clone();
    assert!(state.handle_input_bytes(b"w").actions.is_empty());
    assert!(!state.copy_keys_empty());

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
    assert!(!state.copy_in_flight());
    assert!(state.copy_keys_empty());
    assert!(state.notices.visible().is_none());
}

#[test]
fn cancelling_an_old_copy_request_does_not_reset_a_new_session() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
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
    assert!(state.copy.is_none());
    assert!(state.enter_copy_mode(&mut enter));
    let current = state.handle_input_bytes(b"w");
    let [ClientShellAction::Endpoint { request, .. }] = &current.actions[..] else {
        panic!("expected one copy request");
    };
    let current_id = request.id.clone();
    state.handle_input_bytes(b"l");

    state.drop_request(&old_id, DropReason::Unsent);

    assert!(state.copy_in_flight());
    assert_eq!(state.copy_keys_len(), 1);
    assert!(state.ledger.contains(&current_id));
}

#[test]
fn copy_operation_does_not_capture_input_after_focus_moves() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
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
        ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, .. })
            if pane_id == &crate::tests::test_pane_id("w1:p2")
    )));

    let failed = state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &request_id,
        Err(ClientShellEndpointError::Timeout),
    );
    assert!(!failed.requests.iter().any(|request| matches!(
        request,
        ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { .. })
    )));
    assert!(state.copy_keys_empty());
}

#[test]
fn reentering_copy_mode_on_the_same_pane_is_a_no_op() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
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
    let mut first = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut first));
    // `g` scrolls to the top of history, ten lines up.
    state.handle_input_bytes(b"g");
    assert_eq!(
        state
            .copy
            .as_ref()
            .map(|copy_mode| copy_mode.scroll.offset_from_bottom),
        Some(10)
    );
    let mut reenter = ClientShellInput::default();
    assert!(state.enter_copy_mode(&mut reenter));
    assert!(reenter.actions.is_empty());
    assert_eq!(
        state
            .copy
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
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        0,
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
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    state.handle_input_bytes(b"v");
    let origin = state.copy.as_ref().expect("copy mode").cursor;
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
    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
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
    // Ten rows of history, so the matches below name rows the pane still holds.
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
        0,
        10,
        2,
        shepr_term::AbsRow(0),
    ));
    state.receive_pane_surface_from(
        pane_surface.clone(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state.compose(106, 20).expect("composed frame");
    let mut enter = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
    let found_on = |row| shepr_protocol::command::PaneTextRange {
        start: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(row),
            col: 0,
        },
        end: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(row),
            col: 1,
        },
    };
    // The search lands on the match on row 6; the selection then runs from (1, 2) to the
    // cursor at (2, 1).
    let search = copy_search(&mut state);
    answer(
        &mut state,
        &search,
        Ok(copy_search_result(vec![found_on(1), found_on(6)], Some(1))),
    );
    assert_eq!(
        state.copy.as_ref().map(|copy_mode| copy_mode.cursor),
        Some(found_on(6).start)
    );
    state.handle_input_bytes(b"kkkkkllvjh");
    let copy_mode = state.copy.as_ref().expect("copy mode");
    assert_eq!(
        copy_mode.cursor,
        shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(2),
            col: 1,
        }
    );
    assert!(matches!(
        copy_mode.selection,
        Some(ClientCopySelection::Character { anchor })
            if anchor == shepr_term::Point::new(shepr_term::AbsRow(1), 2)
    ));

    pane_surface.surface_revision = pane_surface
        .surface_revision
        .checked_next()
        .expect("test precondition");
    pane_surface.panes[0].content_revision = shepr_test_fixtures::counter_at(2);
    {
        let metrics = pane_surface.panes[0]
            .scroll
            .as_mut()
            .expect("scroll metrics");
        *metrics = shepr_term::ScrollMetrics::new(
            metrics.offset_from_bottom,
            metrics.max_offset_from_bottom,
            metrics.viewport_rows,
            shepr_term::AbsRow(5),
        );
    }
    state.receive_pane_surface_from(
        pane_surface.clone(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let copy_mode = state.copy.as_ref().expect("copy mode retained");
    let search = copy_mode.search.as_ref().expect("search state retained");
    assert_eq!(search.results.matches, vec![found_on(6)]);
    assert_eq!(search.results.total, 1);
    assert_eq!(
        search.results.current,
        Some(shepr_protocol::command::PaneCopySearchPosition {
            window_index: 0,
            global_index: 0,
        })
    );
    assert_eq!(copy_mode.scroll.history_origin, shepr_term::AbsRow(5));
    assert_eq!(copy_mode.cursor.row, shepr_term::AbsRow(5));
    assert!(matches!(
        copy_mode.selection,
        Some(ClientCopySelection::Character { anchor })
            if anchor == shepr_term::Point::new(shepr_term::AbsRow(5), 2)
    ));
    assert_eq!(
        state
            .mouse_selection
            .selection
            .as_ref()
            .expect("clamped copy selection")
            .ordered_rows(),
        (
            shepr_term::Point::new(shepr_term::AbsRow(5), 1),
            shepr_term::Point::new(shepr_term::AbsRow(5), 2)
        )
    );

    pane_surface.surface_revision = pane_surface
        .surface_revision
        .checked_next()
        .expect("test precondition");
    pane_surface.panes[0].content_rect.width -= 1;
    state.receive_pane_surface_from(
        pane_surface,
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let copy_mode = state.copy.as_ref().expect("copy mode retained");
    let search = copy_mode.search.as_ref().expect("search state retained");
    assert!(search.results.matches.is_empty());
    assert_eq!(search.results.total, 0);
    assert_eq!(search.results.current, None);
}

#[test]
fn word_selection_result_survives_focus_snapshot_lag() {
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
    let hit = state.pane_hits()[0].clone();
    let mut request = ClientShellInput::default();
    let metrics = shepr_term::ScrollMetrics::new(0, 0, 2, shepr_term::AbsRow(0));
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
            .is_some_and(shepr_term::selection::Selection::is_visible)
    );
}

#[test]
fn copy_mode_repeat_during_projection_gap_stays_active() {
    for selection_before_gap in [None, Some(true), Some(false)] {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
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
        let mut enter = ClientShellInput::default();
        state.record_binding(&shepr_termio::input::KeybindAction::CopyMode, &mut enter);
        if selection_before_gap == Some(true) {
            state.handle_input_bytes(b"V");
        }
        state.handle_raw_events(vec![RawInputEvent::Key(shepr_term::key::TerminalKey::new(
            KeyCode::Char('k'),
            KeyModifiers::empty(),
        ))]);

        let mut next = snapshot();
        next.revision = next.revision.checked_next().expect("test precondition");
        state.set_snapshot(Box::new(next));
        // The last composed frame is still on screen, so its hit map stays valid until
        // the matching surface is composed.
        assert!(!state.pane_hits().is_empty());
        assert_eq!(state.mode.kind(), ClientShellMode::Copy);
        if selection_before_gap == Some(false) {
            state.handle_input_bytes(b"V");
            assert_eq!(
                state
                    .mouse_selection
                    .selection
                    .as_ref()
                    .expect("linewise selection")
                    .ordered_rows(),
                (
                    shepr_term::Point::new(shepr_term::AbsRow(20), 0),
                    shepr_term::Point::new(shepr_term::AbsRow(20), 0)
                )
            );
            assert_eq!(
                state
                    .mouse_selection
                    .selection
                    .as_ref()
                    .expect("linewise selection")
                    .shape(),
                shepr_term::selection::SelectionShape::Lines
            );
        }

        let kind = if selection_before_gap == Some(false) {
            crossterm::event::KeyEventKind::Press
        } else {
            crossterm::event::KeyEventKind::Repeat
        };
        let moved = state.handle_raw_events(vec![RawInputEvent::Key(
            shepr_term::key::TerminalKey::new(KeyCode::Char('k'), KeyModifiers::empty())
                .with_kind(kind),
        )]);
        assert!(moved.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.command, EndpointCommand::PaneScroll(params)
                    if params.pane_id == crate::tests::test_pane_id("w1:p1") && params.offset_from_bottom == 1)
        )));
        if selection_before_gap.is_some() {
            assert_eq!(
                state
                    .mouse_selection
                    .selection
                    .as_ref()
                    .expect("linewise selection")
                    .ordered_rows(),
                (
                    shepr_term::Point::new(shepr_term::AbsRow(19), 0),
                    shepr_term::Point::new(
                        shepr_term::AbsRow(if selection_before_gap == Some(true) {
                            21
                        } else {
                            20
                        }),
                        0
                    )
                )
            );
        }

        assert_eq!(state.mode.kind(), ClientShellMode::Copy);
        assert!(state.copy.is_some());
        assert_eq!(
            state.copy.as_ref().map(|copy_mode| copy_mode.cursor.row),
            Some(shepr_term::AbsRow(19))
        );
    }
}

#[test]
fn a_replayed_motion_the_endpoint_refuses_leaves_nothing_in_flight() {
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
    state.compose(106, 20).expect("compose");
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    let out = state.handle_input_bytes(b"w");
    let [ClientShellAction::Endpoint { request, .. }] = out.actions.as_slice() else {
        panic!("request")
    };
    let id = request.id.clone();
    // A selection and a second motion wait behind the first; the endpoint then goes
    // away, so the replayed motion cannot be sent.
    state.handle_input_bytes(b"vw");
    assert_eq!(state.copy_keys_len(), 2);
    state.set_endpoint_status(
        &ClientEndpointId::Local,
        EndpointFailureStatus::Reconnecting,
    );
    let cursor = state.copy.as_ref().expect("copy").cursor;
    let out = state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &id,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: test_pane_id("w1:p1"),
            cursor,
        }),
    );
    assert!(!state.copy_in_flight());
    assert!(state.copy_keys_empty());
    assert!(!out.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(request.command, EndpointCommand::PaneCopyMotion(_))
    )));
    let copy = state.copy.as_ref().expect("copy");
    assert!(copy.selection.is_some(), "the queued key replayed");
    // The session takes the next key at once rather than waiting for the refused motion.
    let next = state.handle_input_bytes(b"l");
    assert!(state.copy_keys_empty());
    assert!(next.actions.is_empty());
}

/// `y` typed behind a search waits for its answer, so it copies the match the answer
/// lands on; a key typed after it was typed after the exit and reaches the pane.
#[test]
fn a_copy_typed_behind_a_search_copies_the_match_it_lands_on() {
    let mut state = copy_shell();
    let id = copy_search(&mut state);
    let typed = state.handle_input_bytes(b"yx");
    assert!(typed.actions.is_empty());
    assert!(typed.requests.is_empty());
    assert_eq!(state.copy_keys_len(), 2);
    let found = shepr_protocol::command::PaneTextRange {
        start: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(0),
            col: 2,
        },
        end: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(0),
            col: 5,
        },
    };

    let out = answer(
        &mut state,
        &id,
        Ok(copy_search_result(vec![found], Some(0))),
    );

    assert_eq!(state.mode.kind(), ClientShellMode::Terminal);
    assert!(state.copy.is_none());
    assert!(out.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(
                &request.command,
                EndpointCommand::PaneSelectionRead(params)
                    if params.anchor == found.start && params.cursor == found.end
            )
    )));
    assert!(out.requests.iter().any(|request| matches!(
        request,
        ClientShellRequest::Shown(ClientMessage::ClientShellPaneInput { pane_id, events })
            if pane_id == &test_pane_id("w1:p1")
                && events.iter().any(|event| matches!(
                    event,
                    ClientPaneInputEvent::Key {
                        code: shepr_protocol::ClientKeyCode::Char('x'),
                        ..
                    }
                ))
    )));
}

/// Gives up on the copy request in flight the way input does: the queue behind it fills,
/// the prefix then abandons it, and Esc returns to copy mode in the same session.
fn abandon_through_a_full_queue(state: &mut ClientShellState) {
    assert!(state.copy_in_flight());
    for _ in 0..crate::limits::MAX_COPY_INPUT_QUEUE {
        state.handle_input_bytes(b"j");
    }
    let prefix = state.config.keybinds.prefix;
    state.handle_raw_events(vec![
        RawInputEvent::Key(shepr_term::key::TerminalKey::new(
            prefix.code,
            prefix.modifiers,
        )),
        RawInputEvent::Key(shepr_term::key::TerminalKey::new(
            KeyCode::Esc,
            KeyModifiers::empty(),
        )),
    ]);
    assert_eq!(state.mode.kind(), ClientShellMode::Copy);
    assert!(!state.copy_in_flight());
    assert!(state.copy_keys_empty());
}

#[test]
fn an_ignored_answer_still_reports_its_server_error() {
    let mut s = copy_shell();
    let old = copy_search(&mut s);
    abandon_through_a_full_queue(&mut s);
    let current = s.handle_input_bytes(b"w");
    let current = request_id(&current.actions).to_owned();
    let out = answer(&mut s, &old, Err(ClientShellEndpointError::Timeout));
    assert!(out.repaint);
    assert!(out.actions.is_empty());
    assert!(s.copy_in_flight());
    assert!(s.ledger.contains(&current));
    assert!(
        s.notices
            .timeout_suppressed(crate::shell::notices::NoticeCode::Command(
                shepr_protocol::command::CommandKind::PaneCopySearch
            ))
    );
    abandon_through_a_full_queue(&mut s);
    let another = copy_search(&mut s);
    abandon_through_a_full_queue(&mut s);
    let current = s.handle_input_bytes(b"w");
    let current = request_id(&current.actions).to_owned();
    answer(&mut s, &another, Ok(copy_search_result(Vec::new(), None)));
    assert!(
        !s.notices
            .timeout_suppressed(crate::shell::notices::NoticeCode::Command(
                shepr_protocol::command::CommandKind::PaneCopySearch
            ))
    );
    assert!(s.copy_in_flight());
    assert!(s.ledger.contains(&current));
}
#[test]
fn a_copy_answer_after_the_pipeline_was_reset_is_ignored() {
    let mut s = copy_shell();
    let out = s.handle_input_bytes(b"w");
    let id = request_id(&out.actions).to_owned();
    abandon_through_a_full_queue(&mut s);
    let before = s.copy.as_ref().expect("copy").cursor;
    let out = answer(
        &mut s,
        &id,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: test_pane_id("w1:p1"),
            cursor: shepr_protocol::command::PaneTextPoint {
                row: shepr_term::AbsRow(0),
                col: 3,
            },
        }),
    );
    assert!(out.actions.is_empty());
    assert_eq!(s.copy.as_ref().expect("copy").cursor, before);
}
#[test]
fn an_in_flight_search_answered_after_a_resize_finishes_without_applying() {
    let mut s = copy_shell();
    let id = copy_search(&mut s);
    s.handle_input_bytes(b"l");
    assert!(s.copy_in_flight());
    assert!(!s.copy_keys_empty());

    let mut resized = surface();
    resized.surface_revision = resized
        .surface_revision
        .checked_next()
        .expect("test precondition");
    resized.panes[0].content_revision.advance();
    resized.panes[0].content_rect.width -= 1;
    s.receive_pane_surface_from(
        resized,
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let found = shepr_protocol::command::PaneTextRange {
        start: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(0),
            col: 5,
        },
        end: shepr_protocol::command::PaneTextPoint {
            row: shepr_term::AbsRow(0),
            col: 8,
        },
    };
    assert_ne!(s.copy.as_ref().expect("copy").cursor, found.start);

    answer(&mut s, &id, Ok(copy_search_result(vec![found], Some(0))));

    let session = s.copy.as_ref().expect("copy");
    assert!(
        session
            .search
            .as_ref()
            .is_none_or(|search| search.results.matches.is_empty())
    );
    assert_ne!(session.cursor, found.start);
    assert!(!s.copy_in_flight());
    assert!(s.copy_keys_empty());
}
