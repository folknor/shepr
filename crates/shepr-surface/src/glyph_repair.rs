//! What becomes of a glyph that a replaced region splits, when chrome a
//! ratatui renderer drew is laid over cells that already hold a frame. A glyph
//! is a cell with a symbol and the columns its width covers: a pane cell by
//! the grid width its terminal reported (a narrow VS16 cell is one column
//! whatever its glyph), a chrome cell by the grapheme rule in
//! `shepr_term::width::text_width`. A half the replacement would leave behind
//! becomes a [`blank`].
//!
//! Two operations lay chrome over a frame, and both use the blank and the
//! width rule here. The client's [`crate::compose::Canvas::overwrite`] repairs
//! a whole region at once and copies the scratch buffer's continuation cells.
//! The server's [`overlay_buffer`] writes one glyph at a time through
//! [`put_run`], which repairs only the edges of each run, and fills a wide
//! glyph's second column with a synthesized [`CellData::blank`].

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_protocol::{CellData, FrameData, GridCellWidth};
use shepr_term::width::text_width;

use crate::ratatui_conversion::CellDataExt as _;

/// Blanks `cell` in place: a space in its own style, no skip, no link. The
/// space is one column wide whatever the cell held, so a blanked wide pane
/// lead no longer claims the column after it. Unlike
/// [`crate::pane_row::blank_pane_cell`] (the blank for a pane glyph cut by a
/// crop or broken by the emulator) it keeps underline and strikethrough, so a
/// pane glyph half that chrome overwrites can look different from one a crop
/// cut. Visual only; both are one column and draw nothing over a neighbour.
pub fn blank(cell: &mut CellData) {
    cell.symbol.clear();
    cell.symbol.push(' ');
    cell.grid_width = GridCellWidth::Grapheme;
    cell.skip = false;
    cell.hyperlink = None;
}

/// The columns a glyph with `symbol` covers, at least one.
fn columns(symbol: &str, grid_width: GridCellWidth) -> usize {
    match grid_width {
        GridCellWidth::Grapheme => text_width(symbol).max(1),
        GridCellWidth::One => 1,
        GridCellWidth::Two => 2,
    }
}

/// Indices, within one row of `len` cells, of the cells that must become blanks because a
/// glyph is split by `covered` (one flag per cell: whether the cell is inside the region
/// being replaced). `blank_covered` picks the side to blank: `false` for the surface being
/// written over (its uncovered remainder would show half a glyph), `true` for the source
/// being written (its covered part would be half a glyph).
///
/// A glyph's columns count whatever those cells hold: pane surfaces mark wide tails with
/// empty symbols, ratatui buffers with space continuations. An empty-symbol cell no glyph
/// reaches is an orphaned tail; on the destination side one that sits right after a
/// covered cell is blanked too, since the glyph it belonged to is being replaced.
pub(crate) fn split_glyph_cells<'a>(
    len: usize,
    symbol: impl Fn(usize) -> &'a str,
    grid_width: impl Fn(usize) -> GridCellWidth,
    covered: &[bool],
    blank_covered: bool,
) -> Vec<usize> {
    let mut out = Vec::new();
    let mut x = 0;
    while x < len {
        let text = symbol(x);
        if text.is_empty() {
            if !blank_covered && !covered[x] && x > 0 && covered[x - 1] {
                out.push(x);
            }
            x += 1;
            continue;
        }
        let end = x.saturating_add(columns(text, grid_width(x))).min(len);
        let covered_count = covered[x..end].iter().filter(|covered| **covered).count();
        if covered_count != 0 && covered_count != end - x {
            out.extend((x..end).filter(|index| covered[*index] == blank_covered));
        }
        x = end;
    }
    out
}

/// Whether `cell` occupies two columns.
fn is_wide(cell: &CellData) -> bool {
    columns(&cell.symbol, cell.grid_width) > 1
}

/// Replaces the frame cells from `(x, y)` rightwards with `cells`, clipped to
/// the frame, and repairs the glyphs the span's edges split: the lead of a wide
/// glyph whose tail is replaced, and the tail of a wide glyph whose lead is
/// replaced, become blanks in their own style (no skip, no link). Wide tails
/// are the empty-symbol cells pane rendering writes. A frame whose cell vector
/// does not match its dimensions is left alone.
pub fn put_run(frame: &mut FrameData, x: u16, y: u16, cells: &[CellData]) {
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
/// into `scratch` there, one glyph at a time through [`put_run`]. Both are in
/// frame coordinates, and `covered` is clipped to the scratch area and the
/// frame. Every covered cell is replaced, drawn or not: a scratch cell's value
/// cannot say whether it was drawn (a deliberately drawn default-style space
/// looks exactly like an untouched one), so the caller states the extent it
/// owns. Each replacement takes the scratch cell's symbol, colours, flags and
/// skip, and no link: a link belongs to the text it was on.
///
/// A wide glyph in the scratch owns the cell after it, which becomes a
/// [`CellData::blank`] whatever the scratch holds there. One whose second
/// column falls outside the covered area or the frame would show half a
/// glyph, so it is drawn as a [`blank`].
pub fn overlay_buffer(frame: &mut FrameData, scratch: &Buffer, covered: Rect) {
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
