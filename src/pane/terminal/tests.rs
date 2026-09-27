use super::*;
use ratatui::{layout::Rect, style::Color};

#[test]
fn plain_page_keys_host_scroll_for_shell_like_decckm_with_bracketed_paste() {
    assert!(
        InputState {
            alternate_screen: false,
            application_cursor: true,
            bracketed_paste: true,
            focus_reporting: false,
            mouse_protocol_mode: crate::input::MouseProtocolMode::None,
            mouse_protocol_encoding: crate::input::MouseProtocolEncoding::Default,
            mouse_alternate_scroll: false,
            modify_other_keys: false,
            color_scheme_reporting: false,
        }
        .plain_page_keys_use_host_scrollback()
    );
}

fn text_cell(text: &str) -> crate::vt::ScreenTextCell {
    crate::vt::ScreenTextCell {
        wide: crate::vt::CellWide::Narrow,
        graphemes: text.chars().map(u32::from).collect(),
    }
}

fn rgb(r: u8, g: u8, b: u8) -> crate::vt::RgbColor {
    crate::vt::RgbColor { r, g, b }
}

#[test]
fn dirty_full_collects_bounded_viewport_patch() {
    let mut terminal = crate::vt::Terminal::new(4, 3, 200);
    terminal.write(b"one\r\ntwo\r\nthree");
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));

    let patch = match pane.collect_dirty_patch(4, 3) {
        TerminalDirtyPatchOutcome::Patch(patch) => patch,
        outcome => panic!("expected viewport patch, got {outcome:?}"),
    };

    assert_eq!(patch.rows.len(), 3);
    assert_eq!(
        patch.rows.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(patch.rows.iter().all(|(_, cells)| cells.len() == 4));
    assert!(matches!(
        pane.collect_dirty_patch(4, 3),
        TerminalDirtyPatchOutcome::Clean
    ));
}

#[test]
fn palette_overrides_are_none_without_an_osc4_write() {
    let default = [rgb(1, 2, 3); 256];
    assert!(PaletteOverrides::new(&default, &default).is_none());
}

#[test]
fn redefined_palette_entries_render_as_rgb_and_others_stay_indexed() {
    let default = [rgb(1, 2, 3); 256];
    let mut active = default;
    active[18] = rgb(169, 177, 214);
    let overrides = PaletteOverrides::new(&active, &default).expect("index 18 differs");

    assert_eq!(
        ghostty_cell_color(crate::vt::CellColor::Palette(18), Some(&overrides)),
        Color::Rgb(169, 177, 214)
    );
    // Untouched entries keep being forwarded, so they still follow the host theme.
    assert_eq!(
        ghostty_cell_color(crate::vt::CellColor::Palette(19), Some(&overrides)),
        Color::Indexed(19)
    );
    // ...and so does everything when the program never wrote a palette at all.
    assert_eq!(
        ghostty_cell_color(crate::vt::CellColor::Palette(18), None),
        Color::Indexed(18)
    );
}

#[test]
fn direct_rgb_cells_are_unaffected_by_palette_overrides() {
    let default = [rgb(1, 2, 3); 256];
    let mut active = default;
    active[18] = rgb(169, 177, 214);
    let overrides = PaletteOverrides::new(&active, &default).expect("index 18 differs");
    assert_eq!(
        ghostty_cell_color(
            crate::vt::CellColor::Rgb(rgb(122, 162, 247)),
            Some(&overrides)
        ),
        Color::Rgb(122, 162, 247)
    );
}

fn wide_text_cells(text: &str) -> [crate::vt::ScreenTextCell; 2] {
    [
        crate::vt::ScreenTextCell {
            wide: crate::vt::CellWide::Wide,
            graphemes: text.chars().map(u32::from).collect(),
        },
        crate::vt::ScreenTextCell {
            wide: crate::vt::CellWide::SpacerTail,
            graphemes: Vec::new(),
        },
    ]
}

fn text_row(
    cells: impl IntoIterator<Item = crate::vt::ScreenTextCell>,
    soft_wrapped: bool,
) -> crate::vt::ScreenTextRow {
    crate::vt::ScreenTextRow {
        cells: cells.into_iter().collect(),
        soft_wrapped,
        wrap_continuation: false,
    }
}

fn search_primary(
    buffer: &RetainedTextBuffer,
    query: &str,
    case_sensitive: bool,
) -> Vec<TerminalTextMatch<AbsRow>> {
    buffer
        .search_window(
            query,
            case_sensitive,
            crate::vt::ActiveScreen::Primary,
            TerminalSearchDirection::Forward,
            TerminalTextPoint {
                row: AbsRow(0),
                col: 0,
            },
            None,
            usize::MAX,
        )
        .matches
}

fn write_numbered_lines(terminal: &mut crate::vt::Terminal, count: usize) {
    for i in 0..count {
        terminal.write(format!("{i:06}\r\n").as_bytes());
    }
}

fn write_wrapped_contract_lines(terminal: &mut crate::vt::Terminal, count: usize) {
    for i in 0..count {
        terminal.write(format!("WRAP-{i:03}-abcdefghijklmnopqrstuvwxyz\r\n").as_bytes());
    }
    terminal.write(b"END");
}

#[test]
fn retained_text_search_crosses_soft_wraps_but_not_hard_lines() {
    let buffer = RetainedTextBuffer::new(
        5,
        vec![
            text_row("abcde".chars().map(|ch| text_cell(&ch.to_string())), true),
            text_row("fgh  ".chars().map(|ch| text_cell(&ch.to_string())), false),
            text_row("abc  ".chars().map(|ch| text_cell(&ch.to_string())), false),
        ],
    );

    let matches = search_primary(&buffer, "def", true);
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0].start,
        TerminalTextPoint {
            row: AbsRow(0),
            col: 3
        }
    );
    assert_eq!(
        matches[0].end,
        TerminalTextPoint {
            row: AbsRow(1),
            col: 0
        }
    );
    assert!(search_primary(&buffer, "hab", true).is_empty());
}

#[test]
fn retained_text_search_maps_wide_and_combining_graphemes_to_cells() {
    let mut cells = vec![text_cell("A")];
    cells.extend(wide_text_cells("界"));
    cells.push(text_cell("e\u{301}"));
    cells.push(text_cell("Z"));
    let buffer = RetainedTextBuffer::new(5, vec![text_row(cells, false)]);

    let matches = search_primary(&buffer, "界e\u{301}", true);
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0].start,
        TerminalTextPoint {
            row: AbsRow(0),
            col: 1
        }
    );
    assert_eq!(
        matches[0].end,
        TerminalTextPoint {
            row: AbsRow(0),
            col: 3
        }
    );
    assert!(search_primary(&buffer, "\u{301}", true).is_empty());
}

#[test]
fn retained_text_search_skips_wide_spacer_heads_at_soft_wraps() {
    let mut first = "abcd"
        .chars()
        .map(|ch| text_cell(&ch.to_string()))
        .collect::<Vec<_>>();
    first.push(crate::vt::ScreenTextCell {
        wide: crate::vt::CellWide::SpacerHead,
        graphemes: Vec::new(),
    });
    let mut second = wide_text_cells("界").to_vec();
    second.extend("xyz".chars().map(|ch| text_cell(&ch.to_string())));
    let buffer = RetainedTextBuffer::new(5, vec![text_row(first, true), text_row(second, false)]);

    let matches = search_primary(&buffer, "d界", true);
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0].start,
        TerminalTextPoint {
            row: AbsRow(0),
            col: 3
        }
    );
    assert_eq!(
        matches[0].end,
        TerminalTextPoint {
            row: AbsRow(1),
            col: 1
        }
    );
}

#[test]
fn retained_text_word_motion_does_not_split_at_a_wide_spacer_head() {
    let mut first = "abcd"
        .chars()
        .map(|ch| text_cell(&ch.to_string()))
        .collect::<Vec<_>>();
    first.push(crate::vt::ScreenTextCell {
        wide: crate::vt::CellWide::SpacerHead,
        graphemes: Vec::new(),
    });
    let mut second = wide_text_cells("界").to_vec();
    second.extend("xyz".chars().map(|ch| text_cell(&ch.to_string())));
    let buffer = RetainedTextBuffer::new(5, vec![text_row(first, true), text_row(second, false)]);

    assert_eq!(
        buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextStart),
        None
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextEnd),
        Some(TerminalTextPoint {
            row: AbsRow(1),
            col: 4
        })
    );
}

#[test]
fn retained_text_search_is_literal_and_unicode_case_aware() {
    let buffer = RetainedTextBuffer::new(
        12,
        vec![text_row(
            "CAFÉ a.b    ".chars().map(|ch| text_cell(&ch.to_string())),
            false,
        )],
    );

    assert_eq!(search_primary(&buffer, "café", false).len(), 1);
    assert!(search_primary(&buffer, "café", true).is_empty());
    assert_eq!(search_primary(&buffer, "a.b", true).len(), 1);
    assert!(search_primary(&buffer, "a?b", true).is_empty());
}

#[test]
fn retained_text_word_motions_use_tmux_separators_across_rows() {
    let buffer = RetainedTextBuffer::new(
        6,
        vec![
            text_row("a_b.c ".chars().map(|ch| text_cell(&ch.to_string())), false),
            text_row(
                "\u{2014}d    ".chars().map(|ch| text_cell(&ch.to_string())),
                false,
            ),
        ],
    );

    assert_eq!(
        buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 3
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 3, TerminalWordMotion::NextStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 4
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 4, TerminalWordMotion::NextStart),
        Some(TerminalTextPoint {
            row: AbsRow(1),
            col: 0
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(1), 1, TerminalWordMotion::PreviousStart),
        Some(TerminalTextPoint {
            row: AbsRow(1),
            col: 0
        })
    );
}

#[test]
fn retained_text_big_word_motions_treat_only_whitespace_as_separators() {
    let buffer = RetainedTextBuffer::new(
        20,
        vec![text_row(
            "foo.bar baz qux/quux"
                .chars()
                .map(|ch| text_cell(&ch.to_string())),
            false,
        )],
    );

    // `W` skips punctuation-separated segments and lands on the next
    // whitespace-delimited run.
    assert_eq!(
        buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextBigStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 8
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 8, TerminalWordMotion::NextBigStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 12
        })
    );
    // `E` lands on the last character of the current/next run.
    assert_eq!(
        buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextBigEnd),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 6
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 6, TerminalWordMotion::NextBigEnd),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 10
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 12, TerminalWordMotion::NextBigEnd),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 19
        })
    );
    // `B` returns to the beginning of the previous run.
    assert_eq!(
        buffer.word_motion(AbsRow(0), 19, TerminalWordMotion::PreviousBigStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 12
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 12, TerminalWordMotion::PreviousBigStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 8
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 8, TerminalWordMotion::PreviousBigStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 0
        })
    );

    // Lowercase motions keep their punctuation-aware behavior.
    assert_eq!(
        buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 3
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 3, TerminalWordMotion::NextStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 4
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 4, TerminalWordMotion::PreviousStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 3
        })
    );
}

#[test]
fn retained_text_big_word_motions_cross_rows_and_blank_lines() {
    let buffer = RetainedTextBuffer::new(
        6,
        vec![
            text_row("a.b-c ".chars().map(|ch| text_cell(&ch.to_string())), false),
            text_row("      ".chars().map(|ch| text_cell(&ch.to_string())), false),
            text_row("d_e   ".chars().map(|ch| text_cell(&ch.to_string())), false),
        ],
    );

    assert_eq!(
        buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextBigStart),
        Some(TerminalTextPoint {
            row: AbsRow(2),
            col: 0
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(2), 0, TerminalWordMotion::PreviousBigStart),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 0
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextBigEnd),
        Some(TerminalTextPoint {
            row: AbsRow(0),
            col: 4
        })
    );
    assert_eq!(
        buffer.word_motion(AbsRow(0), 4, TerminalWordMotion::NextBigEnd),
        Some(TerminalTextPoint {
            row: AbsRow(2),
            col: 2
        })
    );
}

#[test]
fn live_terminal_word_motion_expands_across_long_blank_history() {
    let mut terminal = crate::vt::Terminal::new(10, 3, 200);
    terminal.write(b"origin\r\n");
    for _ in 0..80 {
        terminal.write(b"\r\n");
    }
    let last_row = ScreenRow(terminal.total_rows().saturating_sub(1));
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));

    assert_eq!(
        pane.word_motion_target(last_row, 0, TerminalWordMotion::PreviousStart),
        Some(TerminalTextPoint {
            row: ScreenRow(0),
            col: 0,
        })
    );
}

#[test]
fn live_terminal_word_end_expands_through_a_long_soft_wrap() {
    let mut terminal = crate::vt::Terminal::new(2, 3, 200);
    let word = "a".repeat(132);
    terminal.write(word.as_bytes());
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
    let text_match = pane
        .search_text_window(
            &word,
            true,
            TerminalSearchDirection::Forward,
            TerminalTextPoint {
                row: ScreenRow(0),
                col: 0,
            },
            None,
            1,
        )
        .matches[0];

    assert_eq!(
        pane.word_motion_target(
            text_match.start.row,
            text_match.start.col,
            TerminalWordMotion::NextEnd,
        ),
        Some(text_match.end)
    );
}

#[test]
fn live_terminal_word_end_expands_through_a_long_wide_soft_wrap() {
    let mut terminal = crate::vt::Terminal::new(2, 3, 200);
    let word = "界".repeat(66);
    terminal.write(word.as_bytes());
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
    let text_match = pane
        .search_text_window(
            &word,
            true,
            TerminalSearchDirection::Forward,
            TerminalTextPoint {
                row: ScreenRow(0),
                col: 0,
            },
            None,
            1,
        )
        .matches[0];

    // The word end sits on the head cell of the final wide glyph, past the
    // initial read window, so the window has to expand to reach it.
    assert_eq!(
        pane.word_motion_target(
            text_match.start.row,
            text_match.start.col,
            TerminalWordMotion::NextEnd,
        ),
        Some(TerminalTextPoint {
            row: text_match.end.row,
            col: 0,
        })
    );
}

fn current_palette_color(pane: &GhosttyPaneTerminal, index: u8) -> crate::vt::RgbColor {
    let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
    let GhosttyPaneCore {
        terminal,
        render_state,
        ..
    } = &mut *core;
    render_state.update(terminal);
    render_state.colors().palette[usize::from(index)]
}

fn expected_osc_rgb_response(command: &str, color: crate::vt::RgbColor) -> Bytes {
    let r = u16::from(color.r) * 257;
    let g = u16::from(color.g) * 257;
    let b = u16::from(color.b) * 257;
    Bytes::from(format!("\x1b]{command};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"))
}

#[test]
fn process_pty_bytes_reports_latest_working_directory_report() {
    let terminal = crate::vt::Terminal::new(80, 24, 100);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let partial = pane.process_pty_bytes(pane_id, 0, b"\x1b]7;file:///tmp/shepr%20");
    assert_eq!(partial.reported_cwd, None);

    let completed = pane.process_pty_bytes(pane_id, 0, b"repo\x07");
    assert_eq!(
        completed.reported_cwd,
        Some(std::path::PathBuf::from("/tmp/shepr repo"))
    );

    let latest = pane.process_pty_bytes(
        pane_id,
        0,
        b"\x1b]9;9;/tmp/conemu\x1b\\\x1b]1337;CurrentDir=/tmp/iterm2\x1b\\",
    );
    assert_eq!(
        latest.reported_cwd,
        Some(std::path::PathBuf::from("/tmp/iterm2"))
    );
}

#[test]
fn process_pty_bytes_reports_only_completed_title_changes() {
    let terminal = crate::vt::Terminal::new(80, 24, 100);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    assert!(
        !pane
            .process_pty_bytes(pane_id, 0, b"\x1b]0;buil")
            .terminal_title_changed
    );
    assert!(
        pane.process_pty_bytes(pane_id, 0, b"ding\x07")
            .terminal_title_changed
    );
    assert!(
        !pane
            .process_pty_bytes(pane_id, 0, b"\x1b]2;building\x07")
            .terminal_title_changed
    );
    assert!(
        pane.process_pty_bytes(pane_id, 0, b"\x1b]2;done\x07")
            .terminal_title_changed
    );
}

#[test]
fn process_pty_bytes_surfaces_clipboard_writes_without_other_results() {
    let terminal = crate::vt::Terminal::new(80, 24, 100);
    let pane = GhosttyPaneTerminal::new(terminal);

    let result =
        pane.process_pty_bytes(PaneId::from_raw(1), 0, b"output\x1b]52;c;Y2xpcGJvYXJk\x07");

    assert!(result.request_render);
    assert_eq!(result.render_delay, None);
    assert_eq!(result.clipboard_writes, vec![b"clipboard".to_vec()]);
    assert_eq!(result.reported_cwd, None);
    assert!(result.terminal_responses.is_empty());
}

#[test]
fn seeded_history_clipboard_write_does_not_leak_into_live_output() {
    let terminal = crate::vt::Terminal::new(80, 24, 100);
    let pane = GhosttyPaneTerminal::new(terminal);
    pane.seed_history_ansi("\x1b]52;c;c3RhbGU=\x07");

    let result = pane.process_pty_bytes(PaneId::from_raw(1), 0, b"live output");

    assert!(result.clipboard_writes.is_empty());
}

#[test]
fn seeded_history_pwd_does_not_leak_into_live_output() {
    let terminal = crate::vt::Terminal::new(80, 24, 100);
    let pane = GhosttyPaneTerminal::new(terminal);
    pane.seed_history_ansi("\x1b]7;file:///tmp/restored\x07");

    let result = pane.process_pty_bytes(PaneId::from_raw(1), 0, b"live output");

    assert_eq!(result.reported_cwd, None);
}

fn expected_xtgettcap_response(cap_hex: &str, value: Option<&[u8]>) -> Bytes {
    let mut response = format!("\x1bP1+r{cap_hex}").into_bytes();
    if let Some(value) = value {
        response.push(b'=');
        append_upper_hex(value, &mut response);
    }
    response.extend_from_slice(b"\x1b\\");
    Bytes::from(response)
}

fn append_upper_hex(bytes: &[u8], output: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in bytes {
        output.push(HEX[usize::from(byte >> 4)]);
        output.push(HEX[usize::from(byte & 0x0f)]);
    }
}

#[test]
fn decscusr_cursor_shape_preserves_blinking_variants() {
    assert_eq!(
        decscusr_cursor_shape(crate::vt::CursorVisualStyle::Block, true),
        crate::protocol::CursorShapeParam::BlinkingBlock
    );
    assert_eq!(
        decscusr_cursor_shape(crate::vt::CursorVisualStyle::Block, false),
        crate::protocol::CursorShapeParam::SteadyBlock
    );
    assert_eq!(
        decscusr_cursor_shape(crate::vt::CursorVisualStyle::Underline, true),
        crate::protocol::CursorShapeParam::BlinkingUnderline
    );
    assert_eq!(
        decscusr_cursor_shape(crate::vt::CursorVisualStyle::Underline, false),
        crate::protocol::CursorShapeParam::SteadyUnderline
    );
    assert_eq!(
        decscusr_cursor_shape(crate::vt::CursorVisualStyle::Bar, true),
        crate::protocol::CursorShapeParam::BlinkingBar
    );
    assert_eq!(
        decscusr_cursor_shape(crate::vt::CursorVisualStyle::Bar, false),
        crate::protocol::CursorShapeParam::SteadyBar
    );
    assert_eq!(
        decscusr_cursor_shape(crate::vt::CursorVisualStyle::BlockHollow, false),
        crate::protocol::CursorShapeParam::SteadyBlock
    );
}

#[test]
fn cursor_state_uses_terminal_default_until_child_sets_shape() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    assert_eq!(
        pane.cursor_state().expect("test precondition").shape,
        crate::protocol::CursorShapeParam::Default
    );

    pane.process_pty_bytes(pane_id, 0, b"\x1b[6 q");

    assert_eq!(
        pane.cursor_state().expect("test precondition").shape,
        crate::protocol::CursorShapeParam::SteadyBar
    );
}

#[test]
fn cursor_state_returns_terminal_default_after_decscusr_reset() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.process_pty_bytes(pane_id, 0, b"\x1b[2 q");
    assert_eq!(
        pane.cursor_state().expect("test precondition").shape,
        crate::protocol::CursorShapeParam::SteadyBlock
    );

    pane.process_pty_bytes(pane_id, 0, b"\x1b[0 q");

    assert_eq!(
        pane.cursor_state().expect("test precondition").shape,
        crate::protocol::CursorShapeParam::Default
    );
}

#[test]
fn cursor_shape_tracker_handles_split_decscusr_sequences() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.process_pty_bytes(pane_id, 0, b"\x1b[");
    pane.process_pty_bytes(pane_id, 0, b"5 ");
    pane.process_pty_bytes(pane_id, 0, b"q");

    assert_eq!(
        pane.cursor_state().expect("test precondition").shape,
        crate::protocol::CursorShapeParam::BlinkingBar
    );
}

#[test]
fn cursor_state_reports_the_live_position() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.process_pty_bytes(pane_id, 0, b"x");
    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[6;21H");

    assert_eq!(result.render_delay, None);
    assert_eq!(
        pane.cursor_state()
            .map(|cursor| (cursor.x, cursor.y, cursor.visible)),
        Some((20, 5, true))
    );
}

#[test]
fn cursor_state_returns_terminal_default_after_ris() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.process_pty_bytes(pane_id, 0, b"\x1b[4 q");
    assert_eq!(
        pane.cursor_state().expect("test precondition").shape,
        crate::protocol::CursorShapeParam::SteadyUnderline
    );
    pane.process_pty_bytes(pane_id, 0, b"\x1bc");

    assert_eq!(
        pane.cursor_state().expect("test precondition").shape,
        crate::protocol::CursorShapeParam::Default
    );
}

/// The host theme is applied to the core directly, never written through
/// the child's parser: a CSI the child is halfway through must survive.
#[test]
fn host_theme_change_does_not_split_a_partial_child_sequence() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.process_pty_bytes(pane_id, 0, b"\x1b[3");
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc,
        }),
        background: None,
        ..Default::default()
    });
    pane.process_pty_bytes(pane_id, 0, b"1mred");

    assert_eq!(pane.visible_text(), "red\n");
}

/// A render that force-ends a timed-out synchronized update must not
/// lose the frame's effects: the flush entry point hands them over, and
/// a clipboard write is no longer thrown away by the next read.
#[test]
fn timed_out_synchronized_update_effects_survive_a_render_flush() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let begin = pane.process_pty_bytes(
        pane_id,
        0,
        b"\x1b[?2026h\x1b]52;c;aGk=\x07\x1b]2;framed\x07\x1b[6n",
    );
    assert!(begin.terminal_responses.is_empty());
    assert!(begin.clipboard_writes.is_empty());
    let deadline = crate::vt::lock_terminal_core(&pane.core)
        .expect("test precondition")
        .terminal
        .synchronized_output_deadline()
        .expect("test precondition");
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    // A render flushes the frame; nothing reads its effects yet.
    assert!(matches!(
        pane.collect_dirty_patch(20, 5),
        TerminalDirtyPatchOutcome::Patch(_) | TerminalDirtyPatchOutcome::Clean
    ));
    // A resize in between must not take the queued reply with it.
    assert!(
        pane.resize(crate::core::geometry::PaneGeometry::new(20, 5, 0, 0))
            .is_empty()
    );

    let flushed = pane.flush_expired_synchronized_output(pane_id, 0);
    assert!(!flushed.request_render, "the render already flushed it");
    assert_eq!(
        flushed.terminal_responses,
        vec![Bytes::from_static(b"\x1b[1;1R")]
    );
    assert_eq!(flushed.clipboard_writes, vec![b"hi".to_vec()]);
    assert!(flushed.terminal_title_changed);
    assert_eq!(pane.terminal_title().as_deref(), Some("framed"));

    let next = pane.process_pty_bytes(pane_id, 0, b"x");
    assert!(next.terminal_responses.is_empty());
    assert!(next.clipboard_writes.is_empty());
}

#[test]
fn flush_entry_point_ends_an_expired_update_itself() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let begin = pane.process_pty_bytes(pane_id, 0, b"\x1b[?2026h\x1b[5n");
    assert!(begin.render_delay.is_some());
    assert!(
        !pane
            .flush_expired_synchronized_output(pane_id, 0)
            .request_render
    );
    let deadline = crate::vt::lock_terminal_core(&pane.core)
        .expect("test precondition")
        .terminal
        .synchronized_output_deadline()
        .expect("test precondition");
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let flushed = pane.flush_expired_synchronized_output(pane_id, 0);
    assert!(flushed.request_render);
    assert_eq!(
        flushed.terminal_responses,
        vec![Bytes::from_static(b"\x1b[0n")]
    );
    assert!(!pane.synchronized_output_state().0);
}

#[test]
fn host_terminal_theme_restore_probe_skips_when_no_transient_override() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");

    assert!(!should_probe_host_terminal_theme_restore(&core));
}

#[test]
fn host_terminal_theme_restore_probe_skips_when_host_theme_unknown() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.transient_default_color_owner_pgid = Some(42);
    }
    let core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");

    assert!(!should_probe_host_terminal_theme_restore(&core));
}

#[test]
fn host_terminal_theme_restore_probe_skips_on_alternate_screen() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[?1049h");
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.transient_default_color_owner_pgid = Some(42);
        core.host_terminal_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };
    }
    let core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");

    assert!(!should_probe_host_terminal_theme_restore(&core));
}

#[test]
fn host_terminal_theme_restore_probe_runs_when_restore_is_pending() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.transient_default_color_owner_pgid = Some(42);
        core.host_terminal_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };
    }
    let core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");

    assert!(should_probe_host_terminal_theme_restore(&core));
}

#[test]
fn ghostty_render_can_suppress_cursor_position() {
    let mut first_terminal = crate::vt::Terminal::new(20, 5, 0);
    first_terminal.write(b"left");
    let first = GhosttyPaneTerminal::new(first_terminal);

    let mut second_terminal = crate::vt::Terminal::new(20, 5, 0);
    second_terminal.write(b"r\r\nb");
    let second = GhosttyPaneTerminal::new(second_terminal);

    let backend = ratatui::backend::TestBackend::new(40, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| {
            first.render(frame, Rect::new(0, 0, 20, 5), true);
            second.render(frame, Rect::new(20, 0, 20, 5), false);
        })
        .expect("test precondition");

    terminal.backend_mut().assert_cursor_position((4, 0));
}

#[test]
fn ghostty_keyboard_protocol_tracks_live_terminal_flags() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[>3u");
    let pane = GhosttyPaneTerminal::new(terminal);

    assert_eq!(
        pane.keyboard_protocol(),
        Some(crate::input::KeyboardProtocol::Kitty { flags: 3 })
    );
}

#[test]
fn ghostty_plain_text_chars_still_encode_as_text() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);

    let encoded = pane.encode_terminal_key(
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::empty(),
        ),
        crate::input::KeyboardProtocol::Legacy,
    );

    assert_eq!(encoded, b"a");
}

#[test]
fn ghostty_backtab_preserves_shift_across_keyboard_protocols() {
    for (kitty_flags, expected) in [
        (None, b"\x1b[Z".as_slice()),
        (Some(1), b"\x1b[9;2u".as_slice()),
    ] {
        let mut terminal = crate::vt::Terminal::new(80, 24, 0);
        if let Some(flags) = kitty_flags {
            terminal.write(format!("\x1b[>{flags}u").as_bytes());
        }
        let pane = GhosttyPaneTerminal::new(terminal);
        let protocol = pane.keyboard_protocol().expect("test precondition");

        for modifiers in [
            crossterm::event::KeyModifiers::empty(),
            crossterm::event::KeyModifiers::SHIFT,
        ] {
            let encoded = pane.encode_terminal_key(
                crate::input::TerminalKey::new(crossterm::event::KeyCode::BackTab, modifiers),
                protocol,
            );
            assert_eq!(encoded, expected, "backtab with modifiers {modifiers:?}");
        }
    }

    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let encoded = pane.encode_terminal_key(
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::empty(),
        ),
        crate::input::KeyboardProtocol::Legacy,
    );
    assert_eq!(encoded, b"\t");
}

#[test]
fn ghostty_ctrl_tab_matches_the_pane_keyboard_protocol() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let legacy = GhosttyPaneTerminal::new(terminal);
    let key = crate::input::TerminalKey::new(
        crossterm::event::KeyCode::Tab,
        crossterm::event::KeyModifiers::CONTROL,
    );

    assert_eq!(
        legacy.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy),
        b"\t"
    );

    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[>3u");
    let kitty = GhosttyPaneTerminal::new(terminal);
    // Flags 3 include REPORT_EVENT_TYPES; shepr's encoder always spells out
    // the press event type (`:1`), which the protocol allows.
    assert_eq!(
        kitty.encode_terminal_key(key, crate::input::KeyboardProtocol::Kitty { flags: 3 }),
        b"\x1b[9;5:1u"
    );
}

#[test]
fn ghostty_legacy_modified_enter_is_shell_compatible() {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let protocol = crate::input::KeyboardProtocol::Legacy;

    for modifiers in [
        KeyModifiers::empty(),
        KeyModifiers::SHIFT,
        KeyModifiers::CONTROL,
        KeyModifiers::SUPER,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        KeyModifiers::ALT | KeyModifiers::SHIFT,
        KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER,
    ] {
        let key = crate::input::TerminalKey::new(KeyCode::Enter, modifiers);
        let expected = if modifiers.contains(KeyModifiers::ALT) {
            b"\x1b\r".as_slice()
        } else {
            b"\r".as_slice()
        };
        for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
            assert_eq!(
                pane.encode_terminal_key(key.clone().with_kind(kind), protocol),
                expected,
                "{modifiers:?} {kind:?}"
            );
        }
        assert_eq!(
            pane.encode_terminal_key(key.clone().with_repeat_count(3), protocol),
            expected.repeat(3),
            "{modifiers:?} grouped repeat"
        );
        assert!(
            pane.encode_terminal_key(key.with_kind(KeyEventKind::Release), protocol)
                .is_empty(),
            "{modifiers:?} release"
        );
    }
}

#[test]
fn ghostty_modified_enter_tracks_live_protocol_negotiation() {
    use crossterm::event::{KeyCode, KeyModifiers};

    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    let legacy = ["\r", "\r", "\r", "\x1b\r"];
    let mode_one = ["\x1b[27;2;13~", "\x1b[27;5;13~", "\x1b[27;9;13~", "\x1b\r"];
    let mode_two = [
        "\x1b[27;2;13~",
        "\x1b[27;5;13~",
        "\x1b[27;9;13~",
        "\x1b[27;3;13~",
    ];
    let kitty = ["\x1b[13;2u", "\x1b[13;5u", "\x1b[13;9u", "\x1b[13;3u"];

    for (sequence, expected) in [
        ("", legacy),
        ("\x1b[>4;1m", mode_one),
        ("\x1b[>4;2m", mode_two),
        ("\x1b[>4n", legacy),
        ("\x1b[>4;2m", mode_two),
        ("\x1b[>4;0m", legacy),
        ("\x1b[>5u", kitty),
        ("\x1b[<u", legacy),
        ("\x1b[>4;2m\x1b[>1u", kitty),
        ("\x1b[<u", mode_two),
        ("\x1b[>4;0m", legacy),
        ("\x1b[>4;1m", mode_one),
        ("\x1b[>04n", legacy),
        ("\x1b[>4;2m", mode_two),
        ("\x1b[>4", mode_two),
        ("n", legacy),
    ] {
        pane.process_pty_bytes(pane_id, 0, sequence.as_bytes());
        for (modifiers, expected) in [
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
            KeyModifiers::ALT,
        ]
        .into_iter()
        .zip(expected)
        {
            let key = crate::input::TerminalKey::new(KeyCode::Enter, modifiers);
            assert_eq!(
                pane.encode_terminal_key(key, crate::input::KeyboardProtocol::Legacy),
                expected.as_bytes(),
                "{modifiers:?} after {sequence:?}"
            );
        }
    }
}

#[test]
fn ghostty_modified_enter_respects_existing_terminal_mode() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[>4;2m");
    let pane = GhosttyPaneTerminal::new(terminal);
    let key = crate::input::TerminalKey::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::SHIFT,
    );

    assert_eq!(
        pane.encode_terminal_key(key, crate::input::KeyboardProtocol::Legacy),
        b"\x1b[27;2;13~"
    );
}

#[test]
fn ghostty_enter_backspace_release_in_legacy_pane_emits_nothing() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);

    for code in [
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyCode::Backspace,
    ] {
        let press = pane.encode_terminal_key(
            crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty()),
            crate::input::KeyboardProtocol::Legacy,
        );
        let release = pane.encode_terminal_key(
            crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty())
                .with_kind(crossterm::event::KeyEventKind::Release),
            crate::input::KeyboardProtocol::Legacy,
        );
        assert!(!press.is_empty(), "{code:?} press should emit bytes");
        assert!(
            release.is_empty(),
            "{code:?} release should emit nothing in a legacy pane, got {release:?}"
        );
    }
}

#[test]
fn ghostty_report_event_pane_keeps_basic_compatibility_keys_legacy() {
    // Push kitty flags including REPORT_EVENT_TYPES (0b10) + DISAMBIGUATE (0b1).
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[>3u");
    let pane = GhosttyPaneTerminal::new(terminal);

    for (code, expected) in [
        (crossterm::event::KeyCode::Enter, b"\r".as_slice()),
        (crossterm::event::KeyCode::Backspace, b"\x7f".as_slice()),
    ] {
        let press = pane.encode_terminal_key(
            crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty()),
            pane.keyboard_protocol().expect("test precondition"),
        );
        assert_eq!(
            press, expected,
            "{code:?} press should stay legacy-compatible without REPORT_ALL_KEYS"
        );

        let release = pane.encode_terminal_key(
            crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty())
                .with_kind(crossterm::event::KeyEventKind::Release),
            pane.keyboard_protocol().expect("test precondition"),
        );
        assert!(
            release.is_empty(),
            "{code:?} release should not fall back to legacy bytes, got {release:?}"
        );
    }
}

#[test]
fn ghostty_char_keys_still_use_shepr_encoding() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[>1u");
    let pane = GhosttyPaneTerminal::new(terminal);

    let encoded = pane.encode_terminal_key(
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        ),
        crate::input::KeyboardProtocol::Legacy,
    );

    assert_eq!(encoded, vec![1]);
}

#[test]
fn ghostty_key_encoding_honors_application_cursor_mode() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal
        .mode_set(crate::vt::MODE_APPLICATION_CURSOR_KEYS, true)
        .expect("test precondition");
    let pane = GhosttyPaneTerminal::new(terminal);

    let encoded = pane.encode_terminal_key(
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::empty(),
        ),
        crate::input::KeyboardProtocol::Legacy,
    );

    assert_eq!(encoded, b"\x1bOA");
}

#[test]
fn grouped_key_repeats_expand_at_the_destination() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let key = crate::input::TerminalKey::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::empty(),
    )
    .with_repeat_count(3);

    assert_eq!(
        pane.encode_terminal_key(key, crate::input::KeyboardProtocol::Legacy),
        b"xxx"
    );

    let shifted = crate::input::TerminalKey::new(
        crossterm::event::KeyCode::Char('/'),
        crossterm::event::KeyModifiers::SHIFT,
    )
    .with_generated_text(Some("/".to_owned()))
    .with_repeat_count(3);
    let legacy_expected = b"///".as_slice();
    assert_eq!(
        pane.encode_terminal_key(shifted.clone(), crate::input::KeyboardProtocol::Legacy,),
        legacy_expected
    );
    // Flags 15 (disambiguate + event types + alternate keys + report all
    // keys) reports every key as CSI u but, without flag 16
    // (REPORT_ASSOCIATED_TEXT), carries no committed text: the repeat
    // still has to expand to three identical CSI u sequences rather than
    // three literal slashes.
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[>15u");
    let pane = GhosttyPaneTerminal::new(terminal);
    let kitty_protocol = crate::input::KeyboardProtocol::Kitty { flags: 15 };
    let pressed =
        pane.encode_terminal_key_once(shifted.clone().with_repeat_count(1), kitty_protocol);
    assert!(
        !pressed.is_empty() && pressed != b"/",
        "flags 15 without REPORT_ASSOCIATED_TEXT should encode as CSI u, not plain text"
    );
    let repeated_key = shifted
        .clone()
        .with_repeat_count(1)
        .with_kind(crossterm::event::KeyEventKind::Repeat);
    let repeated = pane.encode_terminal_key_once(repeated_key, kitty_protocol);
    let mut expected = pressed;
    expected.extend_from_slice(&repeated);
    expected.extend_from_slice(&repeated);
    assert_eq!(pane.encode_terminal_key(shifted, kitty_protocol), expected);
}

#[test]
fn grouped_release_is_encoded_once() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[>11u");
    let pane = GhosttyPaneTerminal::new(terminal);
    let protocol = pane.keyboard_protocol().expect("test precondition");
    let release = crate::input::TerminalKey::new(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::empty(),
    )
    .with_kind(crossterm::event::KeyEventKind::Release);
    let expected = pane.encode_terminal_key(release.clone(), protocol);

    assert!(!expected.is_empty());
    let mut malformed_release = release;
    malformed_release.repeat_count = 3;
    assert_eq!(
        pane.encode_terminal_key(malformed_release, protocol),
        expected
    );
}

#[test]
fn ghostty_key_encoder_updates_after_terminal_mode_changes() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let before = pane.encode_terminal_key(
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::empty(),
        ),
        crate::input::KeyboardProtocol::Legacy,
    );
    assert_eq!(before, b"\x1b[A");

    pane.process_pty_bytes(pane_id, 0, b"\x1b[?1h");

    let after = pane.encode_terminal_key(
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::empty(),
        ),
        crate::input::KeyboardProtocol::Legacy,
    );
    assert_eq!(after, b"\x1bOA");
}

#[test]
fn ghostty_key_encoder_updates_after_kitty_flag_changes() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    let key = crate::input::TerminalKey::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
    );

    let before = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);
    pane.process_pty_bytes(pane_id, 0, b"\x1b[>1u");
    let after = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);

    assert_ne!(before, after);
    assert_eq!(after, b"\x1b[13;6u");
}

#[test]
fn ghostty_kitty_pane_encodes_shift_enter_as_csi_u() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.process_pty_bytes(pane_id, 0, b"\x1b[>5u");

    let key = crate::input::parse_terminal_key_sequence("\x1b[13;2u").expect("test precondition");
    let encoded = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);

    assert_eq!(
        pane.keyboard_protocol(),
        Some(crate::input::KeyboardProtocol::Kitty { flags: 5 })
    );
    assert_eq!(encoded, b"\x1b[13;2u");
}

#[test]
fn ghostty_modify_other_keys_mode_one_preserves_shift_enter() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let key = crate::input::parse_terminal_key_sequence("\x1b[13;2u").expect("test precondition");

    pane.seed_history_ansi("\x1b[>4;1m");
    assert_eq!(pane.modify_other_keys_level(), 1);
    let encoded = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);

    assert_eq!(encoded, b"\x1b[27;2;13~");
}

#[test]
fn ghostty_kitty_pane_encodes_parsed_legacy_alt_backspace_as_csi_u() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.process_pty_bytes(pane_id, 0, b"\x1b[>1u");

    let key = crate::input::parse_terminal_key_sequence("\x1b\x7f").expect("test precondition");
    let encoded = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);

    assert_eq!(encoded, b"\x1b[127;3u");
}

#[test]
fn ghostty_kitty_pane_preserves_legacy_ctrl_alt_letter() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.process_pty_bytes(pane_id, 0, b"\x1b[>5u");

    let mut events = crate::raw_input::parse_raw_input_bytes_sync(b"\x1b\x06");
    let crate::raw_input::RawInputEvent::Key(key) = events.remove(0) else {
        panic!("expected key event");
    };
    let encoded =
        pane.encode_terminal_key(key, pane.keyboard_protocol().expect("test precondition"));

    assert_eq!(encoded, b"\x1b[102;7u");
}

#[test]
fn ghostty_pane_characterizes_ctrl_backspace_encoding() {
    let legacy = GhosttyPaneTerminal::new(crate::vt::Terminal::new(80, 24, 0));

    let ctrl_backspace = crate::input::TerminalKey::new(
        crossterm::event::KeyCode::Backspace,
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(
        legacy.encode_terminal_key(
            ctrl_backspace.clone(),
            crate::input::KeyboardProtocol::Legacy
        ),
        b"\x08"
    );

    let plain_backspace = crate::input::TerminalKey::new(
        crossterm::event::KeyCode::Backspace,
        crossterm::event::KeyModifiers::empty(),
    );
    assert_eq!(
        legacy.encode_terminal_key(plain_backspace, crate::input::KeyboardProtocol::Legacy),
        b"\x7f"
    );

    let kitty = GhosttyPaneTerminal::new(crate::vt::Terminal::new(80, 24, 0));
    let pane_id = PaneId::from_raw(1);
    kitty.process_pty_bytes(pane_id, 0, b"\x1b[>1u");

    assert_eq!(
        kitty.encode_terminal_key(ctrl_backspace, crate::input::KeyboardProtocol::Legacy),
        b"\x1b[127;5u"
    );
}

#[test]
fn ghostty_key_encoders_are_isolated_per_pane() {
    let first = GhosttyPaneTerminal::new(crate::vt::Terminal::new(80, 24, 0));
    let second = GhosttyPaneTerminal::new(crate::vt::Terminal::new(80, 24, 0));

    first.process_pty_bytes(PaneId::from_raw(1), 0, b"\x1b[?1h");

    let first_encoded = first.encode_terminal_key(
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::empty(),
        ),
        crate::input::KeyboardProtocol::Legacy,
    );
    let second_encoded = second.encode_terminal_key(
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::empty(),
        ),
        crate::input::KeyboardProtocol::Legacy,
    );

    assert_eq!(first_encoded, b"\x1bOA");
    assert_eq!(second_encoded, b"\x1b[A");
}

#[test]
fn ghostty_mouse_button_encoding_uses_live_terminal_state() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[?1000h\x1b[?1006h");
    let pane = GhosttyPaneTerminal::new(terminal);

    let encoded = pane.encode_mouse_button(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        crate::input::mouse::Position::Cell { column: 11, row: 9 },
        crossterm::event::KeyModifiers::empty(),
    );

    assert_eq!(encoded.as_deref(), Some(&b"\x1b[<0;12;10m"[..]));
}

#[test]
fn ghostty_mouse_drag_encoding_uses_motion_reporting_state() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[?1002h\x1b[?1006h");
    let pane = GhosttyPaneTerminal::new(terminal);

    let encoded = pane.encode_mouse_button(
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        crate::input::mouse::Position::Cell { column: 4, row: 6 },
        crossterm::event::KeyModifiers::SHIFT,
    );

    assert_eq!(encoded.as_deref(), Some(&b"\x1b[<36;5;7M"[..]));
}

#[test]
fn ghostty_mouse_drag_without_motion_reporting_is_not_forwarded() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[?1000h\x1b[?1006h");
    let pane = GhosttyPaneTerminal::new(terminal);

    let encoded = pane.encode_mouse_button(
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        crate::input::mouse::Position::Cell { column: 4, row: 6 },
        crossterm::event::KeyModifiers::empty(),
    );

    assert_eq!(encoded, None);
}

#[test]
fn ghostty_mouse_moved_encoding_uses_any_motion_state() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[?1003h\x1b[?1006h");
    let pane = GhosttyPaneTerminal::new(terminal);

    let encoded = pane.encode_mouse_motion(
        crossterm::event::MouseEventKind::Moved,
        crate::input::mouse::Position::Cell { column: 4, row: 6 },
        crossterm::event::KeyModifiers::empty(),
    );

    assert_eq!(encoded.as_deref(), Some(&b"\x1b[<35;5;7M"[..]));
}

#[test]
fn ghostty_mouse_sgr_pixels_preserves_exact_and_maps_cell_input_to_pixels() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.resize(crate::core::geometry::PaneGeometry::new(80, 24, 10, 20));
    terminal.write(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
    let pane = GhosttyPaneTerminal::new(terminal);

    let exact = pane.encode_mouse_motion(
        crossterm::event::MouseEventKind::Moved,
        crate::input::mouse::Position::Pixels { x: 48, y: 139 },
        crossterm::event::KeyModifiers::empty(),
    );
    let from_cell = pane.encode_mouse_motion(
        crossterm::event::MouseEventKind::Moved,
        crate::input::mouse::Position::Cell { column: 4, row: 6 },
        crossterm::event::KeyModifiers::empty(),
    );

    assert_eq!(exact.as_deref(), Some(&b"\x1b[<35;48;139M"[..]));
    // Column 4 at 10 px per cell starts at pixel 41; row 6 at 20 px at 121.
    assert_eq!(from_cell.as_deref(), Some(&b"\x1b[<35;41;121M"[..]));
}

#[test]
fn ghostty_mouse_sgr_pixels_without_pixel_geometry_sends_cells() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.resize(crate::core::geometry::PaneGeometry::new(80, 24, 0, 0));
    terminal.write(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
    let pane = GhosttyPaneTerminal::new(terminal);

    let encoded = pane.encode_mouse_motion(
        crossterm::event::MouseEventKind::Moved,
        crate::input::mouse::Position::Cell { column: 4, row: 6 },
        crossterm::event::KeyModifiers::empty(),
    );

    assert_eq!(encoded.as_deref(), Some(&b"\x1b[<35;5;7M"[..]));
}

#[test]
fn ghostty_normalize_buffer_symbol_prefers_grapheme_width_when_metadata_disagrees() {
    const WIDE_GRAPHEME: &str = "\u{1F642}";
    const FLAG_GRAPHEME: &str = "\u{1F1E7}\u{1F1F7}";
    const FAMILY_GRAPHEME: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    const VS16_GRAPHEME: &str = "\u{26A0}\u{FE0F}";
    const EMOJI_GRAPHEME: &str = "\u{1F4B3}";

    assert_eq!(
        ghostty_normalize_buffer_symbol(WIDE_GRAPHEME, crate::vt::CellWide::Wide),
        WIDE_GRAPHEME
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol("a", crate::vt::CellWide::Wide),
        "  "
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol(FLAG_GRAPHEME, crate::vt::CellWide::Wide),
        FLAG_GRAPHEME
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol(FAMILY_GRAPHEME, crate::vt::CellWide::Wide),
        FAMILY_GRAPHEME
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol("⌨\u{FE0F}", crate::vt::CellWide::Narrow),
        "⌨\u{FE0F}"
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol(VS16_GRAPHEME, crate::vt::CellWide::Narrow),
        VS16_GRAPHEME
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol(EMOJI_GRAPHEME, crate::vt::CellWide::Narrow),
        EMOJI_GRAPHEME
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol(" ", crate::vt::CellWide::SpacerTail),
        ""
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol("xx", crate::vt::CellWide::SpacerHead),
        " "
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol("ｶ\u{ff9e}", crate::vt::CellWide::Wide),
        "ｶ\u{ff9e}"
    );
    assert_eq!(
        ghostty_normalize_buffer_symbol("ﾊ\u{ff9f}", crate::vt::CellWide::Wide),
        "ﾊ\u{ff9f}"
    );
}

fn render_cells_to_symbols(
    terminal: &mut crate::vt::Terminal,
) -> Vec<(crate::vt::CellWide, String)> {
    let mut render_state = crate::vt::RenderState::new();
    render_state.update(terminal);

    let mut symbol_scratch = String::new();
    let mut out = Vec::new();

    if let Some(row) = render_state.iter_rows().next() {
        for cells in row.cells() {
            let wide = cells.wide();
            let symbol =
                ghostty_buffer_symbol_into(&cells, wide, false, &mut symbol_scratch).to_string();
            out.push((wide, symbol));
        }
    }

    out
}

// The core lays out one codepoint per cell group (no grapheme clustering):
// zero-width joiners and selectors attach to the preceding cell, so the
// rendered cells still spell out the original text in order.
#[test]
fn multi_codepoint_emoji_render_without_losing_text() {
    for text in [
        "\u{1F1E7}\u{1F1F7}",
        "\u{1F468}\u{200d}\u{1F469}\u{200d}\u{1F467}",
        "\u{26A0}\u{fe0f}",
    ] {
        let mut terminal = crate::vt::Terminal::new(40, 1, 0);
        terminal.write(text.as_bytes());

        let cells = render_cells_to_symbols(&mut terminal);
        let rendered: String = cells.iter().map(|(_, symbol)| symbol.as_str()).collect();

        assert_eq!(rendered.trim_end(), text, "{cells:?}");
    }
}

#[test]
fn halfwidth_katakana_voiced_marks_render() {
    let mut terminal = crate::vt::Terminal::new(40, 1, 0);
    terminal.write("ｱｲｳｴｵ ｶﾞｷﾞｸﾞｹﾞｺﾞ ﾊﾟﾋﾟﾌﾟﾍﾟﾎﾟ".as_bytes());

    let cells = render_cells_to_symbols(&mut terminal);
    let rendered: String = cells.iter().map(|(_, symbol)| symbol.as_str()).collect();

    assert!(
        rendered.contains("ｱｲｳｴｵ ｶﾞｷﾞｸﾞｹﾞｺﾞ ﾊﾟﾋﾟﾌﾟﾍﾟﾎﾟ"),
        "expected halfwidth katakana with voiced marks to survive, got {cells:?}"
    );
}

#[test]
fn render_keeps_halfwidth_katakana_and_voiced_mark_in_their_own_cells() {
    let mut terminal = crate::vt::Terminal::new(20, 1, 0);
    terminal.write("ｶﾞZ".as_bytes());
    let pane = GhosttyPaneTerminal::new(terminal);

    let backend = ratatui::backend::TestBackend::new(20, 1);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 1), false))
        .expect("test precondition");
    let buffer = terminal.backend().buffer();

    // The halfwidth voiced mark is a spacing character one cell wide.
    assert_eq!(buffer[(0, 0)].symbol(), "ｶ");
    assert_eq!(buffer[(1, 0)].symbol(), "\u{ff9e}");
    assert_eq!(buffer[(2, 0)].symbol(), "Z");
}

#[test]
fn pane_scrollback_controls_round_trip_and_clamp_without_ui_interference() {
    let mut terminal = crate::vt::Terminal::new(80, 3, 100);
    write_numbered_lines(&mut terminal, 1000);
    let pane = GhosttyPaneTerminal::new(terminal);

    let before = pane.scroll_metrics().expect("scroll metrics before scroll");
    assert!(before.max_offset_from_bottom > 0);
    assert_eq!(before.offset_from_bottom, 0);

    for offset in [
        0,
        before.max_offset_from_bottom / 2,
        before.max_offset_from_bottom,
        usize::MAX,
    ] {
        pane.set_scroll_offset_from_bottom(offset);
        let after = pane.scroll_metrics().expect("scroll metrics after scroll");
        assert_eq!(
            after.offset_from_bottom,
            offset.min(after.max_offset_from_bottom)
        );
    }

    assert!(pane.visible_text().contains("000000"));
}

#[test]
fn empty_or_short_resize_keeps_following_bottom_when_output_creates_scrollback() {
    for initial in [b"".as_slice(), b"seed\r\n".as_slice()] {
        let mut terminal = crate::vt::Terminal::new(10, 3, 100);
        terminal.write(initial);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.resize(crate::core::geometry::PaneGeometry::new(10, 3, 0, 0));
        pane.process_pty_bytes(
            pane_id,
            0,
            b"000000\r\n000001\r\n000002\r\n000003\r\n000004",
        );

        let metrics = pane.scroll_metrics().expect("scroll metrics after output");
        assert_eq!(metrics.offset_from_bottom, 0);
        assert!(pane.visible_text().contains("000004"));
    }
}

#[test]
fn resize_that_removes_scrollback_restores_live_follow() {
    let mut terminal = crate::vt::Terminal::new(10, 3, 100);
    terminal.write(b"000000\r\n000001\r\n000002\r\n000003\r\n000004");
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.set_scroll_offset_from_bottom(1);
    pane.resize(crate::core::geometry::PaneGeometry::new(10, 5, 0, 0));
    let resized = pane.scroll_metrics().expect("scroll metrics after resize");
    assert_eq!(resized.max_offset_from_bottom, 0);

    pane.process_pty_bytes(pane_id, 0, b"\r\n000005\r\n000006");

    let metrics = pane.scroll_metrics().expect("scroll metrics after output");
    assert_eq!(metrics.offset_from_bottom, 0);
    assert!(pane.visible_text().contains("000006"));
}

#[test]
fn detection_text_stays_at_bottom_when_viewport_is_scrolled() {
    let mut terminal = crate::vt::Terminal::new(80, 3, 100);
    write_numbered_lines(&mut terminal, 10);
    let pane = GhosttyPaneTerminal::new(terminal);

    let bottom_snapshot = pane.detection_text();
    assert_eq!(bottom_snapshot, pane.recent_text(3));
    assert!(bottom_snapshot.contains("000009"));

    let before = pane.scroll_metrics().expect("scroll metrics before scroll");
    pane.set_scroll_offset_from_bottom(before.max_offset_from_bottom);

    assert!(pane.visible_text().contains("000000"));
    assert_eq!(pane.detection_text(), bottom_snapshot);
}

#[test]
fn extract_selection_uses_stable_rows_after_viewport_moves() {
    let mut terminal = crate::vt::Terminal::new(8, 3, 1024);
    write_numbered_lines(&mut terminal, 8);
    let pane = GhosttyPaneTerminal::new(terminal);

    pane.set_scroll_offset_from_bottom(3);
    let metrics = pane
        .scroll_metrics()
        .expect("scroll metrics after initial scroll");
    let mut selection = crate::vt::selection::Selection::anchor(
        PaneId::from_raw(1),
        Point::new(metrics.absolute_row_at_viewport(ViewportRow(0)), 0),
    );
    selection.drag(Point::new(
        metrics.absolute_row_at_viewport(ViewportRow(2)),
        5,
    ));

    pane.scroll_reset();

    let text = pane
        .extract_selection(&selection)
        .expect("selection should extract text");
    assert_eq!(text, "000003\n000004\n000005");
}

#[test]
fn recent_reads_include_viewport_before_scrollback_exists() {
    let mut terminal =
        crate::vt::Terminal::new(20, 20, crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES);
    terminal.write(b"hello123");
    let pane = GhosttyPaneTerminal::new(terminal);

    assert_eq!(pane.recent_text(3), "hello123\n");
    assert_eq!(pane.recent_unwrapped_text(3), "hello123");
}

#[test]
fn alternate_screen_recent_reads_keep_physical_row_ranges() {
    let mut terminal = crate::vt::Terminal::new(20, 20, 100);
    terminal.write(b"\x1b[?1049hhello123");
    let pane = GhosttyPaneTerminal::new(terminal);

    assert_eq!(pane.recent_text(3), "");
    assert_eq!(pane.recent_unwrapped_text(3), "");
}

#[test]
fn recent_unwrapped_text_ignores_soft_wraps() {
    let mut terminal = crate::vt::Terminal::new(5, 3, 100);
    terminal.write(b"ABCDEFGHIJ");
    let pane = GhosttyPaneTerminal::new(terminal);

    assert_eq!(pane.recent_text(3), "ABCDE\nFGHIJ\n");
    assert_eq!(pane.recent_unwrapped_text(3), "ABCDEFGHIJ");
}

#[test]
fn recent_snapshots_report_omitted_rendered_rows() {
    let mut terminal = crate::vt::Terminal::new(20, 3, 100);
    terminal.write(b"one\r\ntwo\r\nthree\r\nfour");
    let pane = GhosttyPaneTerminal::new(terminal);

    assert!(pane.recent_text_snapshot(2).truncated);
    assert!(pane.recent_ansi_snapshot(2).truncated);
    assert!(pane.recent_unwrapped_text_snapshot(2).truncated);
    assert!(pane.recent_unwrapped_ansi_snapshot(2).truncated);
    assert!(!pane.recent_text_snapshot(100).truncated);
}

#[test]
fn recent_snapshots_do_not_count_trailing_blank_rows_as_omitted() {
    let mut terminal = crate::vt::Terminal::new(20, 10, 100);
    terminal.write(b"one\r\ntwo");
    let pane = GhosttyPaneTerminal::new(terminal);

    // Ten rows exist but only two hold content; a five-row read leaves
    // nothing out above it.
    let snapshot = pane.recent_text_snapshot(5);
    assert_eq!(snapshot.text, "one\ntwo\n");
    assert!(!snapshot.truncated);
    assert!(!pane.recent_ansi_snapshot(5).truncated);
    assert!(!pane.recent_unwrapped_text_snapshot(5).truncated);
}

#[test]
fn detection_text_ignores_the_frame_a_clear_pushed_into_history() {
    let mut terminal = crate::vt::Terminal::new(20, 4, 100);
    terminal.write(b"a\r\nb\r\nc\r\nproceed? [y/n]");
    terminal.write(b"\x1b[H\x1b[2Jfresh");
    let pane = GhosttyPaneTerminal::new(terminal);

    let detection = pane.detection_text();
    assert_eq!(detection, "fresh\n");
    assert!(!detection.contains("proceed"));
}

#[test]
fn seeded_history_leaves_the_cursor_on_a_fresh_line() {
    let terminal = crate::vt::Terminal::new(20, 5, 100);
    let pane = GhosttyPaneTerminal::new(terminal);
    // Saved history is trimmed and ends mid-line on the old prompt.
    pane.seed_history_ansi("output\r\nuser@host $ ");
    let cursor = pane.cursor_state().expect("test precondition");
    assert_eq!((cursor.x, cursor.y), (0, 2));

    pane.process_pty_bytes(PaneId::from_raw(1), 0, b"new $ ");
    assert_eq!(pane.recent_text(5), "output\nuser@host $\nnew $\n");
}

#[test]
fn seeded_history_ending_in_a_line_break_gets_no_extra_blank_line() {
    let terminal = crate::vt::Terminal::new(20, 5, 100);
    let pane = GhosttyPaneTerminal::new(terminal);
    pane.seed_history_ansi("restored\r\n");
    let cursor = pane.cursor_state().expect("test precondition");
    assert_eq!((cursor.x, cursor.y), (0, 1));
}

#[test]
fn plain_text_reads_skip_wide_character_spacer_cells() {
    let mut terminal = crate::vt::Terminal::new(40, 3, 100);
    terminal.write("日本語テスト ABC 123".as_bytes());
    let pane = GhosttyPaneTerminal::new(terminal);

    assert_eq!(pane.visible_text(), "日本語テスト ABC 123\n");
    assert_eq!(pane.recent_text(3), "日本語テスト ABC 123\n");
    assert_eq!(pane.recent_unwrapped_text(3), "日本語テスト ABC 123");
    assert_eq!(pane.detection_text(), "日本語テスト ABC 123\n");
}

#[test]
fn recent_rows_preserve_combining_text_and_hide_image_placeholders() {
    let mut terminal = crate::vt::Terminal::new(40, 3, 1024 * 1024);
    terminal.write("old\r\n".repeat(100).as_bytes());
    terminal.write("界 e\u{301} \u{10eeee} tail  ".as_bytes());
    let pane = GhosttyPaneTerminal::new(terminal);
    assert_eq!(pane.recent_text(1), "界 e\u{301}   tail\n");
    let detection = pane.detection_text();
    assert_eq!(detection, "old\nold\n界 e\u{301}   tail\n");
    pane.set_scroll_offset_from_bottom(100);
    assert_eq!(pane.detection_text(), detection);
}

#[test]
fn visible_ansi_preserves_cell_style_sequences() {
    let mut terminal = crate::vt::Terminal::new(20, 3, 100);
    terminal.write(b"\x1b[31;1mred\x1b[0m plain");
    let pane = GhosttyPaneTerminal::new(terminal);

    let ansi = pane.visible_ansi();
    assert!(ansi.contains("red"));
    assert!(ansi.contains("plain"));
    assert!(ansi.contains("\x1b["));
}

#[test]
fn recent_ansi_can_read_styled_scrollback() {
    let mut terminal = crate::vt::Terminal::new(20, 3, 100);
    terminal.write(b"\x1b[34mblue\x1b[0m\r\nline2\r\nline3\r\nline4");
    let pane = GhosttyPaneTerminal::new(terminal);

    let ansi = pane.recent_ansi(4);
    assert!(ansi.contains("blue"));
    assert!(ansi.contains("line4"));
    assert!(ansi.contains("\x1b["));
}

#[test]
fn resize_shrinks_both_axes_with_cursor_at_old_bottom() {
    let mut terminal = crate::vt::Terminal::new(8, 4, 10_000);
    terminal.write(b"alpha\r\nbeta\r\ngamma\r\ndelta");
    let pane = GhosttyPaneTerminal::new(terminal);

    pane.resize(crate::core::geometry::PaneGeometry::new(7, 3, 8, 16));

    assert_eq!(pane.visible_text(), "beta\ngamma\ndelta\n");
    assert_eq!(pane.detection_text(), "beta\ngamma\ndelta\n");
    assert_eq!(
        pane.scroll_metrics(),
        Some(ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 1,
            viewport_rows: 3,
            history_origin: AbsRow(4),
        })
    );
}

#[test]
fn resize_reflow_keeps_scrolled_viewport_and_bottom_detection_sane() {
    let mut terminal = crate::vt::Terminal::new(12, 4, 10_000);
    write_wrapped_contract_lines(&mut terminal, 40);
    let pane = GhosttyPaneTerminal::new(terminal);

    let bottom_snapshot = pane.detection_text();
    assert!(bottom_snapshot.contains("END"));

    let initial = pane.scroll_metrics().expect("initial scroll metrics");
    assert!(initial.max_offset_from_bottom > 0);
    pane.set_scroll_offset_from_bottom(initial.max_offset_from_bottom / 2);
    assert!(!pane.visible_text().trim().is_empty());

    for (rows, cols) in [(4, 10), (4, 7), (6, 18), (3, 9), (5, 12)] {
        let before_resize = pane.scroll_metrics().expect("scroll metrics before resize");
        pane.resize(crate::core::geometry::PaneGeometry::new(cols, rows, 0, 0));

        let metrics = pane.scroll_metrics().expect("scroll metrics after resize");
        assert_eq!(metrics.viewport_rows, rows as usize);
        assert_eq!(
            metrics.offset_from_bottom,
            before_resize
                .offset_from_bottom
                .min(metrics.max_offset_from_bottom)
        );
        assert!(
            metrics.offset_from_bottom > 0,
            "resize should preserve a scrolled viewport instead of jumping to bottom"
        );
        assert!(metrics.max_offset_from_bottom > 0);
        let visible = pane.visible_text();
        assert!(
            !visible.trim().is_empty(),
            "visible text should not be empty after resize to {rows}x{cols}; metrics={metrics:?}; detection={:?}; recent={:?}",
            pane.detection_text(),
            pane.recent_text(6)
        );
        assert!(
            pane.detection_text().contains("END"),
            "bottom detection should remain independent from the scrolled viewport after resize"
        );
    }
}

#[test]
fn resize_recovery_does_not_replay_history_when_visible_screen_was_blank() {
    let mut terminal = crate::vt::Terminal::new(20, 3, 10_000);
    terminal.write(b"old history\r\n\x1b[2J\x1b[H");
    let pane = GhosttyPaneTerminal::new(terminal);

    assert!(pane.visible_text().trim().is_empty());
    assert!(pane.detection_text().trim().is_empty());

    pane.resize(crate::core::geometry::PaneGeometry::new(20, 3, 0, 0));

    assert!(pane.visible_text().trim().is_empty());
    assert!(pane.detection_text().trim().is_empty());
    assert!(pane.recent_text(3).trim().is_empty());
}

#[test]
fn resize_recovery_does_not_replay_scrolled_history_over_blank_bottom() {
    let mut terminal = crate::vt::Terminal::new(20, 3, 10_000);
    write_numbered_lines(&mut terminal, 20);
    terminal.write(b"\x1b[2J\x1b[H");
    let pane = GhosttyPaneTerminal::new(terminal);

    assert!(pane.detection_text().trim().is_empty());
    let metrics = pane.scroll_metrics().expect("scroll metrics");
    pane.set_scroll_offset_from_bottom(metrics.max_offset_from_bottom);
    assert!(!pane.visible_text().trim().is_empty());

    pane.resize(crate::core::geometry::PaneGeometry::new(20, 3, 0, 0));

    assert!(pane.detection_text().trim().is_empty());
    assert!(pane.recent_text(3).trim().is_empty());
}

#[test]
fn process_pty_bytes_answers_xtwinops_size_queries() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.resize(crate::core::geometry::PaneGeometry::new(80, 24, 9, 18));

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[14t\x1b[16t\x1b[18t");

    assert_eq!(
        result.terminal_responses,
        vec![
            Bytes::from_static(b"\x1b[4;432;720t"),
            Bytes::from_static(b"\x1b[6;18;9t"),
            Bytes::from_static(b"\x1b[8;24;80t"),
        ]
    );
}

#[test]
fn xtwinops_size_queries_follow_successful_resize() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.resize(crate::core::geometry::PaneGeometry::new(80, 24, 9, 18));
    pane.resize(crate::core::geometry::PaneGeometry::new(100, 30, 10, 20));

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[14t\x1b[16t\x1b[18t");

    assert_eq!(
        result.terminal_responses,
        vec![
            Bytes::from_static(b"\x1b[4;600;1000t"),
            Bytes::from_static(b"\x1b[6;20;10t"),
            Bytes::from_static(b"\x1b[8;30;100t"),
        ]
    );
}

#[test]
fn xtwinops_size_queries_stay_silent_without_pixel_geometry() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    for (cell_width_px, cell_height_px) in [(0, 0), (0, 18), (9, 0)] {
        pane.resize(crate::core::geometry::PaneGeometry::new(
            80,
            24,
            cell_width_px,
            cell_height_px,
        ));
        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[14t\x1b[16t\x1b[18t");
        // CSI 14 t (pixel geometry) and CSI 16 t (cell size in pixels) stay
        // silent without pixel geometry, but CSI 18 t reports characters,
        // which is always known, so it is answered regardless.
        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b[8;24;80t")]
        );
    }
}

#[test]
fn enabling_in_band_size_reports_after_alt_screen_resize_reports_current_size() {
    let terminal = crate::vt::Terminal::new(91, 24, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.process_pty_bytes(pane_id, 0, b"\x1b[?1049h");
    assert!(
        pane.resize(crate::core::geometry::PaneGeometry::new(92, 24, 9, 18))
            .is_empty()
    );

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[?2048h");

    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b[48;24;92;432;828t")]
    );
}

#[test]
fn resize_returns_in_band_size_report_response() {
    let mut terminal = crate::vt::Terminal::new(80, 24, 0);
    terminal.mode_set(2048, true).expect("test precondition");
    let pane = GhosttyPaneTerminal::new(terminal);

    let responses = pane.resize(crate::core::geometry::PaneGeometry::new(100, 40, 9, 18));

    assert_eq!(
        responses,
        vec![Bytes::from_static(b"\x1B[48;40;100;720;900t")]
    );
}

#[test]
fn synchronized_output_suppresses_intermediate_render_requests_until_batch_ends() {
    let terminal = crate::vt::Terminal::new(80, 24, 0);
    let pane_terminal = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    assert_eq!(pane_terminal.synchronized_output_state(), (false, 0));
    pane_terminal.process_pty_bytes(pane_id, 0, b"ordinary output");
    assert_eq!(pane_terminal.synchronized_output_state(), (false, 0));

    let begin = pane_terminal.process_pty_bytes(pane_id, 0, b"\x1b[?2026h");
    assert!(!begin.request_render);
    assert_eq!(pane_terminal.synchronized_output_state(), (true, 1));

    let body = pane_terminal.process_pty_bytes(pane_id, 0, b"hello");
    assert!(!body.request_render);
    assert_eq!(pane_terminal.synchronized_output_state(), (true, 1));

    let end = pane_terminal.process_pty_bytes(pane_id, 0, b"\x1b[?2026l");
    assert!(end.request_render);
    assert_eq!(pane_terminal.synchronized_output_state(), (false, 2));
}

#[test]
fn seeded_history_is_rendered_on_next_draw() {
    let terminal = crate::vt::Terminal::new(20, 5, 100);
    let pane = GhosttyPaneTerminal::new(terminal);
    pane.seed_history_ansi("restored history");

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    let row = (0..16).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
    assert_eq!(row, "restored history");
}

#[test]
fn render_leaves_unknown_host_default_background_transparent() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"hi");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].symbol(), "h");
    assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Reset));
    assert_eq!(buffer[(0, 0)].style().bg, Some(Color::Reset));
    assert_eq!(buffer[(2, 0)].symbol(), " ");
    assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Reset));
    assert_eq!(buffer[(2, 0)].style().bg, Some(Color::Reset));
}

#[test]
fn render_blanks_kitty_unicode_placeholders() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal
            .write("before\u{10eeee}\u{0305}\u{0305}after".as_bytes());
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].symbol(), "b");
    assert_eq!(buffer[(6, 0)].symbol(), " ");
    assert_eq!(buffer[(7, 0)].symbol(), "a");
    assert_eq!(pane.visible_text().lines().next(), Some("before after"));
    assert_eq!(pane.recent_text(5), "before after\n");
}

#[test]
fn render_keeps_explicit_cell_foreground_when_host_is_unknown() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b[38;2;68;85;102mhi\x1b[0m");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    let expected_fg = Some(Color::Rgb(0x44, 0x55, 0x66));
    assert_eq!(buffer[(0, 0)].symbol(), "h");
    assert_eq!(buffer[(0, 0)].style().fg, expected_fg);
    assert_eq!(buffer[(2, 0)].symbol(), " ");
    assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Reset));
}

#[test]
fn render_keeps_explicit_cell_background_when_host_is_unknown() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b[48;2;68;85;102mhi\x1b[0m");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    let expected_bg = Some(Color::Rgb(0x44, 0x55, 0x66));
    assert_eq!(buffer[(0, 0)].symbol(), "h");
    assert_eq!(buffer[(0, 0)].style().bg, expected_bg);
    assert_eq!(buffer[(2, 0)].symbol(), " ");
    assert_eq!(buffer[(2, 0)].style().bg, Some(Color::Reset));
}

#[test]
fn render_preserves_palette_colors_instead_of_flattening_to_rgb() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(
            b"\x1b[31mR\x1b[0m \x1b[38;5;171mI\x1b[0m \x1b[48;5;4mB\x1b[0m \x1b[38;2;1;2;3mT",
        );
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].symbol(), "R");
    assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Indexed(1)));
    assert_eq!(buffer[(2, 0)].symbol(), "I");
    assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Indexed(171)));
    assert_eq!(buffer[(4, 0)].symbol(), "B");
    assert_eq!(buffer[(4, 0)].style().bg, Some(Color::Indexed(4)));
    assert_eq!(buffer[(6, 0)].symbol(), "T");
    assert_eq!(buffer[(6, 0)].style().fg, Some(Color::Rgb(1, 2, 3)));
}

#[test]
fn render_preserves_palette_background_fill_cells() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b[48;5;4m\x1b[K");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    for x in 0..20 {
        assert_eq!(buffer[(x, 0)].symbol(), " ");
        assert_eq!(buffer[(x, 0)].style().bg, Some(Color::Indexed(4)));
    }
}

#[test]
fn render_preserves_rgb_background_fill_cells() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b[48;2;17;34;51m\x1b[K");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    for x in 0..20 {
        assert_eq!(buffer[(x, 0)].symbol(), " ");
        assert_eq!(buffer[(x, 0)].style().bg, Some(Color::Rgb(17, 34, 51)));
    }
}

#[test]
fn process_pty_bytes_does_not_advertise_unsupported_glyph_protocol() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b_25a1;s\x1b\\");

    assert!(result.terminal_responses.is_empty());
}

#[test]
fn process_pty_bytes_returns_core_query_responses_without_queuing_input() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[6n");

    assert_eq!(result.terminal_responses.len(), 1);
    assert!(String::from_utf8_lossy(&result.terminal_responses[0]).contains('R'));
}

#[test]
fn color_scheme_queries_and_live_updates_follow_terminal_mode() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    assert!(
        pane.apply_host_terminal_appearance(Some(crate::host_term::theme::HostAppearance::Dark))
            .is_none()
    );
    let query = pane.process_pty_bytes(pane_id, 0, b"\x1b[?996n");
    assert_eq!(
        query.terminal_responses,
        vec![Bytes::from_static(b"\x1b[?997;1n")]
    );

    pane.process_pty_bytes(pane_id, 0, b"\x1b[?2031h");
    assert!(
        pane.apply_host_terminal_appearance(Some(crate::host_term::theme::HostAppearance::Dark))
            .is_none()
    );
    assert_eq!(
        pane.apply_host_terminal_appearance(Some(crate::host_term::theme::HostAppearance::Light)),
        Some(Bytes::from_static(b"\x1b[?997;2n"))
    );

    assert!(pane.apply_host_terminal_appearance(None).is_none());
    let unknown_query = pane.process_pty_bytes(pane_id, 0, b"\x1b[?996n");
    assert!(unknown_query.terminal_responses.is_empty());
    assert!(
        pane.apply_host_terminal_appearance(Some(crate::host_term::theme::HostAppearance::Dark))
            .is_none()
    );

    pane.process_pty_bytes(pane_id, 0, b"\x1bc");
    assert!(
        pane.apply_host_terminal_appearance(Some(crate::host_term::theme::HostAppearance::Light))
            .is_none()
    );
}

#[test]
fn process_pty_bytes_returns_xtgettcap_truecolor_query_responses_without_queuing_input() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(
        pane_id,
        0,
        b"\x1bP+q5463;524742;73657472676266;73657472676262\x1b\\",
    );

    assert_eq!(
        result.terminal_responses,
        vec![
            expected_xtgettcap_response("5463", None),
            expected_xtgettcap_response("524742", Some(b"8")),
            expected_xtgettcap_response("73657472676266", Some(b"\\E[38:2:%p1%d:%p2%d:%p3%dm")),
            expected_xtgettcap_response("73657472676262", Some(b"\\E[48:2:%p1%d:%p2%d:%p3%dm")),
        ]
    );
}

#[test]
fn process_pty_bytes_returns_fragmented_c1_xtgettcap_once_in_order() {
    // Raw C1 bytes (0x90 here) are text/no-ops to the 7-bit vte parser
    // and never open a DCS: only the ESC-introduced form is a real
    // query. See the framing note atop `ghostty/scan.rs`.
    for (query, opens_dcs) in [
        (b"\x90+q5463;524742\x9c".as_slice(), false),
        (b"\x1bP+q5463;524742\x9c".as_slice(), true),
        (b"\x90+q5463;524742\x1b\\".as_slice(), false),
    ] {
        for fragmented in [false, true] {
            let terminal = crate::vt::Terminal::new(20, 5, 0);
            let pane = GhosttyPaneTerminal::new(terminal);
            let pane_id = PaneId::from_raw(1);
            pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
                background: Some(crate::host_term::theme::RgbColor {
                    r: 0,
                    g: 0x2b,
                    b: 0x36,
                }),
                ..Default::default()
            });
            let mut replies = pane
                .process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07")
                .terminal_responses;
            for chunk in query.chunks(if fragmented { 1 } else { query.len() }) {
                replies.extend(pane.process_pty_bytes(pane_id, 0, chunk).terminal_responses);
            }
            replies.extend(
                pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x1b\\\x1bP+q5375\x1b\\")
                    .terminal_responses,
            );
            let expected = if opens_dcs {
                vec![
                    Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
                    expected_xtgettcap_response("5463", None),
                    expected_xtgettcap_response("524742", Some(b"8")),
                    Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
                    expected_xtgettcap_response("5375", None),
                ]
            } else {
                vec![
                    Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
                    Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
                    expected_xtgettcap_response("5375", None),
                ]
            };
            assert_eq!(
                replies, expected,
                "query={query:?}, fragmented={fragmented}"
            );
        }
    }
}

#[test]
fn process_pty_bytes_returns_split_xtgettcap_query_response() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q4");
    assert!(result.terminal_responses.is_empty());
    let result = pane.process_pty_bytes(pane_id, 0, b"d73");
    assert!(result.terminal_responses.is_empty());
    // The parser ends DCS on ESC, before the final ST backslash.
    // Splitting ST must not lose the reply or emit it again on completion.
    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b");

    assert_eq!(
        result.terminal_responses,
        vec![expected_xtgettcap_response(
            "4D73",
            Some(b"\\E]52;%p1%s;%p2%s\\007")
        )]
    );
    let result = pane.process_pty_bytes(pane_id, 0, b"\\");
    assert!(result.terminal_responses.is_empty());
}

#[test]
fn process_pty_bytes_orders_device_attribute_reply_before_following_xtgettcap_reply() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[c\x1bP+q5463\x1b\\");

    assert_eq!(result.terminal_responses.len(), 2);
    assert!(String::from_utf8_lossy(&result.terminal_responses[0]).contains('c'));
    assert_eq!(
        result.terminal_responses[1],
        expected_xtgettcap_response("5463", None)
    );
}

#[test]
fn process_pty_bytes_orders_xtgettcap_reply_before_following_device_attribute_reply() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q5463\x1b\\\x1b[c");

    assert_eq!(result.terminal_responses.len(), 2);
    assert_eq!(
        result.terminal_responses[0],
        expected_xtgettcap_response("5463", None)
    );
    assert!(String::from_utf8_lossy(&result.terminal_responses[1]).contains('c'));
}

#[test]
fn process_pty_bytes_orders_xtgettcap_reply_before_following_default_color_reply() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: None,
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x00,
            g: 0x2b,
            b: 0x36,
        }),
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q5463\x1b\\\x1b]11;?\x07");

    assert_eq!(
        result.terminal_responses,
        vec![
            expected_xtgettcap_response("5463", None),
            Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
        ]
    );
}

#[test]
fn host_theme_update_preserves_child_default_color_override() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;#112233\x07");
    assert!(result.terminal_responses.is_empty());

    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: None,
        background: Some(crate::host_term::theme::RgbColor {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc,
        }),
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07");
    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]11;rgb:1111/2222/3333\x07")]
    );
}

#[test]
fn child_default_color_reset_restores_cached_host_color() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.process_pty_bytes(pane_id, 0, b"\x1b]11;#112233\x07");
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: None,
        background: Some(crate::host_term::theme::RgbColor {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc,
        }),
        ..Default::default()
    });
    pane.process_pty_bytes(pane_id, 0, b"\x1b]111\x07");
    assert!(!pane.has_transient_default_color_override());

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07");
    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]11;rgb:aaaa/bbbb/cccc\x1b\\")]
    );
}

#[test]
fn process_pty_bytes_recovers_xtgettcap_after_osc_bel_terminator() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]0;title\x07\x1bP+q5463\x1b\\");

    assert_eq!(
        result.terminal_responses,
        vec![expected_xtgettcap_response("5463", None)]
    );
}

#[test]
fn process_pty_bytes_orders_default_color_reset_reply_before_xtgettcap() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x00,
            g: 0x2b,
            b: 0x36,
        }),
        ..Default::default()
    });

    // OSC ends at the ESC of its string terminator, so the reply to the
    // query arrives with the chunk that carries that ESC.
    let result =
        pane.process_pty_bytes(pane_id, 0, b"\x1b]11;#112233\x07\x1b]111\x07\x1b]11;?\x1b");
    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\")]
    );
    let result = pane.process_pty_bytes(pane_id, 0, b"\\\x1bP+q436f\x1b\\");

    assert_eq!(
        result.terminal_responses,
        vec![expected_xtgettcap_response("436F", Some(b"256"))]
    );
}

#[test]
fn process_pty_bytes_ignores_unknown_and_unsupported_xtgettcap_queries() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q6E6F7065;4D7\x1b\\");

    assert!(result.terminal_responses.is_empty());
}

#[test]
fn process_pty_bytes_returns_underline_color_xtgettcap_query_responses() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q5375;536D756C78;536574756C63\x1b\\");

    assert_eq!(
        result.terminal_responses,
        vec![
            expected_xtgettcap_response("5375", None),
            expected_xtgettcap_response("536D756C78", Some(b"\\E[4:%p1%dm")),
            expected_xtgettcap_response(
                "536574756C63",
                Some(b"\\E[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m")
            ),
        ]
    );
}

#[test]
fn render_preserves_underline_color() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b[4m\x1b[58:2::17:34:51mU");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let style = terminal.backend().buffer()[(0, 0)].style();
    assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    assert_eq!(style.underline_color, Some(Color::Rgb(17, 34, 51)));
}

#[test]
fn full_frame_preserves_curly_underline_style() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b[4:3mU");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let frame = crate::protocol::FrameData::from_ratatui_buffer(terminal.backend().buffer(), None);
    assert_eq!(frame.cells[0].symbol, "U");
    assert_eq!(
        frame.cells[0].style.underline,
        crate::vt::UnderlineStyle::Curly
    );
}

#[test]
fn process_pty_bytes_orders_default_color_reply_before_following_device_attribute_reply() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: None,
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x00,
            g: 0x2b,
            b: 0x36,
        }),
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07\x1b[c");

    assert_eq!(result.terminal_responses.len(), 2);
    assert_eq!(
        result.terminal_responses[0],
        Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\")
    );
    assert!(String::from_utf8_lossy(&result.terminal_responses[1]).contains('c'));
}

#[test]
fn process_pty_bytes_returns_host_palette_color_without_queuing_input() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(
        crate::host_term::theme::TerminalTheme::default().with_palette_color(
            0,
            crate::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            },
        ),
    );

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;0;?\x07");

    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]4;0;rgb:1111/2222/3333\x1b\\")]
    );
}

#[test]
fn opentui_256_palette_query_burst_uses_host_snapshot() {
    use std::fmt::Write as _;

    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    let mut theme = crate::host_term::theme::TerminalTheme::default();
    let mut queries = String::new();
    for index in 0..=u8::MAX {
        theme = theme.with_palette_color(
            index,
            crate::host_term::theme::RgbColor {
                r: index,
                g: 0x22,
                b: 0x33,
            },
        );
        let _ = write!(queries, "\x1b]4;{index};?\x07");
    }
    pane.apply_host_terminal_theme(theme);

    let result = pane.process_pty_bytes(pane_id, 0, queries.as_bytes());

    assert_eq!(result.terminal_responses.len(), 256);
    assert_eq!(
        result.terminal_responses[0],
        Bytes::from_static(b"\x1b]4;0;rgb:0000/2222/3333\x1b\\")
    );
    assert_eq!(
        result.terminal_responses[255],
        Bytes::from_static(b"\x1b]4;255;rgb:ffff/2222/3333\x1b\\")
    );
}

#[test]
fn child_palette_override_survives_host_refresh_until_reset() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(
        crate::host_term::theme::TerminalTheme::default().with_palette_color(
            7,
            crate::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            },
        ),
    );
    pane.process_pty_bytes(pane_id, 0, b"\x1b]4;7;rgb:aa/bb/cc\x1b\\");

    pane.apply_host_terminal_theme(
        crate::host_term::theme::TerminalTheme::default().with_palette_color(
            7,
            crate::host_term::theme::RgbColor {
                r: 0x44,
                g: 0x55,
                b: 0x66,
            },
        ),
    );
    let overridden = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;7;?\x1b\\");
    assert_eq!(
        overridden.terminal_responses,
        vec![Bytes::from_static(b"\x1b]4;7;rgb:aaaa/bbbb/cccc\x1b\\")]
    );

    pane.process_pty_bytes(pane_id, 0, b"\x1b]104;7\x1b\\");
    let reset = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;7;?\x1b\\");
    assert_eq!(
        reset.terminal_responses,
        vec![Bytes::from_static(b"\x1b]4;7;rgb:4444/5555/6666\x1b\\")]
    );
}

#[test]
fn process_pty_bytes_returns_split_palette_color_query_response() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    let color = current_palette_color(&pane, 255);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;25");
    assert!(result.terminal_responses.is_empty());
    // The OSC is complete at the ESC of its terminator.
    let result = pane.process_pty_bytes(pane_id, 0, b"5;?\x1b");
    assert_eq!(
        result.terminal_responses,
        vec![expected_osc_rgb_response("4;255", color)]
    );
    let result = pane.process_pty_bytes(pane_id, 0, b"\\");

    assert!(result.terminal_responses.is_empty());
}

#[test]
fn process_pty_bytes_ignores_malformed_and_preserves_multi_palette_queries() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(
            pane_id,
            0,
            b"\x1b]4;;?\x07\x1b]4;-1;?\x07\x1b]4;256;?\x07\x1b]4;0;?;1;?\x07\x1b]4;0;rgb:1111/2222/3333\x07",
        );

    // A multi-entry query is answered one entry per reply.
    assert_eq!(result.terminal_responses.len(), 2);
    assert!(result.terminal_responses[0].starts_with(b"\x1b]4;0;rgb:"));
    assert!(result.terminal_responses[1].starts_with(b"\x1b]4;1;rgb:"));
}

#[test]
fn process_pty_bytes_orders_palette_reply_before_following_terminal_replies() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    let color = current_palette_color(&pane, 0);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: None,
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x00,
            g: 0x2b,
            b: 0x36,
        }),
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;0;?\x07\x1b]11;?\x07\x1b[c");

    assert_eq!(result.terminal_responses.len(), 3);
    assert_eq!(
        result.terminal_responses[0],
        expected_osc_rgb_response("4;0", color)
    );
    assert_eq!(
        result.terminal_responses[1],
        Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\")
    );
    assert!(String::from_utf8_lossy(&result.terminal_responses[2]).contains('c'));
}

#[test]
fn process_pty_bytes_returns_default_color_query_responses_without_queuing_input() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: None,
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x00,
            g: 0x2b,
            b: 0x36,
        }),
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07");

    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\")]
    );
}

#[test]
fn process_pty_bytes_preserves_untracked_multi_color_query_responses() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0x65,
            g: 0x7b,
            b: 0x83,
        }),
        background: Some(crate::host_term::theme::RgbColor {
            r: 0xfd,
            g: 0xf6,
            b: 0xe3,
        }),
        ..Default::default()
    });

    let palette = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;0;?;1;?\x1b\\");
    let palette_response = palette.terminal_responses.concat();
    assert!(palette_response.starts_with(b"\x1b]4;0;rgb:"));
    assert_eq!(
        palette_response
            .windows(4)
            .filter(|window| *window == b"rgb:")
            .count(),
        2
    );

    let defaults = pane.process_pty_bytes(pane_id, 0, b"\x1b]10;?;?;?\x1b\\");
    let default_response = defaults.terminal_responses.concat();
    assert!(
        default_response.starts_with(b"\x1b]10;rgb:"),
        "unexpected default-color report: {:?}",
        String::from_utf8_lossy(&default_response)
    );
    assert_eq!(
        default_response
            .windows(4)
            .filter(|window| *window == b"rgb:")
            .count(),
        3
    );
    let core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
    assert!(!has_default_color_override(&core.terminal));
    drop(core);
}

#[test]
fn process_pty_bytes_preserves_earlier_aggregate_palette_reply() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;0;?;1;?\x1b\\\x1b]4;0;?\x1b\\");

    assert_eq!(result.terminal_responses.len(), 3);
    assert!(result.terminal_responses[0].starts_with(b"\x1b]4;0;rgb:"));
    assert!(result.terminal_responses[1].starts_with(b"\x1b]4;1;rgb:"));
    assert!(result.terminal_responses[2].starts_with(b"\x1b]4;0;rgb:"));
}

#[test]
fn process_pty_bytes_preserves_core_reply_for_child_color_override() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.process_pty_bytes(pane_id, 0, b"\x1b]10;rgb:11/22/33\x07");
    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]10;?\x1b\\");

    assert_eq!(result.terminal_responses.len(), 1);
    assert!(result.terminal_responses[0].starts_with(b"\x1b]10;rgb:1111/2222/3333"));
}

#[test]
fn process_pty_bytes_tracks_later_multi_value_color_set() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);

    pane.process_pty_bytes(pane_id, 0, b"\x1b]10;?;rgb:44/55/66\x1b\\");

    let core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
    assert_eq!(
        core.terminal
            .default_color_override(crate::vt::DefaultColor::Foreground),
        None
    );
    assert_eq!(
        core.terminal
            .default_color_override(crate::vt::DefaultColor::Background),
        Some(rgb(0x44, 0x55, 0x66))
    );
}

#[test]
fn process_pty_bytes_returns_cursor_color_query_response_from_foreground_fallback() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0x65,
            g: 0x7b,
            b: 0x83,
        }),
        background: None,
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]12;?\x07");

    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]12;rgb:6565/7b7b/8383\x1b\\")]
    );
}

#[test]
fn process_pty_bytes_returns_cursor_color_query_response_from_child_foreground() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0x65,
            g: 0x7b,
            b: 0x83,
        }),
        background: None,
        ..Default::default()
    });

    pane.process_pty_bytes(pane_id, 0, b"\x1b]10;rgb:11/22/33\x07");
    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]12;?\x07");

    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]12;rgb:1111/2222/3333\x1b\\")]
    );
}

#[test]
fn process_pty_bytes_returns_explicit_cursor_color_query_response() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0x65,
            g: 0x7b,
            b: 0x83,
        }),
        background: None,
        ..Default::default()
    });

    pane.process_pty_bytes(pane_id, 0, b"\x1b]12;rgb:11/22/33\x07");
    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]12;?\x07");

    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]12;rgb:1111/2222/3333\x1b\\")]
    );
}

#[test]
fn process_pty_bytes_returns_default_color_query_responses_in_order() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0x65,
            g: 0x7b,
            b: 0x83,
        }),
        background: Some(crate::host_term::theme::RgbColor {
            r: 0xfd,
            g: 0xf6,
            b: 0xe3,
        }),
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07");

    assert_eq!(
        result.terminal_responses,
        vec![
            Bytes::from_static(b"\x1b]10;rgb:6565/7b7b/8383\x1b\\"),
            Bytes::from_static(b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\"),
            Bytes::from_static(b"\x1b]12;rgb:6565/7b7b/8383\x1b\\"),
        ]
    );
}

#[test]
fn process_pty_bytes_returns_split_default_color_query_response() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: None,
        background: Some(crate::host_term::theme::RgbColor {
            r: 0xfd,
            g: 0xf6,
            b: 0xe3,
        }),
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11");
    assert!(result.terminal_responses.is_empty());
    let result = pane.process_pty_bytes(pane_id, 0, b";?\x1b");
    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\")]
    );
    let result = pane.process_pty_bytes(pane_id, 0, b"\\");

    assert!(result.terminal_responses.is_empty());
}

#[test]
fn process_pty_bytes_returns_split_cursor_color_query_response() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0xfd,
            g: 0xf6,
            b: 0xe3,
        }),
        background: None,
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]12");
    assert!(result.terminal_responses.is_empty());
    let result = pane.process_pty_bytes(pane_id, 0, b";?\x1b");
    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]12;rgb:fdfd/f6f6/e3e3\x1b\\")]
    );
    let result = pane.process_pty_bytes(pane_id, 0, b"\\");

    assert!(result.terminal_responses.is_empty());
}

#[test]
fn process_pty_bytes_tracks_default_color_set_and_reset_before_replying() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
        foreground: None,
        background: Some(crate::host_term::theme::RgbColor {
            r: 0xfd,
            g: 0xf6,
            b: 0xe3,
        }),
        ..Default::default()
    });

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;rgb:11/22/33\x07\x1b]11;?\x07");
    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]11;rgb:1111/2222/3333\x07")]
    );

    let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]111\x07\x1b]11;?\x07");
    assert_eq!(
        result.terminal_responses,
        vec![Bytes::from_static(b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\")]
    );
}

#[test]
fn render_leaves_host_default_background_transparent() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let host_theme = crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc,
        }),
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x11,
            g: 0x22,
            b: 0x33,
        }),
        ..Default::default()
    };
    pane.apply_host_terminal_theme(host_theme);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"hi");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].symbol(), "h");
    assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Reset));
    assert_eq!(buffer[(0, 0)].style().bg, Some(Color::Reset));
    assert_eq!(buffer[(2, 0)].symbol(), " ");
    assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Reset));
    assert_eq!(buffer[(2, 0)].style().bg, Some(Color::Reset));
}

#[test]
fn render_keeps_explicit_default_foreground_when_it_differs_from_host() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let host_theme = crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc,
        }),
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x11,
            g: 0x22,
            b: 0x33,
        }),
        ..Default::default()
    };
    pane.apply_host_terminal_theme(host_theme);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b]10;rgb:44/55/66\x1b\\hi");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    let expected_fg = Some(Color::Rgb(0x44, 0x55, 0x66));
    assert_eq!(buffer[(0, 0)].symbol(), "h");
    assert_eq!(buffer[(0, 0)].style().fg, expected_fg);
    assert_eq!(buffer[(2, 0)].symbol(), " ");
    assert_eq!(buffer[(2, 0)].style().fg, expected_fg);
}

#[test]
fn render_keeps_explicit_default_background_when_it_differs_from_host() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let host_theme = crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc,
        }),
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x11,
            g: 0x22,
            b: 0x33,
        }),
        ..Default::default()
    };
    pane.apply_host_terminal_theme(host_theme);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b]11;rgb:44/55/66\x1b\\hi");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    let expected_bg = Some(Color::Rgb(0x44, 0x55, 0x66));
    assert_eq!(buffer[(0, 0)].symbol(), "h");
    assert_eq!(buffer[(0, 0)].style().bg, expected_bg);
    assert_eq!(buffer[(2, 0)].symbol(), " ");
    assert_eq!(buffer[(2, 0)].style().bg, expected_bg);
}

#[test]
fn render_inverse_text_swaps_fg_and_resolved_bg_when_bg_is_transparent() {
    let terminal = crate::vt::Terminal::new(20, 5, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let host_theme = crate::host_term::theme::TerminalTheme {
        foreground: Some(crate::host_term::theme::RgbColor {
            r: 0xaa,
            g: 0xbb,
            b: 0xcc,
        }),
        background: Some(crate::host_term::theme::RgbColor {
            r: 0x11,
            g: 0x22,
            b: 0x33,
        }),
        ..Default::default()
    };
    pane.apply_host_terminal_theme(host_theme);
    {
        let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
        // SGR 7 enables inverse/reverse video
        core.terminal.write(b"\x1b[7mhi\x1b[27m");
    }

    let backend = ratatui::backend::TestBackend::new(20, 5);
    let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
    terminal
        .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
        .expect("test precondition");

    let buffer = terminal.backend().buffer();
    let cell = &buffer[(0, 0)];
    assert_eq!(cell.symbol(), "h");
    // After inverse: fg should be the resolved bg, bg should be the original fg.
    // fg must NOT be Color::Reset (which would be the same hue as bg).
    assert_eq!(cell.style().fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
    assert_eq!(cell.style().bg, Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
}

#[test]
fn trim_trailing_blank_rows_drops_empty_viewport_tail() {
    let mut rows = vec!["hello".to_string(), String::new(), "   ".to_string()];
    trim_trailing_blank_rows(&mut rows);
    assert_eq!(rows, vec!["hello".to_string()]);
}

/// Once history is full every line of output evicts one: screen rows
/// drift onto other lines, absolute rows stay on theirs.
#[test]
fn absolute_rows_survive_eviction_where_screen_rows_drift() {
    // One byte of budget buys the minimum history.
    let mut terminal = crate::vt::Terminal::new(10, 3, 1);
    write_numbered_lines(&mut terminal, 1_100);
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
    let pane_id = PaneId::from_raw(1);
    let position = pane.scroll_position().expect("test precondition");
    assert!(
        position.metrics.history_origin > AbsRow(0),
        "history must be full"
    );
    assert_eq!(
        position.viewport_top_row(),
        position
            .metrics
            .history_origin
            .saturating_add(u64::try_from(position.metrics.max_offset_from_bottom).expect("fits"),)
    );

    // Line i was written on absolute row i.
    let found = pane.search_text_window_absolute(
        "001050",
        true,
        TerminalSearchDirection::Forward,
        TerminalTextPoint {
            row: AbsRow(0),
            col: 0,
        },
        None,
        8,
    );
    assert_eq!(found.total, 1);
    let line = found.matches[0].start.row;
    assert_eq!(line, AbsRow(1_050));
    let selection = crate::vt::selection::Selection::range(
        PaneId::from_raw(1),
        Point::new(line, 0),
        Point::new(line, 5),
    );
    assert_eq!(
        pane.extract_selection(&selection).as_deref(),
        Some("001050")
    );

    for i in 1_100..1_150 {
        pane.process_pty_bytes(pane_id, 0, format!("{i:06}\r\n").as_bytes());
    }
    assert_eq!(
        pane.scroll_position()
            .expect("test precondition")
            .metrics
            .history_origin,
        position.metrics.history_origin.saturating_add(50)
    );
    assert_eq!(
        pane.extract_selection(&selection).as_deref(),
        Some("001050")
    );
    assert_eq!(
        pane.word_motion_target_absolute(line, 0, TerminalWordMotion::NextEnd),
        Some(TerminalTextPoint { row: line, col: 5 })
    );
    // The screen-row entry points agree with the absolute ones at the
    // moment they are called.
    let origin = pane
        .scroll_position()
        .expect("test precondition")
        .metrics
        .history_origin;
    let now = line.screen_row(origin).expect("line remains retained");
    assert_eq!(
        pane.word_motion_target(now, 0, TerminalWordMotion::NextEnd),
        Some(TerminalTextPoint { row: now, col: 5 })
    );

    // An evicted row is refused rather than read.
    let evicted = origin.saturating_sub(1);
    let gone = crate::vt::selection::Selection::range(
        PaneId::from_raw(1),
        Point::new(evicted, 0),
        Point::new(evicted, 5),
    );
    assert_eq!(pane.extract_selection(&gone), None);
    assert_eq!(
        pane.word_motion_target_absolute(evicted, 0, TerminalWordMotion::NextStart),
        None
    );
    assert_eq!(pane.paragraph_motion_target_absolute(evicted, 1), None);
}

#[test]
fn paragraph_motion_finds_blank_rows_by_absolute_row() {
    let mut terminal = crate::vt::Terminal::new(10, 3, 1);
    write_numbered_lines(&mut terminal, 1_100);
    terminal.write(b"\r\npara\r\ngraph");
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
    let origin = pane
        .scroll_position()
        .expect("test precondition")
        .metrics
        .history_origin;
    // Rows: ..., 001099 on row 1099, blank on 1100, "para" on 1101.
    assert_eq!(
        pane.paragraph_motion_target_absolute(AbsRow(1_101), -1),
        Some(TerminalTextPoint {
            row: AbsRow(1_100),
            col: 0
        })
    );
    let para = AbsRow(1_101)
        .screen_row(origin)
        .expect("row remains retained");
    assert_eq!(
        pane.paragraph_motion_target(para, -1),
        Some(TerminalTextPoint {
            row: ScreenRow(para.0 - 1),
            col: 0
        })
    );
}

/// The streaming search reads the grid in chunks under short lock holds;
/// it must find exactly what a search of the whole buffer at once finds,
/// including a match whose soft-wrapped rows straddle a chunk boundary.
#[test]
fn chunked_search_matches_a_whole_buffer_search() {
    let mut terminal = crate::vt::Terminal::new(10, 3, 1_000_000);
    write_numbered_lines(&mut terminal, 2_046);
    terminal.write(b"abcdefghijklmnopqrstuvwxyz0123\r\n");
    for i in 3_000..3_100 {
        terminal.write(format!("{i:06}\r\n").as_bytes());
    }
    assert_eq!(
        terminal.history_origin(),
        AbsRow(0),
        "history must not be full"
    );
    let whole = RetainedTextBuffer::new(terminal.cols(), terminal.screen_text_rows());
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));

    let at = |row: u64| TerminalTextPoint {
        row: AbsRow(row),
        col: 0,
    };
    for (query, direction, cursor) in [
        ("00", TerminalSearchDirection::Forward, at(1_000)),
        ("00", TerminalSearchDirection::Backward, at(1_000)),
        ("00", TerminalSearchDirection::Forward, at(9_999)),
        ("00", TerminalSearchDirection::Backward, at(0)),
        (
            "abcdefghijklmnopqrstuvwxyz",
            TerminalSearchDirection::Forward,
            at(0),
        ),
        ("nothing", TerminalSearchDirection::Forward, at(0)),
    ] {
        let expected = whole.search_window(
            query,
            true,
            crate::vt::ActiveScreen::Primary,
            direction,
            cursor,
            None,
            16,
        );
        let actual = pane.search_text_window_absolute(query, true, direction, cursor, None, 16);
        assert_eq!(actual, expected, "{query} {direction:?} from {cursor:?}");
    }
    let word = pane.search_text_window_absolute(
        "abcdefghijklmnopqrstuvwxyz",
        true,
        TerminalSearchDirection::Forward,
        at(0),
        None,
        1,
    );
    assert_eq!(
        (word.matches[0].start, word.matches[0].end),
        (
            TerminalTextPoint {
                row: AbsRow(2_046),
                col: 0
            },
            TerminalTextPoint {
                row: AbsRow(2_048),
                col: 5
            }
        )
    );
}

/// The window kept while matches stream past agrees with slicing the
/// complete match list, for every target position and window size.
#[test]
fn match_window_agrees_with_the_complete_match_list() {
    let all: Vec<TerminalTextMatch<AbsRow>> = (0..40u64)
        .map(|row| TerminalTextMatch {
            start: TerminalTextPoint {
                row: AbsRow(row),
                col: 2,
            },
            end: TerminalTextPoint {
                row: AbsRow(row),
                col: 4,
            },
            source_fingerprint: row,
            scan_cols: 10,
            scan_screen: crate::vt::ActiveScreen::Primary,
        })
        .collect();
    let complete =
        |direction: TerminalSearchDirection, origin: TerminalTextPoint<AbsRow>, limit: usize| {
            let mut target = None;
            for (index, text_match) in all.iter().enumerate() {
                match direction {
                    TerminalSearchDirection::Forward
                        if target.is_none() && text_match.start > origin =>
                    {
                        target = Some(index);
                    }
                    TerminalSearchDirection::Backward if text_match.end < origin => {
                        target = Some(index);
                    }
                    _ => {}
                }
            }
            let total = all.len();
            let target = target.unwrap_or(match direction {
                TerminalSearchDirection::Forward => 0,
                TerminalSearchDirection::Backward => total - 1,
            });
            let retained = limit.min(total);
            let start = target.saturating_sub(retained / 2).min(total - retained);
            TerminalSearchWindow {
                matches: all[start..start + retained].to_vec(),
                current: Some(target - start),
                current_global: Some(target),
                total,
            }
        };
    for direction in [
        TerminalSearchDirection::Forward,
        TerminalSearchDirection::Backward,
    ] {
        for origin_row in [0u64, 1, 7, 20, 38, 39, 45] {
            for origin_col in [0u16, 3, 9] {
                for limit in [1usize, 2, 3, 7, 16, 39, 40, 100] {
                    let origin = TerminalTextPoint {
                        row: AbsRow(origin_row),
                        col: origin_col,
                    };
                    let mut window = MatchWindow {
                        direction,
                        origin,
                        limit,
                        total: 0,
                        target: None,
                        first: Vec::new(),
                        recent: VecDeque::new(),
                        boundary: None,
                        after: Vec::new(),
                    };
                    for text_match in &all {
                        window.push(*text_match);
                    }
                    assert_eq!(
                        window.finish(),
                        complete(direction, origin, limit),
                        "{direction:?} from {origin:?}, limit {limit}"
                    );
                }
            }
        }
    }
}

/// A full render draws every row and must not consume the dirty rows
/// the next patch still has to send.
#[test]
fn full_render_leaves_dirty_rows_for_the_next_patch() {
    let terminal = crate::vt::Terminal::new(8, 4, 100);
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
    let pane_id = PaneId::from_raw(1);
    pane.collect_dirty_patch(8, 4);
    pane.process_pty_bytes(pane_id, 0, b"\x1b[2;1HX");

    let backend = ratatui::backend::TestBackend::new(8, 4);
    let mut host = ratatui::Terminal::new(backend).expect("test precondition");
    host.draw(|frame| pane.render(frame, Rect::new(0, 0, 8, 4), false))
        .expect("test precondition");

    let TerminalDirtyPatchOutcome::Patch(patch) = pane.collect_dirty_patch(8, 4) else {
        panic!("the row written before the render must still be sent");
    };
    assert!(patch.rows.iter().any(|(y, _)| *y == 1));
}

/// Rows below a patch's area stay dirty, and so does the overall state:
/// a taller patch later still sends them.
#[test]
fn rows_below_a_patch_area_are_sent_by_a_later_taller_patch() {
    let terminal = crate::vt::Terminal::new(8, 6, 100);
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
    let pane_id = PaneId::from_raw(1);
    pane.collect_dirty_patch(8, 6);
    pane.process_pty_bytes(pane_id, 0, b"\x1b[2;3HX\x1b[5;4HY");

    let TerminalDirtyPatchOutcome::Patch(short) = pane.collect_dirty_patch(8, 3) else {
        panic!("expected a patch");
    };
    assert_eq!(short.rows.iter().map(|(y, _)| *y).collect::<Vec<_>>(), [1]);
    assert!(
        !matches!(
            pane.collect_dirty_patch(8, 3),
            TerminalDirtyPatchOutcome::Clean
        ),
        "a row is still waiting below the area"
    );
    let TerminalDirtyPatchOutcome::Patch(tall) = pane.collect_dirty_patch(8, 6) else {
        panic!("expected a patch");
    };
    assert_eq!(tall.rows.iter().map(|(y, _)| *y).collect::<Vec<_>>(), [4]);
    assert!(matches!(
        pane.collect_dirty_patch(8, 6),
        TerminalDirtyPatchOutcome::Clean
    ));
}

#[test]
fn default_color_changes_ask_for_an_owner_only_while_an_override_stands() {
    let terminal = crate::vt::Terminal::new(20, 3, 0);
    let pane = GhosttyPaneTerminal::new(terminal);
    let mut core = crate::vt::lock_terminal_core(&pane.core).expect("test precondition");
    core.terminal.write(b"\x1b]11;rgb:10/20/30\x07");
    assert!(note_default_color_change(&mut core));
    // Nothing new since.
    assert!(!note_default_color_change(&mut core));

    core.transient_default_color_owner_pgid = Some(42);
    core.terminal.write(b"\x1b]111\x07");
    assert!(!note_default_color_change(&mut core));
    assert_eq!(core.transient_default_color_owner_pgid, None);
}

#[test]
fn primary_history_is_unavailable_on_the_alternate_screen() {
    let terminal = crate::vt::Terminal::new(20, 3, 100_000);
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
    let pane_id = PaneId::from_raw(1);
    pane.process_pty_bytes(pane_id, 0, b"history one\r\nhistory two\r\nprompt");
    assert!(
        pane.primary_history_ansi()
            .is_some_and(|ansi| ansi.contains("history one"))
    );

    pane.process_pty_bytes(pane_id, 0, b"\x1b[?1049h\x1b[2J\x1b[Hfull-screen frame");
    assert_eq!(pane.primary_history_ansi(), None);

    pane.process_pty_bytes(pane_id, 0, b"\x1b[?1049l");
    assert!(
        pane.primary_history_ansi()
            .is_some_and(|ansi| ansi.contains("history one") && !ansi.contains("full-screen"))
    );
}

#[test]
fn screen_text_snapshot_copies_rows_only_on_the_alternate_screen() {
    let terminal = crate::vt::Terminal::new(20, 3, 100_000);
    let pane = GhosttyPaneTerminal::new(terminal);
    let pane_id = PaneId::from_raw(1);
    pane.process_pty_bytes(pane_id, 0, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

    let (screen, cols, rows) = pane.screen_text_snapshot().expect("snapshot");
    assert_eq!(screen, crate::vt::ActiveScreen::Primary);
    assert_eq!(cols, 20);
    assert!(rows.is_empty());

    pane.process_pty_bytes(pane_id, 0, b"\x1b[?1049h\x1b[2J\x1b[Hframe");
    let (screen, _, rows) = pane.screen_text_snapshot().expect("snapshot");
    assert_eq!(screen, crate::vt::ActiveScreen::Alternate);
    assert_eq!(rows.len(), 3);
}

#[test]
fn a_core_poisoned_off_the_reader_is_reported_to_the_reader() {
    let terminal = crate::vt::Terminal::new(20, 3, 0);
    let pane = std::sync::Arc::new(PaneTerminal::new(GhosttyPaneTerminal::new(terminal)));
    let pane_id = PaneId::from_raw(1);
    assert!(!pane.process_pty_bytes(pane_id, 0, b"before").core_poisoned);
    assert!(!pane.core_poisoned());

    // A render or API read panicking while it holds the core lock.
    let poisoner = std::sync::Arc::clone(&pane);
    let joined = std::thread::spawn(move || {
        let _core = crate::vt::lock_terminal_core(&poisoner.ghostty.core);
        panic!("panic while holding the core lock");
    })
    .join();
    assert!(joined.is_err(), "test precondition");

    // Visible without any output, for the actor's idle check.
    assert!(pane.core_poisoned());
    assert!(pane.process_pty_bytes(pane_id, 0, b"after").core_poisoned);
}
