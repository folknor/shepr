//! The two primitives client composition uses on wire cells: restyling cells in place
//! (`patch_style`) and replacing a region with what a ratatui renderer drew into a scratch
//! buffer (`overwrite`). `FrameData` is the composition target throughout; pane cells never
//! pass through ratatui, so underline shapes, hyperlinks and wide-glyph tails stay in their
//! wire form. Pane cells carry their terminal grid width; chrome cells keep the
//! grapheme rule in `shepr_termio::blit::text_width`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use shepr_protocol::FrameData;

use shepr_protocol::{CellData, GridCellWidth, WireColor, WireStyleFlags};

/// Wire flag for each single-bit ratatui modifier that has one. Underline is not here: it
/// is a typed shape on the wire and is handled apart.
const FLAG_MODIFIERS: [(Modifier, WireStyleFlags); 8] = [
    (Modifier::BOLD, WireStyleFlags::BOLD),
    (Modifier::DIM, WireStyleFlags::DIM),
    (Modifier::ITALIC, WireStyleFlags::ITALIC),
    (Modifier::SLOW_BLINK, WireStyleFlags::SLOW_BLINK),
    (Modifier::RAPID_BLINK, WireStyleFlags::RAPID_BLINK),
    (Modifier::REVERSED, WireStyleFlags::REVERSED),
    (Modifier::HIDDEN, WireStyleFlags::HIDDEN),
    (Modifier::CROSSED_OUT, WireStyleFlags::CROSSED_OUT),
];

/// A ratatui `Style`, applied to a wire cell with exactly the `Cell::set_style` algorithm:
/// colours if set, then insert `add_modifier`, then remove `sub_modifier`. `Style::reset()`
/// needs no special step: its resulting fields (both colours `Reset`, `sub_modifier` all)
/// go through the same algorithm, so `Style::reset().add_modifier(UNDERLINED)` ends
/// underlined. The underline is typed: adding it keeps an existing shape or sets `Single`,
/// removing it clears the shape. Symbol, skip and hyperlink are never touched. Underline
/// colour has no wire form and is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) struct StylePatch {
    fg: Option<WireColor>,
    bg: Option<WireColor>,
    add: Modifier,
    sub: Modifier,
}

impl StylePatch {
    pub(in crate::shell) fn from_style(style: Style) -> Self {
        Self {
            fg: style.fg.map(WireColor::from_ratatui),
            bg: style.bg.map(WireColor::from_ratatui),
            add: style.add_modifier,
            sub: style.sub_modifier,
        }
    }

    fn apply(self, cell: &mut CellData) {
        if let Some(fg) = self.fg {
            cell.fg = fg;
        }
        if let Some(bg) = self.bg {
            cell.bg = bg;
        }
        for (modifier, flag) in FLAG_MODIFIERS {
            // Insert first, then remove, as `Cell::set_style` does.
            if self.add.contains(modifier) && !cell.style.flags.contains(flag) {
                cell.style.flags.toggle(flag);
            }
            if self.sub.contains(modifier) && cell.style.flags.contains(flag) {
                cell.style.flags.toggle(flag);
            }
        }
        if self.add.contains(Modifier::UNDERLINED)
            && cell.style.underline == shepr_vt::UnderlineStyle::None
        {
            cell.style.underline = shepr_vt::UnderlineStyle::Single;
        }
        if self.sub.contains(Modifier::UNDERLINED) {
            cell.style.underline = shepr_vt::UnderlineStyle::None;
        }
    }
}

/// Restyles `cells` in place. Symbol, skip and hyperlink are preserved.
pub(in crate::shell) fn patch_style(cells: &mut [CellData], patch: StylePatch) {
    for cell in cells {
        patch.apply(cell);
    }
}

/// `patch_style` on the frame cell at `(x, y)`; a position outside the frame is ignored.
pub(in crate::shell) fn patch_cell(frame: &mut FrameData, x: u16, y: u16, patch: StylePatch) {
    if x >= frame.width || y >= frame.height {
        return;
    }
    let index = usize::from(y) * usize::from(frame.width) + usize::from(x);
    if let Some(cell) = frame.cells.get_mut(index) {
        patch.apply(cell);
    }
}

/// `patch_style` on every frame cell inside `rect`, clipped to the frame.
pub(in crate::shell) fn patch_rect(frame: &mut FrameData, rect: Rect, patch: StylePatch) {
    let width = usize::from(frame.width);
    let rect = rect.intersection(Rect::new(0, 0, frame.width, frame.height));
    if rect.is_empty() || frame.cells.len() != width * usize::from(frame.height) {
        return;
    }
    for y in rect.top()..rect.bottom() {
        let start = usize::from(y) * width + usize::from(rect.x);
        let end = start + usize::from(rect.width);
        patch_style(&mut frame.cells[start..end], patch);
    }
}

/// Blanks `cell` in place: a space in its own style, no skip, no hyperlink.
/// It keeps underline and strikethrough, unlike `shepr_protocol::blank_pane_cell`,
/// which `compose_pane_surface` uses for pane cells its crop cuts; a pane
/// glyph half that chrome overwrites here can therefore look different from a
/// cropped one. Visual only.
pub(in crate::shell) fn blank(cell: &mut CellData) {
    cell.symbol.clear();
    cell.symbol.push(' ');
    cell.grid_width = GridCellWidth::Grapheme;
    cell.skip = false;
    cell.hyperlink = None;
}

/// Indices, within one row of `len` cells, of the cells that must become blanks because a
/// glyph is split by `covered` (one flag per cell: whether the cell is inside the region
/// being replaced). `blank_covered` picks the side to blank: `false` for the surface being
/// written over (its uncovered remainder would show half a glyph), `true` for the source
/// being written (its covered part would be half a glyph).
///
/// A glyph is a cell with a symbol and the cells its width covers, whatever those hold:
/// pane surfaces mark wide tails with empty symbols, ratatui buffers with space
/// continuations. An empty-symbol cell no glyph reaches is an orphaned tail; on the
/// destination side one that sits right after a covered cell is blanked too, since the
/// glyph it belonged to is being replaced. `grid_width` preserves pane grid widths and
/// selects grapheme sizing for chrome cells.
pub(in crate::shell) fn split_glyph_cells<'a>(
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
        let width = match grid_width(x) {
            GridCellWidth::Grapheme => shepr_termio::blit::text_width(text).max(1),
            GridCellWidth::One => 1,
            GridCellWidth::Two => 2,
        };
        let end = x.saturating_add(width).min(len);
        let covered_count = covered[x..end].iter().filter(|covered| **covered).count();
        if covered_count != 0 && covered_count != end - x {
            out.extend((x..end).filter(|index| covered[*index] == blank_covered));
        }
        x = end;
    }
    out
}

fn scratch_at(scratch: &Buffer, x: usize, y: u16) -> Option<&ratatui::buffer::Cell> {
    scratch.cell((u16::try_from(x).ok()?, y))
}

/// Replaces the union of `rects` in `frame` with what a renderer drew into `scratch`.
///
/// Coordinates are absolute (a scratch buffer is full-screen at origin 0) and are clipped to
/// the frame first; the union is one replacement. Symbol, colours and modifiers come from
/// the scratch cell (its underline becomes `Single`), skip comes from the replacement, and
/// hyperlinks are cleared unconditionally: a link belongs to the text it was on, and the
/// replacement is different text. Glyph repair is computed from the original destination
/// and the source before anything is written: an underlying glyph split by the union's
/// boundary has its uncovered part blanked (a space in its own style, no skip, no link),
/// and a scratch glyph that would cross the boundary becomes a blank.
pub(in crate::shell) fn overwrite(frame: &mut FrameData, rects: &[Rect], scratch: &Buffer) {
    let width = usize::from(frame.width);
    if width == 0 || frame.cells.len() != width * usize::from(frame.height) {
        return;
    }
    let bounds = Rect::new(0, 0, frame.width, frame.height).intersection(scratch.area);
    let rects = rects
        .iter()
        .map(|rect| rect.intersection(bounds))
        .filter(|rect| !rect.is_empty())
        .collect::<Vec<_>>();
    let (Some(top), Some(bottom)) = (
        rects.iter().map(|rect| rect.top()).min(),
        rects.iter().map(|rect| rect.bottom()).max(),
    ) else {
        return;
    };
    let mut covered = vec![false; width];
    for y in top..bottom {
        covered.fill(false);
        for rect in rects
            .iter()
            .filter(|rect| rect.top() <= y && y < rect.bottom())
        {
            covered[usize::from(rect.left())..usize::from(rect.right())].fill(true);
        }
        if !covered.contains(&true) {
            continue;
        }
        let row_start = usize::from(y) * width;
        let row = &mut frame.cells[row_start..row_start + width];
        let underlying = &*row;
        let underlying_remnants = split_glyph_cells(
            width,
            move |x| underlying[x].symbol.as_str(),
            move |x| underlying[x].grid_width,
            &covered,
            false,
        );
        let scratch_remnants = split_glyph_cells(
            width,
            move |x| scratch_at(scratch, x, y).map_or(" ", ratatui::buffer::Cell::symbol),
            |_| GridCellWidth::Grapheme,
            &covered,
            true,
        );
        for x in underlying_remnants {
            blank(&mut row[x]);
        }
        for x in (0..width).filter(|x| covered[*x]) {
            let Some(source) = scratch_at(scratch, x, y) else {
                continue;
            };
            let mut cell = CellData::from_ratatui_cell(source);
            cell.style.underline = if source.modifier.contains(Modifier::UNDERLINED) {
                shepr_vt::UnderlineStyle::Single
            } else {
                shepr_vt::UnderlineStyle::None
            };
            if scratch_remnants.contains(&x) {
                blank(&mut cell);
            }
            row[x] = cell;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CellData, GridCellWidth, WireColor, WireStyleFlags};
    use crate::shell::presentation::wire_cells::{
        StylePatch, overwrite, patch_cell, patch_rect, patch_style,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use shepr_protocol::FrameData;

    use ratatui::style::Color;
    use shepr_vt::UnderlineStyle;

    const SHAPES: [UnderlineStyle; 5] = [
        UnderlineStyle::Single,
        UnderlineStyle::Double,
        UnderlineStyle::Curly,
        UnderlineStyle::Dotted,
        UnderlineStyle::Dashed,
    ];

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.to_owned(),
            grid_width: GridCellWidth::Grapheme,
            fg: WireColor::Reset,
            bg: WireColor::Reset,
            style: shepr_protocol::WireStyle::default(),
            skip: false,
            hyperlink: None,
        }
    }

    /// One row of cells, one per char; `~` is an empty-symbol wide tail as pane surfaces
    /// write them.
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

    fn blank_scratch(width: u16, height: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, width, height))
    }

    #[test]
    fn patch_applies_the_set_style_algorithm_and_keeps_symbol_skip_and_link() {
        let mut cells = vec![cell("a")];
        cells[0].hyperlink = Some(3);
        cells[0].skip = true;
        cells[0].fg = WireColor::Red;
        patch_style(
            &mut cells,
            StylePatch::from_style(
                Style::default()
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC)
                    .remove_modifier(Modifier::ITALIC),
            ),
        );
        // Colours only where set. `remove_modifier` takes `ITALIC` out of the add mask, so
        // only BOLD is inserted.
        assert_eq!(cells[0].fg, WireColor::Red);
        assert_eq!(cells[0].bg, WireColor::Blue);
        assert!(cells[0].style.flags.contains(WireStyleFlags::BOLD));
        assert!(!cells[0].style.flags.contains(WireStyleFlags::ITALIC));
        assert_eq!(cells[0].symbol, "a");
        assert!(cells[0].skip);
        assert_eq!(cells[0].hyperlink, Some(3));
    }

    #[test]
    fn patch_matches_ratatui_cell_set_style_for_reset_and_overlapping_masks() {
        let styles = [
            Style::reset(),
            Style::reset().fg(Color::Green),
            Style::reset().add_modifier(Modifier::BOLD),
            Style::default().add_modifier(Modifier::DIM),
            Style::default().remove_modifier(Modifier::BOLD),
            Style::default().fg(Color::Red).bg(Color::Rgb(1, 2, 3)),
        ];
        for style in styles {
            let mut wire = cell("x");
            wire.fg = WireColor::Yellow;
            wire.style.flags = WireStyleFlags::BOLD.union(WireStyleFlags::REVERSED);
            let mut reference = ratatui::buffer::Cell::new("x");
            reference.fg = Color::Yellow;
            reference.modifier = Modifier::BOLD | Modifier::REVERSED;
            reference.set_style(style);
            patch_style(
                std::slice::from_mut(&mut wire),
                StylePatch::from_style(style),
            );
            assert_eq!(wire.fg, WireColor::from_ratatui(reference.fg), "{style:?}");
            assert_eq!(wire.bg, WireColor::from_ratatui(reference.bg), "{style:?}");
            assert_eq!(
                wire.style.flags,
                shepr_protocol::WireStyle::from_ratatui_modifier(reference.modifier).flags,
                "{style:?}"
            );
        }
    }

    #[test]
    fn reset_with_underline_added_ends_underlined_and_reset_alone_clears_it() {
        let mut underlined = cell("u");
        underlined.style.underline = UnderlineStyle::Curly;
        patch_style(
            std::slice::from_mut(&mut underlined),
            StylePatch::from_style(Style::reset().add_modifier(Modifier::UNDERLINED)),
        );
        // Derived through the resulting masks: `UNDERLINED` is in `add` and not in `sub`.
        assert_eq!(underlined.style.underline, UnderlineStyle::Curly);
        let mut plain = cell("u");
        patch_style(
            std::slice::from_mut(&mut plain),
            StylePatch::from_style(Style::reset().add_modifier(Modifier::UNDERLINED)),
        );
        assert_eq!(plain.style.underline, UnderlineStyle::Single);
        patch_style(
            std::slice::from_mut(&mut underlined),
            StylePatch::from_style(Style::reset()),
        );
        assert_eq!(underlined.style.underline, UnderlineStyle::None);
    }

    #[test]
    fn every_underline_shape_survives_unrelated_patches_and_is_typed() {
        for shape in SHAPES {
            let mut shaped = cell("u");
            shaped.style.underline = shape;
            shaped.hyperlink = Some(0);
            // Colour and other modifiers leave the shape and the link alone.
            patch_style(
                std::slice::from_mut(&mut shaped),
                StylePatch::from_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::DIM),
                ),
            );
            assert_eq!(shaped.style.underline, shape, "{shape:?}");
            assert_eq!(shaped.hyperlink, Some(0));
            // Adding underline keeps the shape.
            patch_style(
                std::slice::from_mut(&mut shaped),
                StylePatch::from_style(Style::default().add_modifier(Modifier::UNDERLINED)),
            );
            assert_eq!(shaped.style.underline, shape, "{shape:?}");
            // Removing it clears the shape.
            patch_style(
                std::slice::from_mut(&mut shaped),
                StylePatch::from_style(Style::default().remove_modifier(Modifier::UNDERLINED)),
            );
            assert_eq!(shaped.style.underline, UnderlineStyle::None, "{shape:?}");
        }
        let mut plain = cell("u");
        patch_style(
            std::slice::from_mut(&mut plain),
            StylePatch::from_style(Style::default().add_modifier(Modifier::UNDERLINED)),
        );
        assert_eq!(plain.style.underline, UnderlineStyle::Single);
    }

    #[test]
    fn patch_cell_and_rect_ignore_positions_outside_the_frame() {
        let mut frame = frame("abcd");
        let patch = StylePatch::from_style(Style::default().bg(Color::Red));
        patch_cell(&mut frame, 4, 0, patch);
        patch_cell(&mut frame, 0, 1, patch);
        patch_rect(&mut frame, Rect::new(2, 0, 10, 5), patch);
        let backgrounds = frame.cells.iter().map(|cell| cell.bg).collect::<Vec<_>>();
        assert_eq!(
            backgrounds,
            [
                WireColor::Reset,
                WireColor::Reset,
                WireColor::Red,
                WireColor::Red
            ]
        );
    }

    #[test]
    fn overwrite_copies_cells_with_typed_underline_and_skip_from_the_replacement() {
        let mut frame = frame("abcd");
        frame.cells[1].style.underline = UnderlineStyle::Dashed;
        frame.cells[1].skip = true;
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(
            1,
            0,
            "X",
            Style::default()
                .fg(Color::Red)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        );
        overwrite(&mut frame, &[Rect::new(1, 0, 2, 1)], &scratch);
        assert_eq!(text(&frame), "aX d");
        let replaced = &frame.cells[1];
        assert_eq!(replaced.fg, WireColor::Red);
        assert_eq!(replaced.bg, WireColor::Blue);
        assert!(replaced.style.flags.contains(WireStyleFlags::BOLD));
        // A scratch `UNDERLINED` is Single; the destination's Dashed shape is gone.
        assert_eq!(replaced.style.underline, UnderlineStyle::Single);
        assert!(!replaced.skip, "skip comes from the replacement");
        // Outside the union nothing changes.
        assert_eq!(frame.cells[0].symbol, "a");
        assert_eq!(frame.cells[3].symbol, "d");
    }

    #[test]
    fn overwrite_skip_comes_from_the_replacement() {
        let mut frame = frame("ab");
        let mut scratch = blank_scratch(2, 1);
        if let Some(cell) = scratch.cell_mut((0, 0)) {
            cell.set_symbol("z");
            cell.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
        }
        overwrite(&mut frame, &[Rect::new(0, 0, 2, 1)], &scratch);
        assert!(frame.cells[0].skip);
        assert!(!frame.cells[1].skip);
    }

    #[test]
    fn overwrite_clears_hyperlinks_unconditionally_even_for_the_same_symbol() {
        let mut frame = frame("abc");
        frame.hyperlinks = vec!["https://a.example".into()];
        for cell in &mut frame.cells {
            cell.hyperlink = Some(0);
        }
        let mut scratch = blank_scratch(3, 1);
        // The replacement paints the very symbol already there.
        scratch.set_string(1, 0, "b", Style::default());
        overwrite(&mut frame, &[Rect::new(1, 0, 1, 1)], &scratch);
        assert_eq!(frame.cells[0].hyperlink, Some(0));
        assert_eq!(frame.cells[1].hyperlink, None);
        assert_eq!(frame.cells[2].hyperlink, Some(0));
    }

    #[test]
    fn overwrite_clips_to_the_frame_and_ignores_rects_entirely_outside() {
        let mut frame = frame("abc");
        let mut scratch = blank_scratch(3, 1);
        scratch.set_string(0, 0, "XYZ", Style::default());
        overwrite(
            &mut frame,
            &[Rect::new(2, 0, 9, 4), Rect::new(7, 7, 2, 2)],
            &scratch,
        );
        assert_eq!(text(&frame), "abZ");
    }

    #[test]
    fn overwrite_treats_adjacent_rects_as_one_replacement() {
        // A wide glyph in the scratch spanning two adjacent rects is not split.
        let mut frame = frame("abcd");
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(1, 0, "漢", Style::default());
        overwrite(
            &mut frame,
            &[Rect::new(1, 0, 1, 1), Rect::new(2, 0, 1, 1)],
            &scratch,
        );
        assert_eq!(frame.cells[1].symbol, "漢");
        assert_eq!(text(&frame), "a漢 d");
    }

    #[test]
    fn overwrite_blanks_the_uncovered_part_of_a_split_underlying_glyph() {
        // Lead covered, tail (empty symbol) not.
        let mut left = frame("a漢~d");
        left.cells[1].bg = WireColor::Green;
        left.cells[2].bg = WireColor::Green;
        left.cells[2].hyperlink = Some(0);
        left.hyperlinks = vec!["https://a.example".into()];
        left.cells[2].skip = true;
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(1, 0, "#", Style::default());
        overwrite(&mut left, &[Rect::new(1, 0, 1, 1)], &scratch);
        assert_eq!(text(&left), "a# d");
        // The blanked tail keeps its own style but not skip or the link.
        assert_eq!(left.cells[2].bg, WireColor::Green);
        assert!(!left.cells[2].skip);
        assert_eq!(left.cells[2].hyperlink, None);

        // Tail covered, lead not: the lead is blanked, also with space continuations.
        let mut right = frame("a漢 d");
        overwrite(&mut right, &[Rect::new(2, 0, 1, 1)], &{
            let mut scratch = blank_scratch(4, 1);
            scratch.set_string(2, 0, "#", Style::default());
            scratch
        });
        assert_eq!(text(&right), "a #d");
    }

    #[test]
    fn overwrite_blanks_an_already_orphaned_tail_beside_the_union() {
        // The lead of the empty-symbol tail at 2 was clipped away earlier; replacing the
        // cell before it must not leave the tail behind.
        let mut frame = frame("ab~d");
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(1, 0, "#", Style::default());
        overwrite(&mut frame, &[Rect::new(1, 0, 1, 1)], &scratch);
        assert_eq!(text(&frame), "a# d");
    }

    #[test]
    fn overlay_edge_after_narrow_vs16_cell_keeps_the_pane_glyph() {
        let mut frame = frame("  ");
        frame.cells[0].symbol = "\u{26a0}\u{fe0f}".to_owned();
        frame.cells[0].grid_width = GridCellWidth::One;
        frame.cells[1].grid_width = GridCellWidth::One;
        let mut scratch = blank_scratch(2, 1);
        scratch.set_string(1, 0, "x", Style::default());

        overwrite(&mut frame, &[Rect::new(1, 0, 1, 1)], &scratch);

        assert_eq!(frame.cells[0].symbol, "\u{26a0}\u{fe0f}");
        assert_eq!(frame.cells[0].grid_width, GridCellWidth::One);
        assert_eq!(frame.cells[1].symbol, "x");
    }

    #[test]
    fn overwrite_blanks_a_scratch_glyph_that_would_cross_the_boundary() {
        let mut frame = frame("abcd");
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(1, 0, "漢", Style::default().fg(Color::Red));
        // The union ends inside the glyph: its covered half becomes a blank.
        overwrite(&mut frame, &[Rect::new(0, 0, 2, 1)], &scratch);
        assert_eq!(text(&frame), "  cd");
        assert_eq!(frame.cells[1].fg, WireColor::Red);
    }

    #[test]
    fn overwrite_uses_the_output_width_rule_for_halfwidth_katakana() {
        let voiced = "\u{ff76}\u{ff9e}";
        assert_eq!(shepr_termio::blit::text_width(voiced), 2);
        let mut frame = frame("a");
        frame.cells[0].symbol = voiced.to_owned();
        frame.cells.push(cell(""));
        frame.width = 2;
        let mut scratch = blank_scratch(2, 1);
        scratch.set_string(0, 0, "#", Style::default());
        overwrite(&mut frame, &[Rect::new(0, 0, 1, 1)], &scratch);
        assert_eq!(text(&frame), "# ");
    }

    #[test]
    fn overwrite_repairs_a_split_emoji_variation_glyph() {
        let mut frame = FrameData {
            cells: vec![cell("\u{2764}\u{fe0f}"), cell(""), cell("z")],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let mut scratch = blank_scratch(3, 1);
        scratch.set_string(0, 0, "#", Style::default());

        assert_eq!(shepr_termio::blit::text_width("\u{2764}\u{fe0f}"), 2);
        overwrite(&mut frame, &[Rect::new(0, 0, 1, 1)], &scratch);

        assert_eq!(text(&frame), "# z");
    }

    #[test]
    fn hint_row_style_patch_plus_prefix_overwrite_blanks_a_wide_glyph_at_the_boundary() {
        // The too-small hint restyles its whole row, then overwrites the prefix its text
        // actually wrote; a wide glyph the prefix boundary splits is blanked.
        let mut frame = frame("ab漢~cd");
        frame.cells[3].hyperlink = Some(0);
        frame.cells[4].hyperlink = Some(0);
        frame.hyperlinks = vec!["https://a.example".into()];
        let hint = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        patch_rect(
            &mut frame,
            Rect::new(0, 0, 6, 1),
            StylePatch::from_style(hint),
        );
        let mut scratch = blank_scratch(6, 1);
        let (written_to, _) = scratch.set_stringn(0, 0, "hi!", 6, hint);
        assert_eq!(written_to, 3);
        overwrite(&mut frame, &[Rect::new(0, 0, written_to, 1)], &scratch);
        assert_eq!(text(&frame), "hi! cd");
        // The row keeps the hint style beyond the prefix, links included, and the blanked
        // tail carries it too.
        assert!(frame.cells.iter().all(|cell| cell.bg == WireColor::Cyan));
        assert_eq!(frame.cells[4].hyperlink, Some(0));
        assert_eq!(frame.cells[3].hyperlink, None);
    }

    #[test]
    fn overwrite_ignores_an_inconsistent_frame() {
        let mut frame = frame("abc");
        frame.width = 2;
        let mut scratch = blank_scratch(3, 1);
        scratch.set_string(0, 0, "XYZ", Style::default());
        overwrite(&mut frame, &[Rect::new(0, 0, 3, 1)], &scratch);
        assert_eq!(text(&frame), "abc");
    }
}
