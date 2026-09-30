use super::*;
use crate::limits::{KEYBOARD_MODE_STACK_MAX_DEPTH, MAX_SCROLLBACK_LINES, MIN_SCROLLBACK_LINES};

fn vp(col: u16, row: u16) -> Point<ViewportRow> {
    Point::new(ViewportRow(row), col)
}

fn sr(col: u16, row: usize) -> Point<ScreenRow> {
    Point::new(ScreenRow(row), col)
}

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
    let mut render_state = RenderState::new();
    render_state.update(terminal);
    let mut cell_text = String::new();
    let mut row_text = String::new();

    let row = render_state.iter_rows().next().expect("terminal has rows");
    for cells in row.cells() {
        cells.grapheme_text_into(&mut cell_text);
        // alacritty cannot tell a printed space from a blank cell, so both come
        // back empty; only wide-char spacers really contribute no text.
        if cell_text.is_empty() && cells.wide() == CellWide::Narrow {
            row_text.push(' ');
        } else {
            row_text.push_str(&cell_text);
        }
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
    assert_eq!(unicode_codepoint_width('\u{ff9e}' as u32), 1);
    assert_eq!(unicode_codepoint_width('\u{ff9f}' as u32), 1);
    assert_eq!(unicode_codepoint_width('界' as u32), 2);
    assert_eq!(unicode_codepoint_width(0x11_0000), 1);
    assert_eq!(unicode_text_width("\u{263a}\u{fe0f}"), 1);
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    assert_eq!(unicode_text_width(family), 6);
    assert_eq!(unicode_text_width("ｶﾞx"), 3);
    assert_eq!(
        unicode_display_units(family).collect::<Vec<_>>(),
        vec![
            ("\u{1f468}\u{200d}", 2),
            ("\u{1f469}\u{200d}", 2),
            ("\u{1f467}", 2),
        ]
    );
}

#[test]
fn focus_encoding_matches_expected_sequences() {
    assert_eq!(encode_focus(FocusEvent::Gained), b"\x1b[I");
    assert_eq!(encode_focus(FocusEvent::Lost), b"\x1b[O");
}

#[test]
fn terminal_reports_pty_responses_and_pwd_changes() {
    let mut terminal = Terminal::new(8, 3, 100);

    terminal.write(b"\x1b[6n\x1b]7;file:///tmp/shepr\x07");

    let output = core_replies(&mut terminal).concat();
    assert_eq!(output, b"\x1b[1;1R");
    assert_eq!(
        terminal.take_pwd_changes(),
        [WorkingDirectoryReport(b"file:///tmp/shepr".to_vec())]
    );
}

#[test]
fn modes_and_kitty_flags_follow_terminal_state() {
    let mut terminal = Terminal::new(80, 24, 0);
    terminal
        .mode_set(DecMode::ApplicationCursorKeys, true)
        .expect("test precondition");
    terminal.write(b"\x1b[>1u\x1b[?1000h\x1b[?1006h");
    terminal.write(b"\x1b[?12h\x1b[?1042h");

    assert!(terminal.mode_get(DecMode::ApplicationCursorKeys));
    assert!(terminal.mode_get(DecMode::CursorBlink));
    assert!(terminal.mode_get(DecMode::UrgencyHints));
    assert_eq!(terminal.kitty_keyboard_flags(), 1);
    assert!(terminal.mouse_tracking_enabled());
    assert!(terminal.mode_get(DecMode::MousePressRelease));
    assert!(terminal.mode_get(DecMode::MouseSgr));

    // X10 replaces the other tracking modes; enabling 1003 cancels X10 again.
    terminal.write(b"\x1b[?9h");
    assert!(terminal.mode_get(DecMode::X10Mouse));
    assert!(!terminal.mode_get(DecMode::MousePressRelease));
    assert!(terminal.mouse_tracking_enabled());
    terminal.write(b"\x1b[?1003h");
    assert!(!terminal.mode_get(DecMode::X10Mouse));
    assert!(terminal.mode_get(DecMode::MouseAnyMotion));

    terminal.write(b"\x1b[<u");
    terminal.write(b"\x1b[?12l\x1b[?1042l");
    assert!(!terminal.mode_get(DecMode::CursorBlink));
    assert!(!terminal.mode_get(DecMode::UrgencyHints));
    assert_eq!(terminal.kitty_keyboard_flags(), 0);
}

#[test]
fn adapter_modes_answer_decrqm_and_reset_on_ris() {
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(b"\x1b[?1005h\x1b[?1016h");
    assert!(!terminal.mode_get(DecMode::MouseUtf8));
    assert!(terminal.mode_get(DecMode::MouseSgrPixels));
    terminal.write(b"\x1b[?1005h");
    assert!(terminal.mode_get(DecMode::MouseUtf8));
    assert!(!terminal.mode_get(DecMode::MouseSgrPixels));

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
    assert!(!terminal.mode_get(DecMode::ColorSchemeReport));
    assert!(!terminal.mode_get(DecMode::MouseSgrPixels));
}

#[test]
fn replies_keep_byte_order_across_core_and_adapter_sources() {
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.set_color_scheme(Some(ColorScheme::Light));
    terminal.write(b"\x1b[5n\x1bP+q5463\x1b\\\x1b[?996n\x1b]10;?\x07\x1b[c");
    let replies = terminal.take_pty_responses();
    assert_eq!(replies.len(), 5);
    assert!(matches!(&replies[0], PtyResponse::Bytes(bytes) if bytes == b"\x1b[0n"));
    assert!(matches!(&replies[1], PtyResponse::Bytes(bytes) if bytes == b"\x1bP1+r5463\x1b\\"));
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
    let mut terminal = Terminal::new(20, 3, 0);
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
fn pixel_size_reports_need_pixel_geometry_but_character_size_does_not() {
    let mut terminal = Terminal::new(80, 24, 0);
    terminal.write(b"\x1b[14t\x1b[16t\x1b[18t");
    assert_eq!(core_replies(&mut terminal), vec![b"\x1b[8;24;80t".to_vec()]);

    terminal.resize(shepr_core::geometry::PaneGeometry::new(80, 24, 9, 18));
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

/// Pixel replies are bounded by the same u16 geometry reported by TIOCGWINSZ.
#[test]
fn text_area_pixel_report_matches_winsize_limits_for_large_cells() {
    let mut terminal = Terminal::new(80, 24, 0);
    terminal.resize(shepr_core::geometry::PaneGeometry::new(
        80, 24, 100_000, 100_000,
    ));
    terminal.write(b"\x1b[14t\x1b[?2048h");
    assert_eq!(
        core_replies(&mut terminal),
        vec![
            b"\x1b[4;65535;65535t".to_vec(),
            b"\x1b[48;24;80;65535;65535t".to_vec(),
        ]
    );
}

/// vte buffers a synchronized update and replays it at ESU; mode changes and
/// replies the adapter handles must follow the replayed order, not arrival.
#[test]
fn adapter_modes_and_replies_keep_byte_order_inside_synchronized_updates() {
    // X10 set after 1000 replaces it, even when both arrive inside a frame.
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(b"\x1b[?2026h\x1b[?1000h\x1b[?9h\x1b[?2026l");
    assert!(terminal.mode_get(DecMode::X10Mouse));
    assert!(!terminal.mode_get(DecMode::MousePressRelease));

    // DECRQM reports the state at its own position in the frame.
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(b"\x1b[?2026h\x1b[?1016$p\x1b[?1016h\x1b[?1016$p\x1b[?2026l");
    assert_eq!(
        core_replies(&mut terminal),
        vec![b"\x1b[?1016;2$y".to_vec(), b"\x1b[?1016;1$y".to_vec()]
    );

    // RIS inside a frame resets adapter modes set before it, not after it.
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(b"\x1b[?2026h\x1b[?2031h\x1bc\x1b[?1016h\x1b[?2026l");
    assert!(!terminal.mode_get(DecMode::ColorSchemeReport));
    assert!(terminal.mode_get(DecMode::MouseSgrPixels));

    // The in-band resize report follows a DSR requested earlier in the frame,
    // and nothing is answered before ESU.
    let mut terminal = Terminal::new(80, 24, 0);
    terminal.resize(shepr_core::geometry::PaneGeometry::new(80, 24, 9, 18));
    terminal.write(b"\x1b[?2026h\x1b[5n\x1b[?2048h");
    assert!(core_replies(&mut terminal).is_empty());
    terminal.write(b"\x1b[?2026l");
    assert_eq!(
        core_replies(&mut terminal),
        vec![b"\x1b[0n".to_vec(), b"\x1b[48;24;80;432;720t".to_vec()]
    );
}

#[test]
fn modify_other_keys_level_is_reported() {
    let mut terminal = Terminal::new(8, 2, 0);
    terminal.write(b"\x1b[?4m\x1b[>4;2m\x1b[?4m");
    assert_eq!(
        core_replies(&mut terminal),
        vec![b"\x1b[>4;0m".to_vec(), b"\x1b[>4;2m".to_vec()]
    );
}

#[test]
fn in_band_resize_reports_on_enable_and_resize() {
    let mut terminal = Terminal::new(80, 24, 0);
    terminal.resize(shepr_core::geometry::PaneGeometry::new(80, 24, 9, 18));
    terminal.write(b"\x1b[?2048h");
    assert_eq!(
        core_replies(&mut terminal),
        vec![b"\x1b[48;24;80;432;720t".to_vec()]
    );
    terminal.resize(shepr_core::geometry::PaneGeometry::new(100, 40, 9, 18));
    assert_eq!(
        core_replies(&mut terminal),
        vec![b"\x1b[48;40;100;720;900t".to_vec()]
    );
}

#[test]
fn synchronized_output_buffers_until_end_or_timeout() {
    let start = Instant::now();
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write_at(b"\x1b[?2026hhidden", start);
    assert!(terminal.mode_get(DecMode::SynchronizedOutput));
    let deadline = start + std::time::Duration::from_millis(150);
    assert_eq!(terminal.synchronized_output_deadline(), Some(deadline));
    assert!(!terminal.tick(deadline - std::time::Duration::from_millis(1)));
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(19, 0), false)
            .expect("test precondition"),
        ""
    );

    terminal.write_at(
        b"\x1b[?2026l",
        deadline - std::time::Duration::from_millis(1),
    );
    assert!(!terminal.mode_get(DecMode::SynchronizedOutput));
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(19, 0), false)
            .expect("test precondition"),
        "hidden"
    );

    let second_start = deadline + std::time::Duration::from_millis(1);
    terminal.write_at(b"\x1b[?2026h forgotten", second_start);
    let second_deadline = second_start + std::time::Duration::from_millis(150);
    assert_eq!(
        terminal.synchronized_output_deadline(),
        Some(second_deadline)
    );
    assert!(terminal.tick(second_deadline));
    assert!(!terminal.mode_get(DecMode::SynchronizedOutput));
    assert!(
        terminal
            .read_text_viewport(vp(0, 0), vp(19, 0), false)
            .expect("test precondition")
            .contains("forgotten")
    );
}

#[test]
fn terminal_read_text_viewport_unwraps_soft_wrapped_selection() {
    let mut terminal = Terminal::new(5, 3, 0);
    terminal.write("1ABCD2EFGH3IJKL".as_bytes());

    let text = terminal
        .read_text_viewport(vp(0, 1), vp(2, 2), false)
        .expect("test precondition");
    assert_eq!(text, "2EFGH3IJ");
}

#[test]
fn terminal_extracts_viewport_hyperlink_uri() {
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(b"\x1b]8;;https://example.com\x1b\\Link\x1b]8;;\x1b\\");

    assert_eq!(
        terminal
            .viewport_hyperlink_uri(0, ViewportRow(0))
            .expect("test precondition")
            .as_deref(),
        Some("https://example.com")
    );
    assert_eq!(
        terminal
            .viewport_hyperlink_uri(4, ViewportRow(0))
            .expect("test precondition"),
        None
    );
}

#[test]
fn terminal_read_text_viewport_handles_wide_chars() {
    let mut terminal = Terminal::new(5, 3, 0);
    terminal.write("1A\u{26A1}".as_bytes());

    let full = terminal
        .read_text_viewport(vp(0, 0), vp(3, 0), false)
        .expect("test precondition");
    assert_eq!(full, "1A\u{26A1}");

    let through_wide_head = terminal
        .read_text_viewport(vp(0, 0), vp(2, 0), false)
        .expect("test precondition");
    assert_eq!(through_wide_head, "1A\u{26A1}");

    let wide_only = terminal
        .read_text_viewport(vp(3, 0), vp(3, 0), false)
        .expect("test precondition");
    assert_eq!(wide_only, "\u{26A1}");
}

#[test]
fn zero_max_scrollback_disables_history() {
    let mut terminal = Terminal::new(80, 3, 0);
    write_numbered_lines(&mut terminal, 3000);
    assert_eq!(terminal.scrollback_rows(), 0);
}

#[test]
fn max_scrollback_limit_bytes_retains_more_history_for_larger_limits() {
    let mut small = Terminal::new(80, 3, 1_000_000);
    let mut large = Terminal::new(80, 3, 10_000_000);

    write_padded_lines(&mut small, 1_250, 70);
    write_padded_lines(&mut large, 1_250, 70);

    let small_scrollback = small.scrollback_rows();
    let large_scrollback = large.scrollback_rows();

    assert!(
        large_scrollback > small_scrollback,
        "expected larger byte limit to retain more history, got small={small_scrollback}, large={large_scrollback}"
    );
}

#[test]
fn large_negative_scroll_delta_reaches_top_of_scrollback() {
    let mut terminal = Terminal::new(80, 3, 1_000_000);
    write_numbered_lines(&mut terminal, 1000);

    let before = terminal.scrollbar();
    assert!(before.total > before.len);

    terminal.scroll_viewport_bottom();
    terminal.scroll_viewport_delta(-10_000);

    let after = terminal.scrollbar();
    assert_eq!(after.offset, 0);
    assert_eq!(after.len, before.len);
}

#[test]
fn absolute_scroll_row_round_trips_and_clamps() {
    let mut terminal = Terminal::new(80, 3, 1_000_000);
    write_numbered_lines(&mut terminal, 1000);

    let before = terminal.scrollbar();
    let max_row = before.total.saturating_sub(before.len);
    assert!(max_row > 0);

    for row in [0, max_row / 2, max_row, usize::MAX] {
        terminal.scroll_viewport_row(ScreenRow(row));
        let after = terminal.scrollbar();
        assert_eq!(after.offset, row.min(max_row));
        assert_eq!(after.len, before.len);
    }
}

#[test]
fn deep_scrollback_resize_preserves_unicode_and_hyperlinks() {
    use std::fmt::Write as _;

    let mut terminal = Terminal::new(20, 5, 100_000_000);
    let mut input =
        String::from("\x1b]8;;https://example.com\x1b\\FIRST \u{1F1E7}\u{1F1F7}\x1b]8;;\x1b\\\r\n");
    for line in 0..70_000 {
        write!(
            input,
            "{line:05} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\r\n"
        )
        .expect("test precondition");
    }
    terminal.write(input.as_bytes());

    assert!(terminal.scrollback_rows() > u16::MAX as usize);
    terminal.scroll_viewport_delta(-100_000);
    assert_eq!(terminal.scrollbar().offset, 0);
    assert!(
        terminal
            .read_text_viewport(vp(0, 0), vp(19, 0), false)
            .expect("test precondition")
            .starts_with("FIRST \u{1F1E7}\u{1F1F7}")
    );
    assert_eq!(
        terminal
            .viewport_hyperlink_uri(0, ViewportRow(0))
            .expect("test precondition")
            .as_deref(),
        Some("https://example.com")
    );

    terminal.resize(shepr_core::geometry::PaneGeometry::new(10, 5, 8, 16));
    terminal.scroll_viewport_delta(-100_000);
    let metrics = terminal.scrollbar();
    assert_eq!(metrics.offset, 0);
    assert_eq!(metrics.len, 5);
    assert!(
        terminal
            .read_text_viewport(vp(0, 0), vp(9, 0), false)
            .expect("test precondition")
            .starts_with("FIRST")
    );
    assert_eq!(
        terminal
            .viewport_hyperlink_uri(0, ViewportRow(0))
            .expect("test precondition")
            .as_deref(),
        Some("https://example.com")
    );
}

#[test]
fn raw_resize_preserves_content_without_replaying_terminal_effects() {
    let mut terminal = Terminal::new(20, 6, 100_000);
    terminal.write(b"header\r\n\x1b[6;1Htail\x1b[6;18H");
    for (cols, rows) in [(10, 3), (30, 8), (8, 4), (20, 6)] {
        terminal.resize(shepr_core::geometry::PaneGeometry::new(cols, rows, 8, 16));
        terminal.write(b"X");
        let text = terminal
            .read_text_screen(
                sr(0, 0),
                sr(cols - 1, terminal.total_rows().saturating_sub(1)),
                false,
            )
            .expect("test precondition");
        assert!(
            text.contains("tail"),
            "lost tail after {cols}x{rows}: {text:?}"
        );
        assert_eq!(text.matches("tail").count(), 1);
        assert!(terminal.cursor_y() < rows);
    }
    terminal.write(b"\x1b[2J\x1b[H");
    terminal.resize(shepr_core::geometry::PaneGeometry::new(12, 3, 8, 16));
    assert!(
        terminal
            .read_text_viewport(vp(0, 0), vp(11, 2), false)
            .expect("test precondition")
            .trim()
            .is_empty()
    );
}

#[test]
fn clipboard_queries_never_disclose_contents_and_split_writes_complete_once() {
    for suffix in [b"\x07".as_slice(), b"\x1b\\".as_slice()] {
        let bytes = [b"\x1b]52;c;YQBi".as_slice(), suffix].concat();
        for split in 0..=bytes.len() {
            let mut terminal = Terminal::new(10, 3, 0);
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
fn oversized_osc52_clipboard_store_reports_only_its_byte_count() {
    let mut terminal = Terminal::new(10, 3, 0);
    let encoded_payload = "A".repeat((MAX_CLIPBOARD_BYTES / 3 + 1) * 4);
    let decoded_bytes = encoded_payload.len() / 4 * 3;
    let sequence = format!("\x1b]52;c;{encoded_payload}\x07");

    terminal.write(sequence.as_bytes());

    assert!(terminal.take_clipboard_writes().is_empty());
    assert_eq!(
        terminal.take_dropped_clipboard_store_bytes(),
        vec![decoded_bytes]
    );
    assert!(terminal.take_dropped_clipboard_store_bytes().is_empty());
}

#[test]
fn osc52_writes_complete_for_bel_and_st_without_queries() {
    let mut terminal = Terminal::new(10, 5, 0);
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
    let mut terminal = Terminal::new(12, 3, 0);
    let mut render_state = RenderState::new();

    terminal.write(b"primary");
    assert_eq!(terminal.active_screen(), ActiveScreen::Primary);
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(6, 0), false)
            .expect("test precondition"),
        "primary"
    );

    render_state.update(&terminal);
    assert!(render_state.cursor().visible);
    terminal.write(b"\x1b[?25l");
    render_state.update(&terminal);
    assert!(!render_state.cursor().visible);

    terminal.write(b"\x1b[?1049h\x1b[HALT");
    assert_eq!(terminal.active_screen(), ActiveScreen::Alternate);
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(2, 0), false)
            .expect("test precondition"),
        "ALT"
    );

    terminal.write(b"\x1b[?1049l");
    assert_eq!(terminal.active_screen(), ActiveScreen::Primary);
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(6, 0), false)
            .expect("test precondition"),
        "primary"
    );
}

#[test]
fn terminal_and_render_state_smoke_test() {
    let mut terminal = Terminal::new(8, 3, 100);
    assert_eq!(terminal.cols(), 8);
    assert_eq!(terminal.rows(), 3);

    terminal.write(b"hello\r\nworld");

    let mut render_state = RenderState::new();
    render_state.update(&terminal);
    assert_eq!(render_state.cols(), 8);
    assert_eq!(render_state.rows(), 3);
    assert_ne!(render_state.dirty(), Dirty::Clean);

    let mut found_hello = false;
    let mut found_world = false;
    for (row_index, row) in render_state.iter_rows().enumerate() {
        let _ = row.is_dirty();
        let mut line = String::new();
        for cells in row.cells() {
            let text = cells.grapheme_text();
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
    }

    assert!(found_hello);
    assert!(found_world);

    render_state.set_dirty(Dirty::Clean);
    assert_eq!(render_state.dirty(), Dirty::Clean);
}

#[test]
fn render_cells_preserve_issue_453_unicode_payload_exactly() {
    const PAYLOAD: &str = "README \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} \u{1F9D1}\u{200D}\u{1F4BB} \u{2705} \u{26A1} 漢字 café é \u{1F3F3}\u{FE0F}\u{200D}\u{1F308} \u{1F680}";
    let mut terminal = Terminal::new(80, 3, 100);
    terminal.write(format!("{PAYLOAD}\r\n").as_bytes());

    assert_eq!(first_rendered_row_text(&terminal), PAYLOAD);
}

#[test]
fn modify_other_keys_level_is_terminal_state() {
    let mut terminal = Terminal::new(8, 2, 0);
    assert_eq!(
        terminal.modify_other_keys_level(),
        ModifyOtherKeysLevel::Off
    );
    terminal.write(b"\x1b[>4;");
    terminal.write(b"1m");
    assert_eq!(
        terminal.modify_other_keys_level(),
        ModifyOtherKeysLevel::ExceptWellDefined
    );
    terminal.write(b"\x1b[>4;2m");
    assert_eq!(
        terminal.modify_other_keys_level(),
        ModifyOtherKeysLevel::All
    );
    terminal.write(b"\x1b[>4n");
    assert_eq!(
        terminal.modify_other_keys_level(),
        ModifyOtherKeysLevel::Off
    );
    terminal.write(b"\x1b[>4;2m\x1bc");
    assert_eq!(
        terminal.modify_other_keys_level(),
        ModifyOtherKeysLevel::Off
    );
}

/// `CSI > 4 n` is a spelling vte drops; the scanner's replacement goes back
/// through the parser, so a synchronized update defers it with its frame.
#[test]
fn scanner_modify_other_keys_change_waits_for_the_synchronized_frame() {
    let mut terminal = Terminal::new(8, 2, 0);
    terminal.write(b"\x1b[>4;2m");
    terminal.write(b"\x1b[?2026h\x1b[>4n");
    assert_eq!(
        terminal.modify_other_keys_level(),
        ModifyOtherKeysLevel::All
    );
    terminal.write(b"\x1b[?2026l");
    assert_eq!(
        terminal.modify_other_keys_level(),
        ModifyOtherKeysLevel::Off
    );
}

#[test]
fn unlisted_dec_modes_are_absent_from_number_lookup() {
    assert!(modes::lookup_number(47).is_none());
    assert!(modes::lookup_number(65_000).is_none());
}

/// The pinned alacritty evicts from the title stack when the keyboard-mode
/// stack is full: with no title pushed that panics the reader thread, with one
/// pushed the keyboard stack grows without bound. The adapter caps it first.
#[test]
fn kitty_keyboard_push_flood_is_bounded_without_panicking() {
    let max = KEYBOARD_MODE_STACK_MAX_DEPTH;
    let flood = b"\x1b[>1u".repeat(max + 10);
    // No title pushed, one title pushed, and inside a synchronized update
    // (whose buffered bytes reach the core only at ESU).
    let cases: [(&[u8], &[u8]); 3] = [
        (b"", b""),
        (b"\x1b[22t", b""),
        (b"\x1b[?2026h", b"\x1b[?2026l"),
    ];
    for (prefix, suffix) in cases {
        let mut terminal = Terminal::new(20, 3, 0);
        terminal.write(prefix);
        terminal.write(&flood);
        terminal.write(suffix);
        assert_eq!(terminal.keyboard_depth.primary, max, "{prefix:?}");
        assert_eq!(terminal.kitty_keyboard_flags(), 1);

        // At the cap a push replaces the top entry, so the new mode is active
        // and one pop returns to the entry beneath it.
        terminal.write(b"\x1b[>3u");
        assert_eq!(terminal.keyboard_depth.primary, max);
        assert_eq!(terminal.kitty_keyboard_flags(), 3);
        terminal.write(b"\x1b[<u");
        assert_eq!(terminal.kitty_keyboard_flags(), 1);

        // alacritty's real stack is bounded too: popping the mirrored depth
        // empties it.
        terminal.write(format!("\x1b[<{}u", max - 2).as_bytes());
        assert_eq!(terminal.kitty_keyboard_flags(), 1);
        terminal.write(b"\x1b[<u");
        assert_eq!(terminal.keyboard_depth.primary, 0);
        assert_eq!(terminal.kitty_keyboard_flags(), 0);
    }
}

#[test]
fn kitty_keyboard_depth_follows_screen_swaps_and_ris() {
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(&b"\x1b[>1u".repeat(5));
    terminal.write(b"\x1b[?1049h");
    terminal.write(&b"\x1b[>2u".repeat(7));
    assert_eq!(
        (
            terminal.keyboard_depth.primary,
            terminal.keyboard_depth.alternate
        ),
        (5, 7)
    );
    terminal.write(b"\x1b[?1049l\x1b[<2u");
    assert_eq!(
        (
            terminal.keyboard_depth.primary,
            terminal.keyboard_depth.alternate
        ),
        (3, 7)
    );
    assert_eq!(terminal.kitty_keyboard_flags(), 1);
    terminal.write(b"\x1b[<9u");
    assert_eq!(terminal.keyboard_depth.primary, 0);
    terminal.write(b"\x1b[?1049h\x1bc");
    assert_eq!(terminal.keyboard_depth, KeyboardStackDepth::default());
    assert_eq!(terminal.kitty_keyboard_flags(), 0);
}

#[test]
fn halfwidth_voiced_marks_take_their_own_cell() {
    // Whole, split mid-character, inside a synchronized update, and (below)
    // with the mark wrapping onto a new line.
    for chunks in [
        vec!["\u{ff76}\u{ff9e}Z".as_bytes().to_vec()],
        vec![b"\xef\xbd\xb6\xef".to_vec(), b"\xbe\x9eZ".to_vec()],
        vec![b"\xef\xbd\xb6\xef\xbe".to_vec(), b"\x9eZ".to_vec()],
        vec![
            b"\x1b[?2026h\xef\xbd\xb6\xef".to_vec(),
            b"\xbe\x9eZ\x1b[?2026l".to_vec(),
        ],
    ] {
        let mut terminal = Terminal::new(8, 2, 0);
        for chunk in &chunks {
            terminal.write(chunk);
        }
        let rows = terminal.screen_text_rows();
        let graphemes: Vec<_> = rows[0].cells[..3]
            .iter()
            .map(|cell| cell.graphemes.clone())
            .collect();
        assert_eq!(
            graphemes,
            vec![vec![0xff76], vec![0xff9e], vec![u32::from('Z')]],
            "{chunks:?}"
        );
    }

    let mut terminal = Terminal::new(4, 2, 0);
    terminal.write("abcd\u{ff9f}".as_bytes());
    let rows = terminal.screen_text_rows();
    assert!(rows[0].wrap.soft_wrapped);
    assert_eq!(rows[1].cells[0].graphemes, vec![0xff9f]);
}

#[test]
fn screen_text_rows_preserve_wrap_and_grapheme_cells() {
    let mut terminal = Terminal::new(5, 3, 100);
    terminal.write("abcdef\r\n界e\u{301}".as_bytes());

    let rows = terminal.screen_text_rows();

    assert_eq!(rows.len(), 3);
    assert!(rows[0].wrap.soft_wrapped);
    assert!(!rows[0].wrap.wrap_continuation);
    assert!(!rows[1].wrap.soft_wrapped);
    assert!(rows[1].wrap.wrap_continuation);
    assert!(!rows[2].wrap.wrap_continuation);
    assert_eq!(rows[2].cells[0].wide, CellWide::Wide);
    assert_eq!(rows[2].cells[0].graphemes, vec!['界' as u32]);
    assert_eq!(rows[2].cells[1].wide, CellWide::SpacerTail);
    assert_eq!(rows[2].cells[2].graphemes, vec!['e' as u32, 0x301]);
}

#[test]
fn render_state_row_dirty_can_be_cleared_independently() {
    let mut terminal = Terminal::new(8, 3, 100);
    let mut render_state = RenderState::new();

    render_state.update(&terminal);
    for row in render_state.iter_rows() {
        row.clear_dirty();
        assert!(!row.is_dirty());
    }
    assert_eq!(
        render_state
            .dirty_rows()
            .map(|row| row.y())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    render_state.set_dirty(Dirty::Clean);
    assert_eq!(render_state.dirty_rows().count(), 0);

    terminal.write(b"A");
    render_state.update(&terminal);
    assert_eq!(render_state.dirty(), Dirty::Partial);
    let dirty: Vec<_> = render_state.dirty_rows().map(|row| row.y()).collect();
    assert_eq!(dirty, vec![0]);
    let row = render_state.dirty_rows().next().expect("row zero is dirty");
    row.clear_dirty();
    assert!(!row.is_dirty());
    assert_eq!(render_state.dirty(), Dirty::Partial);

    render_state.set_dirty(Dirty::Clean);
    assert_eq!(render_state.dirty(), Dirty::Clean);
}

#[test]
fn scrolling_the_viewport_marks_every_row_dirty() {
    let mut terminal = Terminal::new(8, 3, 100);
    write_numbered_lines(&mut terminal, 10);
    let mut render_state = RenderState::new();
    render_state.update(&terminal);
    render_state.clean();

    terminal.scroll_viewport_delta(-2);
    render_state.update(&terminal);
    assert_eq!(render_state.dirty(), Dirty::Full);
    // The cursor sits on the bottom row, which is now two rows below the viewport.
    assert_eq!(render_state.cursor().viewport, None);
}

#[test]
fn row_cell_basic_data_reports_palette_style() {
    let mut terminal = Terminal::new(8, 3, 100);
    terminal.write(b"\x1b[31mA\x1b[0m");

    let mut render_state = RenderState::new();
    render_state.update(&terminal);

    let row = render_state.iter_rows().next().expect("terminal has rows");
    let cells = row.cells().next().expect("row has cells");
    let basic = cells.basic_data();
    assert_eq!(basic.wide, CellWide::Narrow);
    assert!(basic.has_styling);
    assert_eq!(basic.style.fg_color, Some(CellColor::Palette(1)));
    assert!(!basic.has_hyperlink);
    assert_eq!(cells.fg_color(), Some(default_palette()[1]));
    assert_eq!(cells.bg_color(), None);
}

#[test]
fn clear_screen_keeps_the_cursor_line_and_drops_history() {
    let mut terminal = Terminal::new(10, 4, 100_000);
    write_numbered_lines(&mut terminal, 20);
    terminal.write(b"$ prompt");
    assert!(terminal.scrollback_rows() > 0);

    assert_eq!(terminal.clear_screen(), ClearScreenOutcome::Cleared);

    assert_eq!(terminal.scrollback_rows(), 0);
    assert_eq!(terminal.cursor_y(), 0);
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(9, 3), false)
            .expect("test precondition"),
        "$ prompt"
    );
}

#[test]
fn clear_screen_reports_alternate_screen_refusal() {
    let mut terminal = Terminal::new(10, 4, 100);
    terminal.write(b"primary\x1b[?1049h");
    assert_eq!(
        terminal.clear_screen(),
        ClearScreenOutcome::AlternateScreenActive
    );
    terminal.write(b"\x1b[?1049l");
    assert_eq!(terminal.clear_screen(), ClearScreenOutcome::Cleared);
}

#[test]
fn clear_screen_moves_the_saved_cursor_and_fills_with_default_colours() {
    let mut terminal = Terminal::new(10, 4, 100_000);
    // Save the cursor at the start of the prompt row (row 3), then leave a
    // background colour active.
    terminal.write(b"a\r\nb\r\nc\r\n\x1b7$ \x1b[44m");
    assert_eq!(terminal.clear_screen(), ClearScreenOutcome::Cleared);

    let grid = terminal.term.grid();
    assert_eq!(grid.saved_cursor.point.line, Line(0));
    for line in 1..4 {
        for column in 0..10 {
            let cell = &grid[Line(line)][Column(column)];
            assert_eq!(
                cell.bg,
                Color::Named(NamedColor::Background),
                "row {line} col {column}"
            );
        }
    }
    // The child's pen survives the host action.
    assert_eq!(grid.cursor.template.bg, Color::Named(NamedColor::Blue));

    // DECRC lands back on the prompt row.
    terminal.write(b"\x1b8X");
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(9, 0), false)
            .expect("test precondition"),
        "X"
    );
}

/// Widening a pane lowers the byte budget's line count; history that already
/// fit must survive a widen/narrow cycle (zoom, a wider client attaching).
#[test]
fn widening_resize_keeps_history_that_already_fit() {
    let per_line_narrow = 40 * mem::size_of::<Cell>();
    let mut terminal = Terminal::new(40, 3, per_line_narrow * 2_000);
    write_numbered_lines(&mut terminal, 1_500);
    let before = terminal.scrollback_rows();
    assert!(before > 1_400);

    terminal.resize(shepr_core::geometry::PaneGeometry::new(80, 3, 8, 16));
    terminal.resize(shepr_core::geometry::PaneGeometry::new(40, 3, 8, 16));

    assert_eq!(terminal.scrollback_rows(), before);
    assert_eq!(
        terminal
            .read_text_screen(sr(0, 0), sr(39, 0), false)
            .expect("test precondition"),
        "000000"
    );
}

/// Formats the whole screen as unwrapped VT (as history persistence does),
/// replays it into a fresh terminal, and compares cells and styles.
#[test]
fn vt_history_round_trips_through_the_parser() {
    let mut source = Terminal::new(12, 4, 100_000);
    source.write(
        "plain \x1b[1;38;5;196mbold-red\x1b[0m \x1b[4:3;58;2;1;2;3mcurly\x1b[0m\r\n\
         \x1b]8;id=x_alacritty;https://example.test\x1b\\link\x1b]8;;\x1b\\ 界e\u{301}\r\n\
         \x1b[48;2;9;8;7mwrapped-background-row\x1b[0m\r\n\
         last"
            .as_bytes(),
    );
    let total = u32::try_from(source.total_rows()).unwrap_or(u32::MAX);
    let ansi = source
        .read_ansi_screen(
            sr(0, 0),
            sr(11, usize::try_from(total - 1).unwrap_or(usize::MAX)),
            false,
            true,
        )
        .expect("test precondition");
    assert!(ansi.contains("id=x_alacritty"));

    let mut restored = Terminal::new(12, 4, 100_000);
    restored.write(ansi.as_bytes());

    assert_eq!(
        restored.screen_text_rows(),
        source.screen_text_rows(),
        "{ansi:?}"
    );
    let source_rows = restored.total_rows();
    assert_eq!(source_rows, source.total_rows());
    let styles = |terminal: &Terminal| {
        let grid = terminal.term.grid();
        let history = i32::try_from(terminal.term.history_size()).unwrap_or(i32::MAX);
        let mut styles = Vec::new();
        for y in 0..i32::try_from(terminal.term.total_lines()).unwrap_or(i32::MAX) {
            for x in 0..grid.columns() {
                let cell = &grid[Line(y - history)][Column(x)];
                styles.push((
                    cell_style(cell),
                    cell.hyperlink()
                        .map(|link| (link.id().to_owned(), link.uri().to_owned())),
                ));
            }
        }
        styles
    };
    assert_eq!(styles(&restored), styles(&source), "{ansi:?}");
}

#[test]
fn plain_reads_trim_trailing_blank_lines_and_spaces() {
    let mut terminal = Terminal::new(10, 4, 0);
    terminal.write(b"a  \r\n\r\nb   ");
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(9, 3), false)
            .expect("test precondition"),
        "a\n\nb"
    );
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 3), vp(9, 3), false)
            .expect("test precondition"),
        ""
    );
}

#[test]
fn titles_follow_the_parser_title_stack_and_ris() {
    let mut terminal = Terminal::new(20, 3, 100);
    assert_eq!(terminal.take_title_update(), None);

    // An OSC ends at any ESC, exactly as the parser sees it: the CSI after it
    // is not part of the title and the later OSC is a title of its own.
    terminal.write(b"\x1b]0;foo\x1b[m text \x1b]2;bar\x07");
    assert_eq!(
        terminal.take_title_update(),
        Some(TitleUpdate::Set("bar".to_owned()))
    );

    terminal.write(b"\x1b[22t\x1b]2;vim\x07");
    assert_eq!(
        terminal.take_title_update(),
        Some(TitleUpdate::Set("vim".to_owned()))
    );
    terminal.write(b"\x1b[23t");
    assert_eq!(
        terminal.take_title_update(),
        Some(TitleUpdate::Set("bar".to_owned()))
    );

    // Resizing re-announces the title inside alacritty; that is no change.
    terminal.resize(shepr_core::geometry::PaneGeometry::new(30, 5, 0, 0));
    terminal.resize(shepr_core::geometry::PaneGeometry::new(10, 2, 0, 0));
    assert_eq!(terminal.take_title_update(), None);

    terminal.write(b"\x1bc");
    assert_eq!(terminal.take_title_update(), Some(TitleUpdate::Reset));
}

#[test]
fn only_conemu_progress_is_reported_as_progress() {
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(b"\x1b]9;4;3;\x07");
    assert_eq!(
        terminal.take_progress_update(),
        Some(ProgressReport(b"4;3;".to_vec()))
    );
    terminal.write(b"\x1b]9;build finished\x07");
    assert_eq!(terminal.take_progress_update(), None);
}

/// Inside a frame vte replays DECRQM after BSU and before ESU, so it must see
/// the update as active; outside one it is reset.
#[test]
fn decrqm_2026_reports_an_active_synchronized_update() {
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(b"\x1b[?2026$p");
    terminal.write(b"\x1b[?2026h\x1b[?2026$p\x1bc\x1b[?2026$p\x1b[?2026l\x1b[?2026$p");
    assert_eq!(
        core_replies(&mut terminal),
        vec![
            b"\x1b[?2026;2$y".to_vec(),
            b"\x1b[?2026;1$y".to_vec(),
            b"\x1b[?2026;1$y".to_vec(),
            b"\x1b[?2026;2$y".to_vec(),
        ]
    );
    assert!(
        !terminal.mode_get(DecMode::SynchronizedOutput),
        "the parser agrees the update ended"
    );
}

#[test]
fn resetting_any_tracking_mode_ends_x10_mouse() {
    for mode in [1000u16, 1002, 1003] {
        let mut terminal = Terminal::new(20, 3, 0);
        terminal.write(b"\x1b[?9h");
        assert!(terminal.mode_get(DecMode::X10Mouse));
        terminal.write(format!("\x1b[?{mode}l").as_bytes());
        assert!(!terminal.mode_get(DecMode::X10Mouse), "mode {mode}");
        assert!(!terminal.mouse_tracking_enabled(), "mode {mode}");
    }
}

#[test]
fn host_default_colors_sit_under_child_overrides() {
    let host_fg = RgbColor {
        r: 0xaa,
        g: 0xbb,
        b: 0xcc,
    };
    let host_bg = RgbColor {
        r: 0x11,
        g: 0x22,
        b: 0x33,
    };
    let child_bg = RgbColor {
        r: 0x44,
        g: 0x55,
        b: 0x66,
    };
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.set_default_colors(Some(host_fg), Some(host_bg));
    let mut render_state = RenderState::new();
    render_state.update(&terminal);
    let colors = render_state.colors();
    assert_eq!((colors.foreground, colors.background), (host_fg, host_bg));
    assert_eq!(
        terminal.default_color_override(DefaultColor::Background),
        None
    );

    terminal.write(b"\x1b]11;rgb:44/55/66\x07\x1b]11;?\x07");
    assert!(terminal.take_default_color_set());
    assert_eq!(
        terminal.default_color_override(DefaultColor::Background),
        Some(child_bg)
    );
    // A host theme change leaves the child's override alone.
    terminal.set_default_colors(Some(host_fg), Some(RgbColor::default()));
    terminal.write(b"\x1b]111\x07\x1b]11;?\x07");
    assert!(!terminal.take_default_color_set());
    let queries: Vec<_> = terminal
        .take_pty_responses()
        .into_iter()
        .map(|response| match response {
            PtyResponse::ColorQuery(query) => (query.core_color(), query.child_override()),
            other => panic!("expected colour query, got {other:?}"),
        })
        .collect();
    // OSC 111 falls back to the host default, not to the built-in one.
    assert_eq!(
        queries,
        vec![(Some(child_bg), true), (Some(RgbColor::default()), false)]
    );

    terminal.write(b"\x1b]10;rgb:01/02/03\x07");
    terminal.reset_default_color_overrides();
    assert_eq!(
        terminal.default_color_override(DefaultColor::Foreground),
        None
    );
    render_state.update(&terminal);
    assert_eq!(render_state.colors().foreground, host_fg);
}

#[test]
fn cursor_shape_override_follows_decscusr_osc50_and_ris() {
    let mut terminal = Terminal::new(20, 3, 0);
    assert!(!terminal.cursor_shape_overridden());
    terminal.write(b"\x1b[5 q");
    assert!(terminal.cursor_shape_overridden());
    terminal.write(b"\x1b[0 q");
    assert!(!terminal.cursor_shape_overridden());
    terminal.write(b"\x1b]50;CursorShape=1\x07");
    assert!(terminal.cursor_shape_overridden());
    terminal.write(b"\x1bc");
    assert!(!terminal.cursor_shape_overridden());
}

/// Host-side mode changes must not be fed through the parser: a sequence the
/// child has half-written would be cut short.
#[test]
fn mode_set_does_not_disturb_a_partial_child_sequence() {
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.write(b"\x1b[3");
    terminal
        .mode_set(DecMode::BracketedPaste, true)
        .expect("test precondition");
    terminal.write(b"1mred");
    assert!(terminal.mode_get(DecMode::BracketedPaste));
    assert_eq!(
        terminal
            .read_text_viewport(vp(0, 0), vp(19, 0), false)
            .expect("test precondition"),
        "red"
    );
    assert!(
        terminal
            .mode_set(DecMode::SynchronizedOutput, true)
            .is_err()
    );
}

/// Writes lines `"{i:06}"` for `i` in `lines`, `per_write` lines per write.
fn write_line_range(terminal: &mut Terminal, lines: std::ops::Range<usize>, per_write: usize) {
    let lines: Vec<String> = lines.map(|i| format!("{i:06}\r\n")).collect();
    for chunk in lines.chunks(per_write.max(1)) {
        terminal.write(chunk.concat().as_bytes());
    }
}

/// The text of the line an absolute row id names, `None` once it is gone.
fn absolute_row_text(terminal: &Terminal, row: AbsRow) -> Option<String> {
    let y = terminal.screen_row_for_absolute(row)?;
    let last = terminal.cols().saturating_sub(1);
    terminal
        .read_text_screen(sr(0, y.0), sr(last, y.0), false)
        .ok()
}

/// Line `i` of `write_line_range` output was written on absolute row `i`.
fn assert_rows_name_their_lines(terminal: &Terminal, rows: impl IntoIterator<Item = AbsRow>) {
    for row in rows {
        assert_eq!(
            absolute_row_text(terminal, row),
            Some(format!("{:06}", row.0)),
            "absolute row {} (origin {})",
            row.0,
            terminal.history_origin().0
        );
    }
}

#[test]
fn absolute_rows_keep_naming_their_lines_while_full_history_evicts() {
    // One byte of budget buys the minimum history.
    let mut terminal = Terminal::new(10, 3, 1);
    let limit = u64::try_from(MIN_SCROLLBACK_LINES).expect("test precondition");
    write_line_range(&mut terminal, 0..900, 1);
    assert_eq!(
        terminal.history_origin(),
        AbsRow(0),
        "history is not full yet"
    );
    assert_rows_name_their_lines(&terminal, [AbsRow(0), AbsRow(450), AbsRow(899)]);

    write_line_range(&mut terminal, 900..1_500, 1);
    write_line_range(&mut terminal, 1_500..2_500, 37);
    // 2500 lines and the cursor's empty row were written; three screen rows
    // and a full history are retained.
    let origin = terminal.history_origin();
    assert_eq!(origin, AbsRow(2_501 - (limit + 3)));
    assert_eq!(absolute_row_text(&terminal, origin.saturating_sub(1)), None);
    assert_rows_name_their_lines(
        &terminal,
        [origin, origin.saturating_add(500), AbsRow(2_499)],
    );
    assert_eq!(terminal.absolute_row_for_screen(ScreenRow(0)), origin);

    // A single write longer than the whole history evicts the tracker's
    // reference row as well: every earlier id is retired rather than guessed.
    write_line_range(&mut terminal, 2_500..6_000, 3_500);
    assert!(terminal.history_origin() > AbsRow(2_499));
    assert_eq!(absolute_row_text(&terminal, AbsRow(2_499)), None);
}

/// A blank followed row and a flood of blank lines is the case an address
/// match cannot tell apart: every recycled row looks the same. One write
/// longer than the whole ring must still count every eviction.
#[test]
fn one_write_of_blank_lines_longer_than_the_ring_counts_every_eviction() {
    let mut terminal = Terminal::new(10, 3, 1);
    let limit = MIN_SCROLLBACK_LINES;
    let retained = u64::try_from(limit + 3).expect("test precondition");
    write_line_range(&mut terminal, 0..limit + 10, 1);
    // The newest history row, which the next batch follows, is blank.
    terminal.write(b"\r\n\r\n\r\n\r\n");
    let flood = 3 * (limit + 3) + 7;
    terminal.write("\r\n".repeat(flood).as_bytes());

    // Every "\r\n" moved the cursor one absolute row down.
    let written = u64::try_from(limit + 10 + 4 + flood).expect("test precondition");
    assert_eq!(
        terminal.history_origin(),
        AbsRow(written + 1 - retained),
        "the origin must count each evicted line exactly"
    );
    assert_eq!(
        terminal.absolute_row_for_screen(ScreenRow(terminal.total_rows() - 1)),
        AbsRow(written),
        "the cursor row keeps the id it was written on"
    );
}

#[test]
fn purges_retire_the_ids_of_purged_lines() {
    let mut terminal = Terminal::new(10, 3, 100_000);
    write_line_range(&mut terminal, 0..50, 1);
    assert_rows_name_their_lines(&terminal, [AbsRow(0), AbsRow(49)]);

    // ED 3 drops the history; the screen's lines keep their ids.
    terminal.write(b"\x1b[3J");
    assert_eq!(terminal.history_origin(), AbsRow(48));
    assert_eq!(absolute_row_text(&terminal, AbsRow(47)), None);
    assert_rows_name_their_lines(&terminal, [AbsRow(48), AbsRow(49)]);

    // So does the `CSI ? 3 J` spelling the scanner feeds through.
    write_line_range(&mut terminal, 50..60, 1);
    terminal.write(b"\x1b[?3J");
    assert_eq!(terminal.history_origin(), AbsRow(58));
    assert_rows_name_their_lines(&terminal, [AbsRow(58), AbsRow(59)]);

    // The host's clear keeps the cursor line, moved to the top.
    terminal.write(b"$ prompt");
    assert_eq!(terminal.clear_screen(), ClearScreenOutcome::Cleared);
    assert_eq!(terminal.history_origin(), AbsRow(60));
    assert_eq!(
        absolute_row_text(&terminal, AbsRow(60)).as_deref(),
        Some("$ prompt")
    );
    assert_eq!(absolute_row_text(&terminal, AbsRow(59)), None);

    // RIS resets every line.
    terminal.write(b"\x1bc");
    assert!(terminal.history_origin() > AbsRow(60));
    assert_eq!(absolute_row_text(&terminal, AbsRow(60)), None);
}

#[test]
fn the_alternate_screen_leaves_primary_row_ids_alone() {
    let mut terminal = Terminal::new(10, 3, 1);
    write_line_range(&mut terminal, 0..1_200, 1);
    let origin = terminal.history_origin();
    assert!(origin > AbsRow(0));

    terminal.write(b"\x1b[?1049h");
    for _ in 0..50 {
        terminal.write(b"full-screen\r\n");
    }
    assert_eq!(terminal.history_origin(), origin);
    terminal.write(b"\x1b[?1049l");
    assert_eq!(terminal.history_origin(), origin);
    assert_rows_name_their_lines(&terminal, [origin, AbsRow(1_199)]);

    write_line_range(&mut terminal, 1_200..1_300, 5);
    assert_rows_name_their_lines(&terminal, [terminal.history_origin(), AbsRow(1_299)]);

    // RIS from the alternate screen discards the primary screen too.
    terminal.write(b"\x1b[?1049h\x1bc");
    assert_eq!(absolute_row_text(&terminal, AbsRow(1_299)), None);
}

#[test]
fn height_resizes_keep_row_ids_and_column_resizes_retire_them() {
    let mut terminal = Terminal::new(10, 5, 1);
    write_line_range(&mut terminal, 0..1_500, 1);

    // Height changes move lines between screen and history, evicting at the
    // history limit.
    terminal.resize(shepr_core::geometry::PaneGeometry::new(10, 3, 0, 0));
    assert_rows_name_their_lines(&terminal, [terminal.history_origin(), AbsRow(1_499)]);
    terminal.resize(shepr_core::geometry::PaneGeometry::new(10, 8, 0, 0));
    assert_rows_name_their_lines(&terminal, [terminal.history_origin(), AbsRow(1_499)]);

    // A column change re-wraps every line.
    let retained_end = terminal.absolute_row_for_screen(ScreenRow(terminal.total_rows()));
    terminal.resize(shepr_core::geometry::PaneGeometry::new(12, 8, 0, 0));
    assert!(terminal.history_origin() >= retained_end);
    assert_eq!(absolute_row_text(&terminal, AbsRow(1_499)), None);
}

#[test]
fn visited_rows_match_the_owned_text_rows() {
    let mut terminal = Terminal::new(6, 3, 100_000);
    terminal.write("ab界e\u{301}\u{10eeee}x\r\nwrapped-row-text\r\n".as_bytes());
    let owned = terminal.screen_text_rows();
    let mut scratch = String::new();
    for (y, row) in owned.iter().enumerate() {
        let mut cells = Vec::new();
        let wrap = terminal
            .visit_screen_row_text(ScreenRow(y), &mut scratch, |x, wide, text| {
                cells.push((x, wide, text.to_owned()));
            })
            .expect("row is retained");
        assert_eq!(
            (wrap.soft_wrapped, wrap.wrap_continuation),
            (row.wrap.soft_wrapped, row.wrap.wrap_continuation),
            "row {y}"
        );
        let expected: Vec<_> = row
            .cells
            .iter()
            .enumerate()
            .map(|(x, cell)| {
                // The owned rows already blank the kitty placeholder; see
                // `kitty_unicode_placeholder_is_blank_in_reads_and_rendering`.
                let text = if cell.graphemes.is_empty() {
                    " ".to_owned()
                } else {
                    cell.graphemes
                        .iter()
                        .filter_map(|&codepoint| char::from_u32(codepoint))
                        .collect()
                };
                (
                    u16::try_from(x).expect("test precondition"),
                    cell.wide,
                    text,
                )
            })
            .collect();
        assert_eq!(cells, expected, "row {y}");
    }
    assert!(
        terminal
            .visit_screen_row_text(ScreenRow(owned.len()), &mut scratch, |_, _, _| {})
            .is_none()
    );
}

#[test]
fn kitty_unicode_placeholder_is_blank_in_reads_and_rendering() {
    let mut terminal = Terminal::new(8, 2, 0);
    terminal.write("A\u{10eeee}B".as_bytes());

    assert_eq!(
        terminal
            .screen_cell(1, ScreenRow(0))
            .expect("test precondition")
            .1,
        Vec::<u32>::new()
    );
    let rows = terminal.screen_text_rows();
    assert!(rows[0].cells[1].graphemes.is_empty());
    assert_eq!(
        terminal
            .read_text_screen(sr(0, 0), sr(2, 0), true)
            .expect("test precondition"),
        "A B"
    );

    let mut scratch = String::new();
    let mut visited = Vec::new();
    terminal
        .visit_screen_row_text(ScreenRow(0), &mut scratch, |x, _, text| {
            if x < 3 {
                visited.push(text.to_owned());
            }
        })
        .expect("screen row is retained");
    assert_eq!(visited, ["A", " ", "B"]);
    assert_eq!(first_rendered_row_text(&terminal), "A B");
}

#[test]
fn ris_drops_the_childs_colour_overrides() {
    let host_fg = RgbColor {
        r: 0xaa,
        g: 0xbb,
        b: 0xcc,
    };
    let host_bg = RgbColor {
        r: 0x11,
        g: 0x22,
        b: 0x33,
    };
    let mut terminal = Terminal::new(20, 3, 0);
    terminal.set_default_colors(Some(host_fg), Some(host_bg));
    terminal.write(
        b"\x1b]10;rgb:01/02/03\x07\x1b]11;rgb:04/05/06\x07\
          \x1b]12;rgb:0a/0b/0c\x07\x1b]4;1;rgb:07/08/09\x07",
    );
    assert!(
        terminal
            .default_color_override(DefaultColor::Foreground)
            .is_some()
    );
    assert!(terminal.effective_cursor_color().is_some());

    terminal.write(b"\x1bc");

    assert_eq!(
        terminal.default_color_override(DefaultColor::Foreground),
        None
    );
    assert_eq!(
        terminal.default_color_override(DefaultColor::Background),
        None
    );
    assert_eq!(terminal.effective_cursor_color(), None);
    let mut render_state = RenderState::new();
    render_state.update(&terminal);
    let colors = render_state.colors();
    // The host's colours show again underneath.
    assert_eq!((colors.foreground, colors.background), (host_fg, host_bg));
    assert_eq!(colors.palette[1], default_palette()[1]);
}

/// A logical line read a few rows at a time, each piece from the state the
/// one before it stopped in, is byte for byte what one read gives, wherever
/// the cuts fall: inside styled runs, inside a hyperlink, and with the line
/// ending in blank cells.
#[test]
fn a_wrapped_line_read_in_pieces_joins_to_one_read() {
    let mut terminal = Terminal::new(10, 3, 100_000);
    terminal.write(b"head\r\n");
    terminal.write(
        "\x1b[1;31mred bold \x1b[0m plain \x1b[4:3mcurly\x1b[0m \
         \x1b]8;;https://example.test/a\x1b\\linked text across rows\x1b]8;;\x1b\\ \
         \x1b[7minverse\x1b[0m tail  \x1b[32m      "
            .as_bytes(),
    );
    terminal.write(b"\r\nnext line\r\nlast");
    let cols = terminal.cols();
    let last_row = terminal.total_rows() - 1;
    let whole = terminal
        .read_ansi_screen(sr(0, 0), sr(cols - 1, last_row), false, true)
        .expect("test precondition");

    // The wrapped line is the rows from 1 up to the first row that is not
    // soft-wrapped. Cut it after every soft-wrapped row in turn, and after
    // every pair of them.
    let mut line_end = 1;
    while terminal
        .screen_row_wrap(ScreenRow(line_end))
        .is_some_and(|wrap| wrap.soft_wrapped)
    {
        line_end += 1;
    }
    assert!(line_end > 4, "the line must wrap over several rows");
    for step in [1, 2, 3] {
        let mut carry = AnsiCarry::default();
        let mut joined = terminal
            .read_ansi_screen(sr(0, 0), sr(cols - 1, 0), false, true)
            .expect("test precondition");
        joined.push_str("\r\n");
        let mut row = 1;
        while row <= line_end {
            let last = (row + step - 1).min(line_end);
            let open_end = last < line_end;
            joined.push_str(
                &terminal
                    .read_ansi_screen_carrying(sr(0, row), sr(cols - 1, last), &mut carry, open_end)
                    .expect("test precondition")
                    .0,
            );
            assert_eq!(carry.is_fresh(), !open_end, "step {step}, row {row}");
            row = last + 1;
        }
        // The rest of the screen follows the line as it does in the whole read.
        joined.push_str("\r\n");
        joined.push_str(
            &terminal
                .read_ansi_screen(sr(0, line_end + 1), sr(cols - 1, last_row), false, true)
                .expect("test precondition"),
        );
        assert_eq!(joined, whole, "step {step}");
    }
}

/// The carrying read leaves trailing blank lines in its text and reports
/// where the text ends when cut back to its content.
#[test]
fn a_carrying_read_reports_its_content_end_and_keeps_blank_lines() {
    let mut terminal = Terminal::new(4, 6, 1_000);
    terminal.write(b"abc\r\n\r\n\x1b[42m  \x1b[0m\r\n\r\n");
    let cols = terminal.cols();
    let read = |first: usize, last: usize, carry: &mut AnsiCarry, open_end: bool| {
        terminal
            .read_ansi_screen_carrying(sr(0, first), sr(cols - 1, last), carry, open_end)
            .expect("test precondition")
    };

    // Rows: "abc", blank, painted blank, blank, blank, cursor row.
    let (text, end) = read(0, 1, &mut AnsiCarry::default(), false);
    assert_eq!((text.as_str(), end), ("abc\r\n", Some(3)));

    // Blank rows alone have no content.
    let (text, end) = read(3, 4, &mut AnsiCarry::default(), false);
    assert_eq!((text.as_str(), end), ("\r\n", None));

    // A painted blank row is content, and the blank lines after it stay in
    // the text without counting.
    let (text, end) = read(2, 4, &mut AnsiCarry::default(), false);
    let painted = end.expect("painted blanks are content");
    assert!(painted > 0 && text.len() > painted, "{text:?} {end:?}");
    assert!(text[..painted].contains("\x1b[0;42m"));
    assert!(text[painted..].chars().all(|c| c == '\r' || c == '\n'));
}

/// A blank row that continues a soft-wrapped line finishes a line that has
/// content: its read reports `Some(0)`, an open read reports its whole text.
#[test]
fn a_blank_continuation_of_a_wrapped_line_reports_content() {
    let mut terminal = Terminal::new(4, 4, 1_000);
    // The wrapped row keeps its wrap flag when the continuation is erased.
    terminal.write(b"abcde\x1b[2K\r\nnext");
    let cols = terminal.cols();
    assert!(
        terminal
            .screen_row_wrap(ScreenRow(0))
            .is_some_and(|wrap| wrap.soft_wrapped)
    );

    let mut carry = AnsiCarry::default();
    let (text, end) = terminal
        .read_ansi_screen_carrying(sr(0, 0), sr(cols - 1, 0), &mut carry, true)
        .expect("test precondition");
    assert_eq!((text.as_str(), end), ("abcd", Some(4)));
    assert!(!carry.is_fresh());

    let (text, end) = terminal
        .read_ansi_screen_carrying(sr(0, 1), sr(cols - 1, 1), &mut carry, false)
        .expect("test precondition");
    assert_eq!((text.as_str(), end), ("", Some(0)));
    assert!(carry.is_fresh());

    // Without the carry the same blank row has no content.
    let (text, end) = terminal
        .read_ansi_screen_carrying(sr(0, 1), sr(cols - 1, 1), &mut AnsiCarry::default(), false)
        .expect("test precondition");
    assert_eq!((text.as_str(), end), ("", None));
}
