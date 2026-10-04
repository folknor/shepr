//! What becomes of a glyph that a replaced region splits, when chrome a
//! ratatui renderer drew is laid over cells that already hold a frame. A glyph
//! is a cell with a symbol and the columns its width covers: a pane cell by
//! the grid width its terminal reported (a narrow VS16 cell is one column
//! whatever its glyph), a chrome cell by the grapheme rule in
//! `shepr_term::width::text_width`. A half the replacement would leave behind
//! becomes a [`blank`].
//!
//! Two operations lay chrome over a frame, and both use the one repair rule
//! of [`split_glyph_cells`], the blank and the width rule here. The client's
//! [`crate::compose::Canvas::overwrite`] repairs a whole region at once; the
//! server's [`overlay_buffer`] and [`put_run`] (or [`put_run_with`]) repair
//! the region or run they write the same way, and copy the scratch buffer's
//! continuation cells.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_protocol::{CellData, FrameData, GridCellWidth};
use shepr_term::width::text_width;

use crate::ratatui_conversion::CellDataExt as _;

/// Blanks `cell` in place: a space in its own style, no link. The
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
    cell.hyperlink = None;
}

/// The columns a glyph with `symbol` covers, at least one.
fn columns(symbol: &str, grid_width: GridCellWidth) -> usize {
    match grid_width {
        GridCellWidth::Grapheme => text_width(symbol).max(1),
        GridCellWidth::One | GridCellWidth::WideTail => 1,
        GridCellWidth::WideLead => 2,
    }
}

/// Indices, within one row of `len` cells, of the cells that must become blanks because a
/// glyph is split by `covered` (whether the cell at an index below `len` is inside the
/// region being replaced). `blank_covered` picks the side to blank: `false` for the surface
/// being written over (its uncovered remainder would show half a glyph), `true` for the
/// source being written (its covered part would be half a glyph).
///
/// A glyph's columns count whatever those cells hold: pane surfaces mark wide tails with
/// empty symbols, ratatui buffers with space continuations. An empty-symbol cell no glyph
/// reaches is an orphaned tail; on the destination side one that sits right after a
/// covered cell is blanked too, since the glyph it belonged to is being replaced.
///
/// `covered` is a predicate rather than a mask so that a caller whose region is one span
/// (the server's chrome writes, a pane composed into the client canvas) tests a range and
/// allocates nothing; the server writes every pane border cell through here, per pane, per
/// client draw. The returned list allocates only when a glyph is actually split.
pub(crate) fn split_glyph_cells<'a>(
    len: usize,
    symbol: impl Fn(usize) -> &'a str,
    grid_width: impl Fn(usize) -> GridCellWidth,
    covered: impl Fn(usize) -> bool,
    blank_covered: bool,
) -> Vec<usize> {
    let mut out = Vec::new();
    let mut x = 0;
    while x < len {
        let text = symbol(x);
        if text.is_empty() {
            if !blank_covered && !covered(x) && x > 0 && covered(x - 1) {
                out.push(x);
            }
            x += 1;
            continue;
        }
        let end = x.saturating_add(columns(text, grid_width(x))).min(len);
        let covered_count = (x..end).filter(|index| covered(*index)).count();
        if covered_count != 0 && covered_count != end - x {
            out.extend((x..end).filter(|index| covered(*index) == blank_covered));
        }
        x = end;
    }
    out
}

/// Blanks the cells of `row` that a replacement of the `covered` span would
/// leave as half a glyph, by [`split_glyph_cells`]'s rule for the surface
/// written over.
fn blank_underlying_remnants(row: &mut [CellData], covered: std::ops::Range<usize>) {
    let view = &*row;
    let remnants = split_glyph_cells(
        view.len(),
        |x| view[x].symbol.as_str(),
        |x| view[x].grid_width,
        |x| covered.contains(&x),
        false,
    );
    for x in remnants {
        blank(&mut row[x]);
    }
}

/// Replaces the frame cells from `(x, y)` rightwards with `cells`, clipped to
/// the frame, and repairs the glyphs the span splits by the one rule the
/// client compositor uses ([`split_glyph_cells`]): the uncovered part of any
/// glyph the span cuts, whether its tail is an empty symbol or a space
/// continuation, and an empty-symbol cell right after the span that no glyph
/// reaches, becomes a blank in its own style (no link). `cells` are
/// taken as whole glyphs, except that a wide one cut by the frame's right edge
/// is blanked.
pub fn put_run(frame: &mut FrameData, x: u16, y: u16, cells: &[CellData]) {
    put_run_with(frame, x, y, cells.len(), |index, slot| {
        slot.clone_from(&cells[index]);
    });
}

/// [`put_run`] for a run of `count` cells that `write` produces in place:
/// `write(index, slot)` makes the frame cell `slot` hold the run's cell
/// `index`, overwriting every field, and is called for each index the frame
/// does not clip, in order. The repair is [`put_run`]'s. A caller that builds
/// its cells on the fly (the server's pane border strokes) writes them straight
/// into the frame, reusing each cell's symbol buffer, with no run to allocate.
pub fn put_run_with(
    frame: &mut FrameData,
    x: u16,
    y: u16,
    count: usize,
    mut write: impl FnMut(usize, &mut CellData),
) {
    let width = usize::from(frame.width());
    if y >= frame.height() || x >= frame.width() {
        return;
    }
    let count = count.min(width - usize::from(x));
    if count == 0 {
        return;
    }
    let row_start = usize::from(y) * width;
    let left = usize::from(x);
    let row = &mut frame.cells_mut()[row_start..row_start + width];
    blank_underlying_remnants(row, left..left + count);
    for (index, slot) in row[left..left + count].iter_mut().enumerate() {
        write(index, slot);
    }
    let last = &mut row[left + count - 1];
    if left + count == width && columns(&last.symbol, last.grid_width) > 1 {
        blank(last);
    }
}

/// Replaces the frame cells in `covered` with what a ratatui renderer drew
/// into `scratch` there. Both are in frame coordinates, and `covered` is
/// clipped to the scratch area and the frame. Every covered cell is replaced,
/// drawn or not: a scratch cell's value cannot say whether it was drawn (a
/// deliberately drawn default-style space looks exactly like an untouched
/// one), so the caller states the extent it owns. Each replacement takes the
/// scratch cell's symbol, colours and flags, and no link: a link belongs
/// to the text it was on.
///
/// Glyphs are repaired by the rule the client compositor's
/// [`crate::compose::Canvas::overwrite`] uses ([`split_glyph_cells`]), on both
/// sides. A frame glyph the region cuts loses its uncovered part. A scratch
/// glyph the region or the frame's edge cuts is drawn as a [`blank`] where
/// covered, which includes a continuation cell at the region's left edge
/// whose lead lies outside it. A wide scratch glyph's continuation cell is
/// otherwise copied as the scratch holds it.
pub fn overlay_buffer(frame: &mut FrameData, scratch: &Buffer, covered: Rect) {
    let width = usize::from(frame.width());
    let area = covered.intersection(scratch.area);
    let right = area.right().min(frame.width());
    let bottom = area.bottom().min(frame.height());
    if area.left() >= right {
        return;
    }
    // One column past the scratch, so a wide glyph in its last column reads as
    // cut (cells outside the scratch read as spaces).
    let scratch_len = usize::from(scratch.area.right()) + 1;
    let left = usize::from(area.left());
    // The covered span of every row, within both the frame and the scratch.
    let span = left..usize::from(right);
    for y in area.top()..bottom {
        let row_start = usize::from(y) * width;
        let row = &mut frame.cells_mut()[row_start..row_start + width];
        blank_underlying_remnants(row, span.clone());
        let scratch_at = |x: usize| u16::try_from(x).ok().and_then(|x| scratch.cell((x, y)));
        let scratch_remnants = split_glyph_cells(
            scratch_len,
            |x| scratch_at(x).map_or(" ", ratatui::buffer::Cell::symbol),
            |_| GridCellWidth::Grapheme,
            |x| span.contains(&x),
            true,
        );
        for (x, slot) in row
            .iter_mut()
            .enumerate()
            .take(usize::from(right))
            .skip(left)
        {
            let Some(source) = scratch_at(x) else {
                continue;
            };
            slot.assign_ratatui_cell(source);
            if scratch_remnants.contains(&x) {
                blank(slot);
            }
        }
    }
}
