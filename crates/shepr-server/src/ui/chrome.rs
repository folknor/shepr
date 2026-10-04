//! Chrome (borders, titles, scrollbars, restore-error text) drawn onto a wire
//! frame that already holds the pane cells.
//!
//! Pane cells are written straight into the `FrameData`; they never pass
//! through a ratatui `Buffer`. The chrome the server still draws with ratatui
//! widgets goes into a scratch buffer of its own and is laid over the frame
//! with `overlay_buffer`, so the frame is only ever written through
//! `put_run`. Both are `shepr_surface::glyph_repair`'s, whose one repair rule
//! (the client compositor's) blanks the uncovered half of any glyph the
//! written region splits, and an orphaned empty tail right after it. Chrome cells only ever carry what ratatui can express
//! (colours, flags, a single underline), which `CellData::from_ratatui_cell`
//! converts without loss.

pub(super) use shepr_surface::glyph_repair::{overlay_buffer, put_run};

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use shepr_protocol::{CellData, FrameData, GridCellWidth, WireColor, WireStyleFlags};

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.to_owned(),
            ..CellData::blank()
        }
    }

    /// One row of cells, one per char; `~` is an empty-symbol wide tail as
    /// pane rendering writes them.
    fn frame(row: &str) -> FrameData {
        let cells = row
            .chars()
            .map(|c| {
                if c == '~' {
                    cell("")
                } else {
                    cell(&c.to_string())
                }
            })
            .collect::<Vec<_>>();
        FrameData::new(
            cells.clone(),
            u16::try_from(cells.len()).expect("test row fits"),
            1,
            None,
            Vec::new(),
        )
        .expect("test row is a valid frame")
    }

    fn text(frame: &FrameData) -> String {
        frame
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    #[test]
    fn put_run_replaces_cells_and_clips_to_the_frame() {
        let mut frame = frame("abcd");
        frame
            .set_hyperlinks(vec!["https://example.test".into()])
            .expect("no cell links yet");
        frame.cells_mut()[1].hyperlink = Some(0);
        put_run(&mut frame, 1, 0, &[cell("X"), cell("Y")]);
        assert_eq!(text(&frame), "aXYd");
        assert_eq!(frame.cells()[1].hyperlink, None);
        put_run(&mut frame, 3, 0, &[cell("1"), cell("2"), cell("3")]);
        assert_eq!(text(&frame), "aXY1");
        put_run(&mut frame, 4, 0, &[cell("no")]);
        put_run(&mut frame, 0, 1, &[cell("no")]);
        assert_eq!(text(&frame), "aXY1");
    }

    #[test]
    fn put_run_blanks_the_uncovered_half_of_a_split_wide_glyph() {
        // Lead replaced: the tail left behind becomes a blank, keeping its
        // style but not its link.
        let mut lead = frame("a漢~d");
        lead.set_hyperlinks(vec!["https://example.test".into()])
            .expect("no cell links yet");
        lead.cells_mut()[2].bg = WireColor::Green;
        lead.cells_mut()[2].hyperlink = Some(0);
        put_run(&mut lead, 1, 0, &[cell("#")]);
        assert_eq!(text(&lead), "a# d");
        assert_eq!(lead.cells()[2].bg, WireColor::Green);
        assert_eq!(lead.cells()[2].hyperlink, None);

        // Tail replaced: the lead becomes a blank.
        let mut tail = frame("a漢~d");
        put_run(&mut tail, 2, 0, &[cell("#")]);
        assert_eq!(text(&tail), "a #d");

        // A span that covers the whole glyph leaves nothing to repair.
        let mut whole = frame("a漢~d");
        put_run(&mut whole, 1, 0, &[cell("#"), cell("#")]);
        assert_eq!(text(&whole), "a##d");
    }

    #[test]
    fn put_run_blanks_a_split_pane_glyph_by_its_grid_width() {
        // A pane's wide lead whose tail is replaced loses its two-column claim.
        let mut wide = frame("a漢~d");
        wide.cells_mut()[1].grid_width = GridCellWidth::WideLead;
        wide.cells_mut()[2].grid_width = GridCellWidth::WideTail;
        put_run(&mut wide, 2, 0, &[cell("#")]);
        assert_eq!(text(&wide), "a #d");
        assert_eq!(wide.cells()[1].grid_width, GridCellWidth::Grapheme);
    }

    #[test]
    fn put_run_blanks_the_orphaned_tail_at_the_end_of_a_span() {
        let mut frame = frame("ab漢~d");
        put_run(&mut frame, 1, 0, &[cell("#"), cell("#")]);
        assert_eq!(text(&frame), "a## d");
    }

    #[test]
    fn put_run_blanks_an_orphaned_empty_tail_after_a_narrow_cell() {
        // The tail's lead is already gone (a narrow cell stands before it), and
        // the span ends right before it.
        let mut frame = frame("ab~d");
        put_run(&mut frame, 0, 0, &[cell("#"), cell("#")]);
        assert_eq!(text(&frame), "## d");
    }

    #[test]
    fn put_run_blanks_a_wide_lead_whose_tail_is_a_space_continuation() {
        // The form chrome writes: a lead followed by a space, not an empty
        // symbol.
        let mut tail = frame("a漢 d");
        put_run(&mut tail, 2, 0, &[cell("#")]);
        assert_eq!(text(&tail), "a #d");

        let mut lead = frame("a漢 d");
        put_run(&mut lead, 1, 0, &[cell("#")]);
        assert_eq!(text(&lead), "a# d");
        assert_eq!(lead.cells()[2].grid_width, GridCellWidth::Grapheme);
    }

    #[test]
    fn overlay_blanks_a_scratch_continuation_at_the_left_edge() {
        // The wide glyph's lead is outside the covered area; its continuation
        // cell, whatever the scratch holds there, is not copied.
        let mut frame = frame("abcd");
        let mut scratch = Buffer::empty(Rect::new(0, 0, 4, 1));
        scratch.set_string(1, 0, "漢", Style::default());
        scratch.cell_mut((2, 0)).expect("in area").set_symbol("x");
        overlay_buffer(&mut frame, &scratch, Rect::new(2, 0, 1, 1));
        assert_eq!(text(&frame), "ab d");
    }

    #[test]
    fn overlay_replaces_only_the_covered_cells() {
        let mut frame = frame("abcd");
        frame
            .set_hyperlinks(vec!["https://example.test".into()])
            .expect("no cell links yet");
        frame.cells_mut()[1].bg = WireColor::Green;
        frame.cells_mut()[2].hyperlink = Some(0);
        frame.cells_mut()[2].style.underline = shepr_vt::UnderlineStyle::Curly;
        let mut scratch = Buffer::empty(Rect::new(1, 0, 3, 1));
        scratch.set_string(
            2,
            0,
            "X",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        );
        overlay_buffer(&mut frame, &scratch, Rect::new(2, 0, 1, 1));
        assert_eq!(text(&frame), "abXd");
        // Cells of the scratch area outside the covered rect keep what was
        // under them.
        assert_eq!(frame.cells()[1].bg, WireColor::Green);
        let drawn = &frame.cells()[2];
        assert_eq!(drawn.fg, WireColor::Red);
        assert!(drawn.style.flags.contains(WireStyleFlags::BOLD));
        assert_eq!(drawn.style.underline, shepr_vt::UnderlineStyle::Single);
        assert_eq!(drawn.hyperlink, None);
    }

    #[test]
    fn a_drawn_default_style_space_replaces_what_is_under_it() {
        // A title padded with default-style spaces (as with `overlay0 =
        // "reset"`) must clear the border stroke under its padding.
        let mut frame = frame("─────");
        let mut scratch = Buffer::empty(Rect::new(0, 0, 5, 1));
        let (end, _) = scratch.set_stringn(0, 0, " a b ", 5, Style::default());
        overlay_buffer(&mut frame, &scratch, Rect::new(0, 0, end, 1));
        assert_eq!(text(&frame), " a b ");
    }

    #[test]
    fn overlay_drops_ratatuis_skip_hint() {
        let mut frame = frame("ab");
        let mut scratch = Buffer::empty(Rect::new(0, 0, 2, 1));
        if let Some(cell) = scratch.cell_mut((0, 0)) {
            cell.set_symbol("z");
            cell.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
        }
        overlay_buffer(&mut frame, &scratch, Rect::new(0, 0, 1, 1));
        assert_eq!(text(&frame), "zb");
    }

    #[test]
    fn overlay_clips_to_the_frame() {
        let mut frame = frame("abc");
        let mut scratch = Buffer::empty(Rect::new(1, 0, 6, 3));
        scratch.set_string(1, 0, "XYZW", Style::default());
        overlay_buffer(&mut frame, &scratch, Rect::new(1, 0, 4, 1));
        assert_eq!(text(&frame), "aXY");
    }

    #[test]
    fn overlay_gives_a_wide_glyph_its_second_column() {
        let mut frame = frame("abcd");
        let mut scratch = Buffer::empty(Rect::new(0, 0, 4, 1));
        scratch.set_string(1, 0, "漢", Style::default());
        overlay_buffer(&mut frame, &scratch, Rect::new(1, 0, 2, 1));
        assert_eq!(frame.cells()[1].symbol, "漢");
        assert_eq!(text(&frame), "a漢 d");

        // Over a pane's wide glyph: nothing half-drawn is left.
        let mut over = self::frame("a漢~d");
        let mut scratch = Buffer::empty(Rect::new(0, 0, 4, 1));
        scratch.set_string(2, 0, "#", Style::default());
        overlay_buffer(&mut over, &scratch, Rect::new(2, 0, 1, 1));
        assert_eq!(text(&over), "a #d");
    }

    #[test]
    fn overlay_draws_a_wide_glyph_that_would_cross_the_area_as_a_blank() {
        let mut frame = frame("abcd");
        // The glyph starts in the last column of a narrower scratch area.
        let mut scratch = Buffer::empty(Rect::new(0, 0, 2, 1));
        scratch.cell_mut((1, 0)).expect("in area").set_symbol("漢");
        overlay_buffer(&mut frame, &scratch, Rect::new(1, 0, 1, 1));
        assert_eq!(text(&frame), "a cd");
    }
}
