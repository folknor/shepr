use super::*;

fn write_numbered_lines(terminal: &mut Terminal, count: usize) {
    for i in 0..count {
        terminal.write(format!("{i:06}\r\n").as_bytes());
    }
}

fn write_padded_lines(terminal: &mut Terminal, count: usize, width: usize) {
    let line = format!("{}\r\n", "x".repeat(width));
    terminal.write(line.repeat(count).as_bytes());
}

fn core_replies(terminal: &mut Terminal) -> Vec<Vec<u8>> {
    terminal
        .take_pty_responses()
        .into_iter()
        .filter_map(PtyResponse::into_core_bytes)
        .collect()
}

fn first_rendered_row_text(terminal: &Terminal) -> String {
    let mut render_state = RenderState::new().expect("test precondition");
    render_state.update(terminal).expect("test precondition");
    let mut row_iterator = RowIterator::new().expect("test precondition");
    let mut rows = render_state
        .populate_row_iterator(&mut row_iterator)
        .expect("test precondition");
    let mut row_cells = RowCells::new().expect("test precondition");
    let mut bytes = Vec::new();
    let mut cell_text = String::new();
    let mut row_text = String::new();

    assert!(rows.next());
    let mut cells = rows.populate_cells(&mut row_cells).expect("test precondition");
    while cells.next() {
        cells
            .grapheme_text_into(&mut bytes, &mut cell_text)
            .expect("test precondition");
        row_text.push_str(&cell_text);
    }
    row_text.trim_end().to_owned()
}

#[test]
fn scrollback_bytes_convert_to_bounded_line_counts() {
    assert_eq!(scrollback_lines(0, 80), 0);
    assert_eq!(scrollback_lines(1, 80), MIN_SCROLLBACK_LINES);
    let per_line = 80 * mem::size_of::<Cell>();
    assert_eq!(scrollback_lines(per_line * 5_000, 80), 5_000);
    assert_eq!(scrollback_lines(usize::MAX, 80), MAX_SCROLLBACK_LINES);
    // Narrower panes get proportionally more lines for the same budget.
    assert!(scrollback_lines(per_line * 5_000, 40) > scrollback_lines(per_line * 5_000, 80));
}

#[test]
fn unicode_width_helpers_match_terminal_layout_rules() {
    assert_eq!(unicode_codepoint_width('A' as u32), 1);
    assert_eq!(unicode_codepoint_width('\u{301}' as u32), 0);
    assert_eq!(unicode_codepoint_width('界' as u32), 2);
    assert_eq!(unicode_codepoint_width(0x11_0000), 1);

    let cases: &[(&[u32], usize, u8)] = &[
        (&[], 0, 0),
        (&['e' as u32, '\u{301}' as u32], 2, 1),
        (&['⚠' as u32, '\u{fe0f}' as u32], 2, 2),
        (&['⚠' as u32, '\u{fe0e}' as u32], 2, 1),
        (&['🇧' as u32, '🇷' as u32], 2, 2),
        (&['👍' as u32, '🏽' as u32], 2, 2),
        (
            &[
                '👨' as u32,
                '\u{200d}' as u32,
                '👩' as u32,
                '\u{200d}' as u32,
                '👧' as u32,
            ],
            5,
            2,
        ),
        (&[0x11_0000, 'A' as u32], 1, 1),
    ];
    for &(codepoints, consumed, width) in cases {
        assert_eq!(
            unicode_grapheme_width(codepoints),
            (consumed, width),
            "{codepoints:x?}"
        );
    }
}

#[test]
fn focus_encoding_matches_expected_sequences() {
    assert_eq!(encode_focus(FocusEvent::Gained).expect("test precondition"), b"\x1b[I");
    assert_eq!(encode_focus(FocusEvent::Lost).expect("test precondition"), b"\x1b[O");
}

#[test]
fn terminal_reports_pty_responses_and_pwd_changes() {
    let mut terminal = Terminal::new(8, 3, 100).expect("test precondition");

    terminal.write(b"\x1b[6n\x1b]7;file:///tmp/shepr\x07");

    let output = core_replies(&mut terminal).concat();
    assert_eq!(output, b"\x1b[1;1R");
    assert_eq!(terminal.take_pwd_changes(), [b"file:///tmp/shepr".to_vec()]);
}

#[test]
fn modes_and_kitty_flags_follow_terminal_state() {
    let mut terminal = Terminal::new(80, 24, 0).expect("test precondition");
    terminal.mode_set(1, true).expect("test precondition");
    terminal.write(b"\x1b[>1u\x1b[?1000h\x1b[?1006h");

    assert!(terminal.mode_get(1).expect("test precondition"));
    assert_eq!(terminal.kitty_keyboard_flags().expect("test precondition"), 1);
    assert!(terminal.mouse_tracking_enabled().expect("test precondition"));
    assert!(terminal.mode_get(1000).expect("test precondition"));
    assert!(terminal.mode_get(1006).expect("test precondition"));

    // X10 replaces the other tracking modes; enabling 1003 cancels X10 again.
    terminal.write(b"\x1b[?9h");
    assert!(terminal.mode_get(9).expect("test precondition"));
    assert!(!terminal.mode_get(1000).expect("test precondition"));
    assert!(terminal.mouse_tracking_enabled().expect("test precondition"));
    terminal.write(b"\x1b[?1003h");
    assert!(!terminal.mode_get(9).expect("test precondition"));
    assert!(terminal.mode_get(1003).expect("test precondition"));

    terminal.write(b"\x1b[<u");
    assert_eq!(terminal.kitty_keyboard_flags().expect("test precondition"), 0);
}

#[test]
fn adapter_modes_answer_decrqm_and_reset_on_ris() {
    let mut terminal = Terminal::new(20, 3, 0).expect("test precondition");
    terminal.write(b"\x1b[?1016h\x1b[?1016$p\x1b[?2031$p\x1b[?1004$p");
    assert_eq!(
        core_replies(&mut terminal),
        vec![
            b"\x1b[?1016;1$y".to_vec(),
            b"\x1b[?2031;2$y".to_vec(),
            b"\x1b[?1004;2$y".to_vec(),
        ]
    );

    terminal.write(b"\x1b[?2031h\x1bc");
    assert!(!terminal.mode_get(MODE_COLOR_SCHEME_REPORT).expect("test precondition"));
    assert!(!terminal.mode_get(MODE_MOUSE_SGR_PIXELS).expect("test precondition"));
}

#[test]
fn replies_keep_byte_order_across_core_and_adapter_sources() {
    let mut terminal = Terminal::new(20, 3, 0).expect("test precondition");
    terminal.set_color_scheme(Some(ColorScheme::Light));
    terminal.write(b"\x1b[5n\x1bP+q5463\x1b\\\x1b[?996n\x1b]10;?\x07\x1b[c");
    let replies = terminal.take_pty_responses();
    assert_eq!(replies.len(), 5);
    assert!(matches!(&replies[0], PtyResponse::Bytes(bytes) if bytes == b"\x1b[0n"));
    assert!(
        matches!(&replies[1], PtyResponse::Bytes(bytes) if bytes == b"\x1bP1+r5463\x1b\\")
    );
    assert!(matches!(&replies[2], PtyResponse::Bytes(bytes) if bytes == b"\x1b[?997;2n"));
    match &replies[3] {
        PtyResponse::ColorQuery(query) => {
            assert_eq!(query.target(), ColorQueryTarget::Foreground);
            // Nothing set a foreground yet.
            assert_eq!(query.core_color(), None);
            assert_eq!(
                query.encode(RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33
                }),
                b"\x1b]10;rgb:1111/2222/3333\x07"
            );
        }
        other => panic!("expected colour query, got {other:?}"),
    }
    assert!(matches!(&replies[4], PtyResponse::Bytes(bytes) if bytes.ends_with(b"c")));
}

#[test]
fn color_queries_report_child_overrides_and_palette_defaults() {
    let mut terminal = Terminal::new(20, 3, 0).expect("test precondition");
    terminal.write(b"\x1b]11;rgb:12/34/56\x07\x1b]11;?\x1b\\\x1b]4;1;?\x07");
    let replies = terminal.take_pty_responses();
    assert_eq!(replies.len(), 2);
    let PtyResponse::ColorQuery(background) = &replies[0] else {
        panic!("expected background query");
    };
    assert_eq!(
        background.core_color(),
        Some(RgbColor {
            r: 0x12,
            g: 0x34,
            b: 0x56
        })
    );
    let PtyResponse::ColorQuery(palette) = &replies[1] else {
        panic!("expected palette query");
    };
    assert_eq!(palette.target(), ColorQueryTarget::Palette(1));
    assert_eq!(palette.core_color(), Some(default_palette()[1]));
}

#[test]
fn window_size_reports_need_pixel_geometry() {
    let mut terminal = Terminal::new(80, 24, 0).expect("test precondition");
    terminal.write(b"\x1b[14t\x1b[16t\x1b[18t");
    assert!(core_replies(&mut terminal).is_empty());

    terminal.resize(80, 24, 9, 18).expect("test precondition");
    terminal.write(b"\x1b[14t\x1b[16t\x1b[18t");
    assert_eq!(
        core_replies(&mut terminal),
        vec![
            b"\x1b[4;432;720t".to_vec(),
            b"\x1b[6;18;9t".to_vec(),
            b"\x1b[8;24;80t".to_vec(),
        ]
    );
}

#[test]
fn in_band_resize_reports_on_enable_and_resize() {
    let mut terminal = Terminal::new(80, 24, 0).expect("test precondition");
    terminal.resize(80, 24, 9, 18).expect("test precondition");
    terminal.write(b"\x1b[?2048h");
    assert_eq!(
        core_replies(&mut terminal),
        vec![b"\x1b[48;24;80;432;720t".to_vec()]
    );
    terminal.resize(100, 40, 9, 18).expect("test precondition");
    assert_eq!(
        core_replies(&mut terminal),
        vec![b"\x1b[48;40;100;720;900t".to_vec()]
    );
}

#[test]
fn synchronized_output_buffers_until_end_or_timeout() {
    let mut terminal = Terminal::new(20, 3, 0).expect("test precondition");
    terminal.write(b"\x1b[?2026hhidden");
    assert!(terminal.mode_get(MODE_SYNCHRONIZED_OUTPUT).expect("test precondition"));
    assert!(terminal.synchronized_output_deadline().is_some());
    assert!(!terminal.flush_expired_synchronized_output());
    assert_eq!(
        terminal.read_text_viewport((0, 0), (19, 0), false).expect("test precondition"),
        ""
    );

    terminal.write(b"\x1b[?2026l");
    assert!(!terminal.mode_get(MODE_SYNCHRONIZED_OUTPUT).expect("test precondition"));
    assert_eq!(
        terminal.read_text_viewport((0, 0), (19, 0), false).expect("test precondition"),
        "hidden"
    );

    terminal.write(b"\x1b[?2026h forgotten");
    let deadline = terminal.synchronized_output_deadline().expect("test precondition");
    while Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(terminal.flush_expired_synchronized_output());
    assert!(!terminal.mode_get(MODE_SYNCHRONIZED_OUTPUT).expect("test precondition"));
    assert!(terminal
        .read_text_viewport((0, 0), (19, 0), false)
        .expect("test precondition")
        .contains("forgotten"));
}

#[test]
fn terminal_read_text_viewport_unwraps_soft_wrapped_selection() {
    let mut terminal = Terminal::new(5, 3, 0).expect("test precondition");
    terminal.write("1ABCD2EFGH3IJKL".as_bytes());

    let text = terminal.read_text_viewport((0, 1), (2, 2), false).expect("test precondition");
    assert_eq!(text, "2EFGH3IJ");
}

#[test]
fn terminal_extracts_viewport_hyperlink_uri() {
    let mut terminal = Terminal::new(20, 3, 0).expect("test precondition");
    terminal.write(b"\x1b]8;;https://example.com\x1b\\Link\x1b]8;;\x1b\\");

    assert_eq!(
        terminal.viewport_hyperlink_uri(0, 0).expect("test precondition").as_deref(),
        Some("https://example.com")
    );
    assert_eq!(terminal.viewport_hyperlink_uri(4, 0).expect("test precondition"), None);
}

#[test]
fn terminal_read_text_viewport_handles_wide_chars() {
    let mut terminal = Terminal::new(5, 3, 0).expect("test precondition");
    terminal.write("1A⚡".as_bytes());

    let full = terminal.read_text_viewport((0, 0), (3, 0), false).expect("test precondition");
    assert_eq!(full, "1A⚡");

    let through_wide_head = terminal.read_text_viewport((0, 0), (2, 0), false).expect("test precondition");
    assert_eq!(through_wide_head, "1A⚡");

    let wide_only = terminal.read_text_viewport((3, 0), (3, 0), false).expect("test precondition");
    assert_eq!(wide_only, "⚡");
}

#[test]
fn zero_max_scrollback_disables_history() {
    let mut terminal = Terminal::new(80, 3, 0).expect("test precondition");
    write_numbered_lines(&mut terminal, 3000);
    assert_eq!(terminal.scrollback_rows().expect("test precondition"), 0);
}

#[test]
fn max_scrollback_limit_bytes_retains_more_history_for_larger_limits() {
    let mut small = Terminal::new(80, 3, 1_000_000).expect("test precondition");
    let mut large = Terminal::new(80, 3, 10_000_000).expect("test precondition");

    write_padded_lines(&mut small, 1_250, 70);
    write_padded_lines(&mut large, 1_250, 70);

    let small_scrollback = small.scrollback_rows().expect("test precondition");
    let large_scrollback = large.scrollback_rows().expect("test precondition");

    assert!(
        large_scrollback > small_scrollback,
        "expected larger byte limit to retain more history, got small={small_scrollback}, large={large_scrollback}"
    );
}

#[test]
fn large_negative_scroll_delta_reaches_top_of_scrollback() {
    let mut terminal = Terminal::new(80, 3, 1_000_000).expect("test precondition");
    write_numbered_lines(&mut terminal, 1000);

    let before = terminal.scrollbar().expect("test precondition");
    assert!(before.total > before.len);

    terminal.scroll_viewport_bottom();
    terminal.scroll_viewport_delta(-10_000);

    let after = terminal.scrollbar().expect("test precondition");
    assert_eq!(after.offset, 0);
    assert_eq!(after.len, before.len);
}

#[test]
fn absolute_scroll_row_round_trips_and_clamps() {
    let mut terminal = Terminal::new(80, 3, 1_000_000).expect("test precondition");
    write_numbered_lines(&mut terminal, 1000);

    let before = terminal.scrollbar().expect("test precondition");
    let max_row = before.total.saturating_sub(before.len);
    assert!(max_row > 0);

    for row in [0, max_row / 2, max_row, usize::MAX] {
        terminal.scroll_viewport_row(row);
        let after = terminal.scrollbar().expect("test precondition");
        assert_eq!(after.offset, row.min(max_row));
        assert_eq!(after.len, before.len);
    }
}

#[test]
fn deep_scrollback_resize_preserves_unicode_and_hyperlinks() {
    use std::fmt::Write as _;

    let mut terminal = Terminal::new(20, 5, 100_000_000).expect("test precondition");
    let mut input = String::from("\x1b]8;;https://example.com\x1b\\FIRST 🇧🇷\x1b]8;;\x1b\\\r\n");
    for line in 0..70_000 {
        write!(input, "{line:05} 👨‍👩‍👧\r\n").expect("test precondition");
    }
    terminal.write(input.as_bytes());

    assert!(terminal.scrollback_rows().expect("test precondition") > u16::MAX as usize);
    terminal.scroll_viewport_delta(-100_000);
    assert_eq!(terminal.scrollbar().expect("test precondition").offset, 0);
    assert!(terminal
        .read_text_viewport((0, 0), (19, 0), false)
        .expect("test precondition")
        .starts_with("FIRST 🇧🇷"));
    assert_eq!(
        terminal.viewport_hyperlink_uri(0, 0).expect("test precondition").as_deref(),
        Some("https://example.com")
    );

    terminal.resize(10, 5, 8, 16).expect("test precondition");
    terminal.scroll_viewport_delta(-100_000);
    let metrics = terminal.scrollbar().expect("test precondition");
    assert_eq!(metrics.offset, 0);
    assert_eq!(metrics.len, 5);
    assert!(terminal
        .read_text_viewport((0, 0), (9, 0), false)
        .expect("test precondition")
        .starts_with("FIRST"));
    assert_eq!(
        terminal.viewport_hyperlink_uri(0, 0).expect("test precondition").as_deref(),
        Some("https://example.com")
    );
}

#[test]
fn raw_resize_preserves_content_without_replaying_terminal_effects() {
    let mut terminal = Terminal::new(20, 6, 100_000).expect("test precondition");
    terminal.write(b"header\r\n\x1b[6;1Htail\x1b[6;18H");
    for (cols, rows) in [(10, 3), (30, 8), (8, 4), (20, 6)] {
        terminal.resize(cols, rows, 8, 16).expect("test precondition");
        terminal.write(b"X");
        let text = terminal
            .read_text_screen(
                (0, 0),
                (cols - 1, terminal.total_rows().expect("test precondition") as u32 - 1),
                false,
            )
            .expect("test precondition");
        assert!(
            text.contains("tail"),
            "lost tail after {cols}x{rows}: {text:?}"
        );
        assert_eq!(text.matches("tail").count(), 1);
        assert!(terminal.cursor_y().expect("test precondition") < rows);
    }
    terminal.write(b"\x1b[2J\x1b[H");
    terminal.resize(12, 3, 8, 16).expect("test precondition");
    assert!(terminal
        .read_text_viewport((0, 0), (11, 2), false)
        .expect("test precondition")
        .trim()
        .is_empty());
}

#[test]
fn clipboard_queries_never_disclose_contents_and_split_writes_complete_once() {
    for suffix in [b"\x07".as_slice(), b"\x1b\\".as_slice()] {
        let bytes = [b"\x1b]52;c;YQBi".as_slice(), suffix].concat();
        for split in 0..=bytes.len() {
            let mut terminal = Terminal::new(10, 3, 0).expect("test precondition");
            terminal.write(&bytes[..split]);
            terminal.write(&bytes[split..]);
            assert_eq!(terminal.take_clipboard_writes(), vec![b"a\0b".to_vec()]);
            terminal.write(b"\x1b]52;c;?\x07\x1b]52;p;YQBi\x07");
            assert!(terminal.take_clipboard_writes().is_empty());
            assert!(terminal.take_pty_responses().is_empty());
        }
    }
}

#[test]
fn osc52_writes_complete_for_bel_and_st_without_queries() {
    let mut terminal = Terminal::new(10, 5, 0).expect("test precondition");
    terminal.write(b"\x1b]52;c;aGVs");
    assert!(terminal.take_clipboard_writes().is_empty());
    terminal.write(b"bG8=\x07");
    assert_eq!(terminal.take_clipboard_writes(), vec![b"hello".to_vec()]);

    terminal.write(b"\x1b]52;c;d29ybGQ=\x1b\\");
    assert_eq!(terminal.take_clipboard_writes(), vec![b"world".to_vec()]);

    terminal.write(b"\x1b]52;c;?\x07");
    assert!(terminal.take_clipboard_writes().is_empty());

    terminal.write(b"\x1b]52;c;\x07");
    assert!(terminal.take_clipboard_writes().is_empty());
}

#[test]
fn active_screen_and_cursor_visibility_contract() {
    let mut terminal = Terminal::new(12, 3, 0).expect("test precondition");
    let mut render_state = RenderState::new().expect("test precondition");

    terminal.write(b"primary");
    assert_eq!(terminal.active_screen().expect("test precondition"), ActiveScreen::Primary);
    assert_eq!(
        terminal.read_text_viewport((0, 0), (6, 0), false).expect("test precondition"),
        "primary"
    );

    render_state.update(&terminal).expect("test precondition");
    assert!(render_state.cursor().expect("test precondition").visible);
    terminal.write(b"\x1b[?25l");
    render_state.update(&terminal).expect("test precondition");
    assert!(!render_state.cursor().expect("test precondition").visible);

    terminal.write(b"\x1b[?1049h\x1b[HALT");
    assert_eq!(terminal.active_screen().expect("test precondition"), ActiveScreen::Alternate);
    assert_eq!(
        terminal.read_text_viewport((0, 0), (2, 0), false).expect("test precondition"),
        "ALT"
    );

    terminal.write(b"\x1b[?1049l");
    assert_eq!(terminal.active_screen().expect("test precondition"), ActiveScreen::Primary);
    assert_eq!(
        terminal.read_text_viewport((0, 0), (6, 0), false).expect("test precondition"),
        "primary"
    );
}

#[test]
fn terminal_and_render_state_smoke_test() {
    let mut terminal = Terminal::new(8, 3, 100).expect("test precondition");
    assert_eq!(terminal.cols().expect("test precondition"), 8);
    assert_eq!(terminal.rows().expect("test precondition"), 3);

    terminal.write(b"hello\r\nworld");

    let mut render_state = RenderState::new().expect("test precondition");
    render_state.update(&terminal).expect("test precondition");
    assert_eq!(render_state.cols().expect("test precondition"), 8);
    assert_eq!(render_state.rows().expect("test precondition"), 3);
    assert_ne!(render_state.dirty().expect("test precondition"), Dirty::Clean);

    let mut row_iterator = RowIterator::new().expect("test precondition");
    let mut row_iter = render_state
        .populate_row_iterator(&mut row_iterator)
        .expect("test precondition");
    let mut row_cells = RowCells::new().expect("test precondition");

    let mut found_hello = false;
    let mut found_world = false;
    let mut row_index = 0usize;
    while row_iter.next() {
        let _ = row_iter.dirty().expect("test precondition");
        let mut cells = row_iter.populate_cells(&mut row_cells).expect("test precondition");
        let mut line = String::new();
        while cells.next() {
            let text = cells.grapheme_text().expect("test precondition");
            if text.is_empty() {
                line.push(' ');
            } else {
                line.push_str(&text);
            }
        }
        let trimmed = line.trim_end().to_string();
        if row_index == 0 {
            found_hello = trimmed.starts_with("hello");
        }
        if row_index == 1 {
            found_world = trimmed.starts_with("world");
        }
        row_index += 1;
    }

    assert!(found_hello);
    assert!(found_world);

    render_state.set_dirty(Dirty::Clean).expect("test precondition");
    assert_eq!(render_state.dirty().expect("test precondition"), Dirty::Clean);
}

#[test]
fn render_cells_preserve_issue_453_unicode_payload_exactly() {
    const PAYLOAD: &str = "README 👨‍👩‍👧‍👦 🧑‍💻 ✅ ⚡ 漢字 café é 🏳️‍🌈 🚀";
    let mut terminal = Terminal::new(80, 3, 100).expect("test precondition");
    terminal.write(format!("{PAYLOAD}\r\n").as_bytes());

    assert_eq!(first_rendered_row_text(&terminal), PAYLOAD);
}

#[test]
fn screen_text_rows_preserve_wrap_and_grapheme_cells() {
    let mut terminal = Terminal::new(5, 3, 100).expect("test precondition");
    terminal.write("abcdef\r\n界e\u{301}".as_bytes());

    let rows = terminal.screen_text_rows().expect("test precondition");

    assert_eq!(rows.len(), 3);
    assert!(rows[0].soft_wrapped);
    assert!(!rows[0].wrap_continuation);
    assert!(!rows[1].soft_wrapped);
    assert!(rows[1].wrap_continuation);
    assert!(!rows[2].wrap_continuation);
    assert_eq!(rows[2].cells[0].wide, CellWide::Wide);
    assert_eq!(rows[2].cells[0].graphemes, vec!['界' as u32]);
    assert_eq!(rows[2].cells[1].wide, CellWide::SpacerTail);
    assert_eq!(rows[2].cells[2].graphemes, vec!['e' as u32, 0x301]);
}

#[test]
fn render_state_row_dirty_can_be_cleared_independently() {
    let mut terminal = Terminal::new(8, 3, 100).expect("test precondition");
    let mut render_state = RenderState::new().expect("test precondition");

    render_state.update(&terminal).expect("test precondition");
    {
        let mut row_iterator = RowIterator::new().expect("test precondition");
        let mut rows = render_state
            .populate_row_iterator(&mut row_iterator)
            .expect("test precondition");
        while rows.next() {
            rows.clear_dirty().expect("test precondition");
            assert!(!rows.dirty().expect("test precondition"));
        }
    }
    {
        let mut iterator = RowIterator::new().expect("test precondition");
        let mut rows = render_state.populate_row_iterator(&mut iterator).expect("test precondition");
        assert_eq!(rows.next_dirty(), Some(0));
        assert_eq!(rows.next_dirty(), Some(1));
        assert_eq!(rows.next_dirty(), Some(2));
        assert_eq!(rows.next_dirty(), None);
    }
    render_state.set_dirty(Dirty::Clean).expect("test precondition");
    assert_eq!(render_state.dirty().expect("test precondition"), Dirty::Clean);
    {
        let mut iterator = RowIterator::new().expect("test precondition");
        let mut rows = render_state.populate_row_iterator(&mut iterator).expect("test precondition");
        assert_eq!(rows.next_dirty(), None);
    }

    terminal.write(b"A");
    render_state.update(&terminal).expect("test precondition");
    assert_eq!(render_state.dirty().expect("test precondition"), Dirty::Partial);

    let mut dirty_rows = 0usize;
    {
        let mut row_iterator = RowIterator::new().expect("test precondition");
        let mut rows = render_state
            .populate_row_iterator(&mut row_iterator)
            .expect("test precondition");
        while let Some(y) = rows.next_dirty() {
            assert_eq!(y, 0);
            assert!(rows.dirty().expect("test precondition"));
            dirty_rows += 1;
            rows.clear_dirty().expect("test precondition");
            assert!(!rows.dirty().expect("test precondition"));
        }
    }
    assert_eq!(dirty_rows, 1);
    assert_eq!(render_state.dirty().expect("test precondition"), Dirty::Partial);

    render_state.set_dirty(Dirty::Clean).expect("test precondition");
    assert_eq!(render_state.dirty().expect("test precondition"), Dirty::Clean);
}

#[test]
fn scrolling_the_viewport_marks_every_row_dirty() {
    let mut terminal = Terminal::new(8, 3, 100).expect("test precondition");
    write_numbered_lines(&mut terminal, 10);
    let mut render_state = RenderState::new().expect("test precondition");
    render_state.update(&terminal).expect("test precondition");
    render_state.clean().expect("test precondition");

    terminal.scroll_viewport_delta(-2);
    render_state.update(&terminal).expect("test precondition");
    assert_eq!(render_state.dirty().expect("test precondition"), Dirty::Full);
    // The cursor sits on the bottom row, which is now two rows below the viewport.
    assert_eq!(render_state.cursor().expect("test precondition").viewport, None);
}

#[test]
fn row_selection_returns_none_without_selection() {
    let terminal = Terminal::new(8, 3, 100).expect("test precondition");
    let mut render_state = RenderState::new().expect("test precondition");
    render_state.update(&terminal).expect("test precondition");

    let mut row_iterator = RowIterator::new().expect("test precondition");
    let mut rows = render_state
        .populate_row_iterator(&mut row_iterator)
        .expect("test precondition");
    assert!(rows.next());
    assert_eq!(rows.selection().expect("test precondition"), None);
}

#[test]
fn row_cell_basic_data_reports_palette_style() {
    let mut terminal = Terminal::new(8, 3, 100).expect("test precondition");
    terminal.write(b"\x1b[31mA\x1b[0m");

    let mut render_state = RenderState::new().expect("test precondition");
    render_state.update(&terminal).expect("test precondition");

    let mut row_iterator = RowIterator::new().expect("test precondition");
    let mut rows = render_state
        .populate_row_iterator(&mut row_iterator)
        .expect("test precondition");
    assert!(rows.next());

    let mut row_cells = RowCells::new().expect("test precondition");
    let mut cells = rows.populate_cells(&mut row_cells).expect("test precondition");
    assert!(cells.next());

    let basic = cells.basic_data().expect("test precondition");
    assert_eq!(basic.wide, CellWide::Narrow);
    assert!(basic.has_styling);
    assert_eq!(basic.style.fg_color, Some(CellColor::Palette(1)));
    assert!(!basic.has_hyperlink);
    assert_eq!(cells.fg_color().expect("test precondition"), Some(default_palette()[1]));
    assert_eq!(cells.bg_color().expect("test precondition"), None);
}

#[test]
fn clear_screen_keeps_the_cursor_line_and_drops_history() {
    let mut terminal = Terminal::new(10, 4, 100_000).expect("test precondition");
    write_numbered_lines(&mut terminal, 20);
    terminal.write(b"$ prompt");
    assert!(terminal.scrollback_rows().expect("test precondition") > 0);

    assert!(terminal.clear_screen());

    assert_eq!(terminal.scrollback_rows().expect("test precondition"), 0);
    assert_eq!(terminal.cursor_y().expect("test precondition"), 0);
    assert_eq!(
        terminal.read_text_viewport((0, 0), (9, 3), false).expect("test precondition"),
        "$ prompt"
    );
}

/// Formats the whole screen as unwrapped VT (as history persistence does),
/// replays it into a fresh terminal, and compares cells and styles.
#[test]
fn vt_history_round_trips_through_the_parser() {
    let mut source = Terminal::new(12, 4, 100_000).expect("test precondition");
    source.write(
        "plain \x1b[1;38;5;196mbold-red\x1b[0m \x1b[4:3;58;2;1;2;3mcurly\x1b[0m\r\n\
         \x1b]8;id=x;https://example.test\x1b\\link\x1b]8;;\x1b\\ 界e\u{301}\r\n\
         \x1b[48;2;9;8;7mwrapped-background-row\x1b[0m\r\n\
         last"
            .as_bytes(),
    );
    let total = source.total_rows().expect("test precondition") as u32;
    let ansi = source
        .read_ansi_screen((0, 0), (11, total - 1), false, true)
        .expect("test precondition");

    let mut restored = Terminal::new(12, 4, 100_000).expect("test precondition");
    restored.write(ansi.as_bytes());

    assert_eq!(
        restored.screen_text_rows().expect("test precondition"),
        source.screen_text_rows().expect("test precondition"),
        "{ansi:?}"
    );
    let source_rows = restored.total_rows().expect("test precondition");
    assert_eq!(source_rows, source.total_rows().expect("test precondition"));
    let styles = |terminal: &Terminal| {
        let grid = terminal.term.grid();
        let history = terminal.term.history_size() as i32;
        let mut styles = Vec::new();
        for y in 0..terminal.term.total_lines() as i32 {
            for x in 0..grid.columns() {
                let cell = &grid[Line(y - history)][Column(x)];
                styles.push((
                    cell_style(cell),
                    cell.hyperlink().map(|link| link.uri().to_owned()),
                ));
            }
        }
        styles
    };
    assert_eq!(styles(&restored), styles(&source), "{ansi:?}");
}

#[test]
fn plain_reads_trim_trailing_blank_lines_and_spaces() {
    let mut terminal = Terminal::new(10, 4, 0).expect("test precondition");
    terminal.write(b"a  \r\n\r\nb   ");
    assert_eq!(
        terminal.read_text_viewport((0, 0), (9, 3), false).expect("test precondition"),
        "a\n\nb"
    );
    assert_eq!(
        terminal.read_text_viewport((0, 3), (9, 3), false).expect("test precondition"),
        ""
    );
}
