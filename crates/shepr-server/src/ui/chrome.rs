//! Chrome (borders, titles, scrollbars, restore-error text) drawn onto a wire
//! frame that already holds the pane cells.
//!
//! Pane cells are written straight into the `FrameData`; they never pass
//! through a ratatui `Buffer`. The chrome the server still draws with ratatui
//! widgets goes into a scratch buffer of its own and is laid over the frame
//! here, so the frame is only ever written through `put_run`, which cannot
//! leave half a glyph of what it replaces. Chrome cells only ever carry what
//! ratatui can express (colours, flags, a single underline), which
//! `CellData::from_ratatui_cell` converts without loss.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_protocol::{CellData, FrameData, GridCellWidth};
use shepr_term::width::text_width;

/// Blanks `cell` in place: a space in its own style, no skip, no link. The
/// space is one column wide whatever the cell held, so a blanked wide pane
/// lead no longer claims the column after it. Unlike
/// `shepr_protocol::blank_pane_cell` (the blank for a pane glyph cut by a crop
/// or broken by the emulator) it keeps underline and strikethrough, so a pane
/// glyph half that chrome overwrites can look different from one a crop cut.
/// Visual only; both are one column and draw nothing over a neighbour.
fn blank(cell: &mut CellData) {
    cell.symbol.clear();
    cell.symbol.push(' ');
    cell.grid_width = GridCellWidth::Grapheme;
    cell.skip = false;
    cell.hyperlink = None;
}

/// Whether `cell` occupies two columns: a pane cell by the grid width its
/// terminal reported (a narrow VS16 cell is one column whatever its glyph),
/// a chrome cell by its glyph.
fn is_wide(cell: &CellData) -> bool {
    match cell.grid_width {
        GridCellWidth::Grapheme => text_width(&cell.symbol) > 1,
        GridCellWidth::One => false,
        GridCellWidth::Two => true,
    }
}

/// Replaces the frame cells from `(x, y)` rightwards with `cells`, clipped to
/// the frame, and repairs the glyphs the span's edges split: the lead of a wide
/// glyph whose tail is replaced, and the tail of a wide glyph whose lead is
/// replaced, become blanks in their own style (no skip, no link). Wide tails
/// are the empty-symbol cells pane rendering writes.
pub(super) fn put_run(frame: &mut FrameData, x: u16, y: u16, cells: &[CellData]) {
    let width = usize::from(frame.width);
    if y >= frame.height
        || x >= frame.width
        || frame.cells.len() != width * usize::from(frame.height)
    {
        return;
    }
    let start = usize::from(y) * width + usize::from(x);
    let count = cells.len().min(width - usize::from(x));
    if count == 0 {
        return;
    }
    let end = start + count;
    let row_start = usize::from(y) * width;
    let row_end = row_start + width;

    if start > row_start && frame.cells[start].symbol.is_empty() && is_wide(&frame.cells[start - 1])
    {
        blank(&mut frame.cells[start - 1]);
    }
    let last_was_wide = is_wide(&frame.cells[end - 1]);
    for (slot, cell) in frame.cells[start..end].iter_mut().zip(cells) {
        slot.clone_from(cell);
    }
    if end < row_end && last_was_wide && frame.cells[end].symbol.is_empty() {
        blank(&mut frame.cells[end]);
    }
}

/// Replaces the frame cells in `covered` with what a ratatui renderer drew
/// into `scratch` there. Both are in frame coordinates, and `covered` is
/// clipped to the scratch area and the frame. Every covered cell is replaced,
/// drawn or not: a scratch cell's value cannot say whether it was drawn (a
/// deliberately drawn default-style space looks exactly like an untouched
/// one), so the caller states the extent it owns. Each replacement takes the
/// scratch cell's symbol, colours, flags and skip, and no link: a link belongs
/// to the text it was on.
///
/// A wide glyph in the scratch owns the cell after it, which becomes a blank.
/// One whose second column falls outside the covered area or the frame would
/// show half a glyph, so it is drawn as a blank.
pub(super) fn overlay_buffer(frame: &mut FrameData, scratch: &Buffer, covered: Rect) {
    let area = covered.intersection(scratch.area);
    let right = area.right().min(frame.width);
    let bottom = area.bottom().min(frame.height);
    for y in area.top()..bottom {
        let mut x = area.left();
        while x < right {
            let Some(source) = scratch.cell((x, y)) else {
                break;
            };
            let mut cell = CellData::from_ratatui_cell(source);
            let glyph_width = text_width(&cell.symbol).max(1);
            let columns = u16::try_from(glyph_width).unwrap_or(u16::MAX);
            let mut run = Vec::with_capacity(glyph_width);
            if x.saturating_add(columns) > right {
                blank(&mut cell);
                run.push(cell);
            } else {
                run.push(cell);
                run.resize(glyph_width, CellData::blank());
            }
            put_run(frame, x, y, &run);
            x = x.saturating_add(columns.max(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier, Style};
    use shepr_protocol::{WireColor, WireStyleFlags};

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
        FrameData {
            width: u16::try_from(cells.len()).expect("test row fits"),
            height: 1,
            cells,
            cursor: None,
            hyperlinks: Vec::new(),
        }
    }

    fn text(frame: &FrameData) -> String {
        frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    #[test]
    fn put_run_replaces_cells_and_clips_to_the_frame() {
        let mut frame = frame("abcd");
        frame.cells[1].hyperlink = Some(0);
        frame.cells[1].skip = true;
        put_run(&mut frame, 1, 0, &[cell("X"), cell("Y")]);
        assert_eq!(text(&frame), "aXYd");
        assert_eq!(frame.cells[1].hyperlink, None);
        assert!(!frame.cells[1].skip);
        put_run(&mut frame, 3, 0, &[cell("1"), cell("2"), cell("3")]);
        assert_eq!(text(&frame), "aXY1");
        put_run(&mut frame, 4, 0, &[cell("no")]);
        put_run(&mut frame, 0, 1, &[cell("no")]);
        assert_eq!(text(&frame), "aXY1");
    }

    #[test]
    fn put_run_blanks_the_uncovered_half_of_a_split_wide_glyph() {
        // Lead replaced: the tail left behind becomes a blank, keeping its
        // style but not its skip or link.
        let mut lead = frame("a漢~d");
        lead.cells[2].bg = WireColor::Green;
        lead.cells[2].hyperlink = Some(0);
        lead.cells[2].skip = true;
        put_run(&mut lead, 1, 0, &[cell("#")]);
        assert_eq!(text(&lead), "a# d");
        assert_eq!(lead.cells[2].bg, WireColor::Green);
        assert!(!lead.cells[2].skip);
        assert_eq!(lead.cells[2].hyperlink, None);

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
        wide.cells[1].grid_width = GridCellWidth::Two;
        wide.cells[2].grid_width = GridCellWidth::One;
        put_run(&mut wide, 2, 0, &[cell("#")]);
        assert_eq!(text(&wide), "a #d");
        assert_eq!(wide.cells[1].grid_width, GridCellWidth::Grapheme);
    }

    #[test]
    fn put_run_blanks_the_orphaned_tail_at_the_end_of_a_span() {
        let mut frame = frame("ab漢~d");
        put_run(&mut frame, 1, 0, &[cell("#"), cell("#")]);
        assert_eq!(text(&frame), "a## d");
    }

    #[test]
    fn overlay_replaces_only_the_covered_cells() {
        let mut frame = frame("abcd");
        frame.cells[1].bg = WireColor::Green;
        frame.cells[2].hyperlink = Some(0);
        frame.cells[2].style.underline = shepr_vt::UnderlineStyle::Curly;
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
        assert_eq!(frame.cells[1].bg, WireColor::Green);
        let drawn = &frame.cells[2];
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
    fn overlay_skip_comes_from_the_scratch_cell() {
        let mut frame = frame("ab");
        let mut scratch = Buffer::empty(Rect::new(0, 0, 2, 1));
        if let Some(cell) = scratch.cell_mut((0, 0)) {
            cell.set_symbol("z");
            cell.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
        }
        overlay_buffer(&mut frame, &scratch, Rect::new(0, 0, 1, 1));
        assert!(frame.cells[0].skip);
        assert!(!frame.cells[1].skip);
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
        assert_eq!(frame.cells[1].symbol, "漢");
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
