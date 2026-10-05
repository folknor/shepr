//! The client's frame composition. A [`Canvas`] is the composition target: a
//! frame whose cell vector matches its width and height and whose hyperlink
//! indices name entries of its link table, kept so by every operation, since
//! none of its fields can be written from outside. It takes three kinds of
//! change: restyling cells in place (`patch_*`), replacing a region with what a
//! ratatui renderer drew into a scratch buffer (`overwrite`), and pasting a
//! server pane surface into a region (`compose_pane`). Pane cells never pass
//! through ratatui, so underline shapes, hyperlinks and wide-glyph tails stay
//! in their wire form. Pane cells carry their terminal grid width; chrome
//! cells keep the grapheme rule in `shepr_term::width::text_width`.
//!
//! Styles and colours arrive resolved, as ratatui `Style` values: nothing here
//! knows a theme.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use shepr_protocol::{
    CellData, CursorState, FrameData, FrameGridError, GridCellWidth, WireColor, WireStyleFlags,
};

use crate::glyph_repair::{blank, split_glyph_cells};
use crate::ratatui_conversion::{CellDataExt as _, FrameDataExt as _, WireColorExt as _};

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
/// removing it clears the shape. Symbol and hyperlink are never touched. Underline
/// colour has no wire form and is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StylePatch {
    fg: Option<WireColor>,
    bg: Option<WireColor>,
    add: Modifier,
    sub: Modifier,
}

impl StylePatch {
    pub fn from_style(style: Style) -> Self {
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
            && cell.style.underline == shepr_term::UnderlineStyle::None
        {
            cell.style.underline = shepr_term::UnderlineStyle::Single;
        }
        if self.sub.contains(Modifier::UNDERLINED) {
            cell.style.underline = shepr_term::UnderlineStyle::None;
        }
    }
}

/// Restyles `cells` in place. Symbol and hyperlink are preserved.
fn patch_style(cells: &mut [CellData], patch: StylePatch) {
    for cell in cells {
        patch.apply(cell);
    }
}

fn scratch_at(scratch: &Buffer, x: usize, y: u16) -> Option<&ratatui::buffer::Cell> {
    scratch.cell((u16::try_from(x).ok()?, y))
}

/// The composition target. Its frame is a [`FrameData`], so its cell vector
/// always holds exactly `width * height` cells within the surface budgets and
/// its link table is within budget; every operation also keeps every cell's
/// hyperlink index naming an entry of that table. A host terminal larger than
/// the surface budgets has no canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    frame: FrameData,
}

impl Canvas {
    /// A canvas over `frame`, refused when a cell names a hyperlink its table
    /// lacks (cell edits through `FrameData::cells_mut` are not rechecked
    /// until here).
    pub fn new(frame: FrameData) -> Result<Self, FrameGridError> {
        frame.validate()?;
        Ok(Self { frame })
    }

    /// What a ratatui renderer drew into `buffer`, with no cursor and no
    /// links, refused when the buffer is not a grid the surface budgets admit.
    pub fn from_buffer(buffer: &Buffer) -> Result<Self, FrameGridError> {
        Self::new(FrameData::from_ratatui_buffer_with_hyperlinks(
            buffer,
            None,
            &[],
        )?)
    }

    pub fn width(&self) -> u16 {
        self.frame.width()
    }

    pub fn height(&self) -> u16 {
        self.frame.height()
    }

    /// The cells in row-major order.
    pub fn cells(&self) -> &[CellData] {
        self.frame.cells()
    }

    pub fn hyperlinks(&self) -> &[String] {
        self.frame.hyperlinks()
    }

    pub fn cursor(&self) -> Option<&CursorState> {
        self.frame.cursor()
    }

    pub fn set_cursor(&mut self, cursor: Option<CursorState>) {
        self.frame.set_cursor(cursor);
    }

    /// The finished frame.
    pub fn into_frame(self) -> FrameData {
        self.frame
    }

    /// Restyles every cell. Symbol and hyperlink are preserved.
    pub fn patch_all(&mut self, patch: StylePatch) {
        patch_style(self.frame.cells_mut(), patch);
    }

    /// Restyles the cell at `(x, y)`; a position outside the canvas is ignored.
    pub fn patch_cell(&mut self, x: u16, y: u16, patch: StylePatch) {
        let (width, height) = (self.frame.width(), self.frame.height());
        if x >= width || y >= height {
            return;
        }
        let index = usize::from(y) * usize::from(width) + usize::from(x);
        if let Some(cell) = self.frame.cells_mut().get_mut(index) {
            patch.apply(cell);
        }
    }

    /// Restyles every cell inside `rect`, clipped to the canvas.
    pub fn patch_rect(&mut self, rect: Rect, patch: StylePatch) {
        let width = usize::from(self.frame.width());
        let rect = rect.intersection(Rect::new(0, 0, self.frame.width(), self.frame.height()));
        if rect.is_empty() {
            return;
        }
        let cells = self.frame.cells_mut();
        for y in rect.top()..rect.bottom() {
            let start = usize::from(y) * width + usize::from(rect.x);
            let end = start + usize::from(rect.width);
            patch_style(&mut cells[start..end], patch);
        }
    }

    /// Replaces the union of `rects` with what a renderer drew into `scratch`.
    ///
    /// Coordinates are absolute (a scratch buffer is full-screen at origin 0) and are
    /// clipped to the canvas first; the union is one replacement. Symbol, colours and
    /// modifiers come from the scratch cell (its underline becomes `Single`), and
    /// hyperlinks are cleared unconditionally: a link belongs
    /// to the text it was on, and the replacement is different text. Glyph repair is
    /// computed from the original destination and the source before anything is
    /// written: an underlying glyph split by the union's boundary has its uncovered part
    /// blanked (a space in its own style, no link), and a scratch glyph that
    /// would cross the boundary becomes a blank.
    pub fn overwrite(&mut self, rects: &[Rect], scratch: &Buffer) {
        let width = usize::from(self.frame.width());
        if width == 0 {
            return;
        }
        let bounds =
            Rect::new(0, 0, self.frame.width(), self.frame.height()).intersection(scratch.area);
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
        let cells = self.frame.cells_mut();
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
            let row = &mut cells[row_start..row_start + width];
            let underlying = &*row;
            let underlying_remnants = split_glyph_cells(
                width,
                move |x| underlying[x].symbol.as_str(),
                move |x| underlying[x].grid_width,
                |x| covered[x],
                false,
            );
            let scratch_remnants = split_glyph_cells(
                width,
                move |x| scratch_at(scratch, x, y).map_or(" ", ratatui::buffer::Cell::symbol),
                |_| GridCellWidth::Grapheme,
                |x| covered[x],
                true,
            );
            for x in underlying_remnants {
                blank(&mut row[x]);
            }
            for x in (0..width).filter(|x| covered[*x]) {
                let Some(source) = scratch_at(scratch, x, y) else {
                    continue;
                };
                let cell = &mut row[x];
                cell.assign_ratatui_cell(source);
                cell.style.underline = if source.modifier.contains(Modifier::UNDERLINED) {
                    shepr_term::UnderlineStyle::Single
                } else {
                    shepr_term::UnderlineStyle::None
                };
                if scratch_remnants.contains(&x) {
                    blank(cell);
                }
            }
        }
    }

    /// Copies `source` cells into the canvas at `area`, clipped to both, keeping each
    /// cell's underline shape and remapping hyperlinks into the canvas's table (a link
    /// `source`'s own table lacks, or one the canvas's table has no room for, is
    /// dropped). It shares only the glyph repair with
    /// [`Self::overwrite`]: a canvas glyph split by the pasted region loses its uncovered
    /// part, and a source glyph cut by the clip becomes a blank. The cursor becomes
    /// `source`'s, moved to `area`, when it falls in the copied part, and none otherwise.
    pub fn compose_pane(&mut self, source: &FrameData, area: Rect) {
        let target = &mut self.frame;
        let (target_cols, target_rows) = (target.width(), target.height());
        let target_width = usize::from(target_cols);
        let source_width = usize::from(source.width());
        let copy_width = source
            .width()
            .min(area.width)
            .min(target_cols.saturating_sub(area.x));
        let copy_height = source
            .height()
            .min(area.height)
            .min(target_rows.saturating_sub(area.y));
        // Every source link is appended, used or not; one the table has no
        // room for maps to no link.
        let hyperlink_map: Vec<Option<u32>> = source
            .hyperlinks()
            .iter()
            .map(|uri| target.push_hyperlink(uri.clone()))
            .collect();

        if copy_width > 0 {
            let target_covered = usize::from(area.x)..usize::from(area.x + copy_width);
            let source_covered = 0..usize::from(copy_width);
            let target_cells = target.cells_mut();
            for row in 0..copy_height {
                let source_row = &source.cells()[usize::from(row) * source_width..][..source_width];
                let target_start = usize::from(area.y + row) * target_width;
                let target_row = &mut target_cells[target_start..target_start + target_width];
                let target_view = &*target_row;
                let target_remnants = split_glyph_cells(
                    target_width,
                    move |x| target_view[x].symbol.as_str(),
                    move |x| target_view[x].grid_width,
                    |x| target_covered.contains(&x),
                    false,
                );
                for x in target_remnants {
                    blank(&mut target_row[x]);
                }
                let source_remnants = if source_width > usize::from(copy_width) {
                    split_glyph_cells(
                        source_width,
                        move |x| source_row[x].symbol.as_str(),
                        move |x| source_row[x].grid_width,
                        |x| source_covered.contains(&x),
                        true,
                    )
                } else {
                    Vec::new()
                };
                for (col, source_cell) in source_row[..usize::from(copy_width)].iter().enumerate() {
                    let mut cell = source_cell.clone();
                    cell.hyperlink = source_cell.hyperlink.and_then(|index| {
                        usize::try_from(index)
                            .ok()
                            .and_then(|index| hyperlink_map.get(index).copied().flatten())
                    });
                    if source_remnants.contains(&col) {
                        // A cut pane glyph becomes the blank the server emits for
                        // the same cut; chrome keeps its own repair.
                        match cell.grid_width {
                            GridCellWidth::One
                            | GridCellWidth::WideLead
                            | GridCellWidth::WideTail => {
                                crate::pane_row::blank_pane_cell(&mut cell);
                            }
                            GridCellWidth::Grapheme => blank(&mut cell),
                        }
                    }
                    target_row[usize::from(area.x) + col] = cell;
                }
            }
        }

        target.set_cursor(source.cursor().and_then(|cursor| {
            (cursor.x < copy_width && cursor.y < copy_height).then(|| CursorState {
                x: area.x + cursor.x,
                y: area.y + cursor.y,
                visible: cursor.visible,
                shape: cursor.shape,
            })
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::{Canvas, StylePatch, patch_style};
    use crate::ratatui_conversion::{WireColorExt as _, WireStyleExt as _};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use shepr_protocol::{
        CellData, CursorShapeParam, CursorState, FrameData, FrameGridError, GridCellWidth,
        WireColor, WireStyle, WireStyleFlags,
    };
    use shepr_term::UnderlineStyle;

    const SHAPES: [UnderlineStyle; 5] = [
        UnderlineStyle::Single,
        UnderlineStyle::Double,
        UnderlineStyle::Curly,
        UnderlineStyle::Dotted,
        UnderlineStyle::Dashed,
    ];

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.into(),
            grid_width: GridCellWidth::Grapheme,
            fg: WireColor::Reset,
            bg: WireColor::Reset,
            style: WireStyle::default(),
            hyperlink: None,
        }
    }

    /// One row of cells, one per char; `~` is an empty-symbol cell, the trailing half of a
    /// wide glyph as chrome writes it.
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

    fn canvas(frame: FrameData) -> Canvas {
        Canvas::new(frame).expect("test frame is a valid canvas")
    }

    fn text(canvas: &Canvas) -> String {
        canvas
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    fn blank_scratch(width: u16, height: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, width, height))
    }

    #[test]
    fn a_canvas_refuses_a_frame_whose_links_do_not_hold() {
        let mut dangling = frame("ab");
        dangling.cells_mut()[1].hyperlink = Some(0);
        assert_eq!(Canvas::new(dangling), Err(FrameGridError::InvalidHyperlink));
        assert_eq!(
            Canvas::from_buffer(&blank_scratch(0, 0)),
            Err(FrameGridError::InvalidDimensions)
        );
    }

    #[test]
    fn a_canvas_refuses_a_buffer_whose_content_does_not_match_its_area() {
        let mut cleared = blank_scratch(1, 1);
        cleared.content.clear();
        assert_eq!(
            Canvas::from_buffer(&cleared),
            Err(FrameGridError::InvalidCellCount)
        );
        let mut extra = blank_scratch(2, 1);
        extra.content.push(ratatui::buffer::Cell::new("x"));
        assert_eq!(
            Canvas::from_buffer(&extra),
            Err(FrameGridError::InvalidCellCount)
        );
    }

    #[test]
    fn patch_applies_the_set_style_algorithm_and_keeps_symbol_and_link() {
        let mut cells = vec![cell("a")];
        cells[0].hyperlink = Some(3);
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
                WireStyle::from_ratatui_modifier(reference.modifier).flags,
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
        let mut canvas = canvas(frame("abcd"));
        let patch = StylePatch::from_style(Style::default().bg(Color::Red));
        canvas.patch_cell(4, 0, patch);
        canvas.patch_cell(0, 1, patch);
        canvas.patch_rect(Rect::new(2, 0, 10, 5), patch);
        let backgrounds = canvas
            .cells()
            .iter()
            .map(|cell| cell.bg)
            .collect::<Vec<_>>();
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
    fn overwrite_copies_cells_with_typed_underline_from_the_replacement() {
        let mut source = frame("abcd");
        source.cells_mut()[1].style.underline = UnderlineStyle::Dashed;
        let mut canvas = canvas(source);
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
        canvas.overwrite(&[Rect::new(1, 0, 2, 1)], &scratch);
        assert_eq!(text(&canvas), "aX d");
        let replaced = &canvas.cells()[1];
        assert_eq!(replaced.fg, WireColor::Red);
        assert_eq!(replaced.bg, WireColor::Blue);
        assert!(replaced.style.flags.contains(WireStyleFlags::BOLD));
        // A scratch `UNDERLINED` is Single; the destination's Dashed shape is gone.
        assert_eq!(replaced.style.underline, UnderlineStyle::Single);
        // Outside the union nothing changes.
        assert_eq!(canvas.cells()[0].symbol, "a");
        assert_eq!(canvas.cells()[3].symbol, "d");
    }

    #[test]
    fn overwrite_drops_ratatuis_skip_hint() {
        let mut canvas = canvas(frame("ab"));
        let mut scratch = blank_scratch(2, 1);
        if let Some(cell) = scratch.cell_mut((0, 0)) {
            cell.set_symbol("z");
            cell.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
        }
        canvas.overwrite(&[Rect::new(0, 0, 2, 1)], &scratch);
        assert_eq!(text(&canvas), "z ");
    }

    #[test]
    fn overwrite_clears_hyperlinks_unconditionally_even_for_the_same_symbol() {
        let mut source = frame("abc");
        source
            .set_hyperlinks(vec!["https://a.example".into()])
            .expect("no cell links yet");
        for cell in source.cells_mut() {
            cell.hyperlink = Some(0);
        }
        let mut canvas = canvas(source);
        let mut scratch = blank_scratch(3, 1);
        // The replacement paints the very symbol already there.
        scratch.set_string(1, 0, "b", Style::default());
        canvas.overwrite(&[Rect::new(1, 0, 1, 1)], &scratch);
        assert_eq!(canvas.cells()[0].hyperlink, Some(0));
        assert_eq!(canvas.cells()[1].hyperlink, None);
        assert_eq!(canvas.cells()[2].hyperlink, Some(0));
    }

    #[test]
    fn overwrite_clips_to_the_frame_and_ignores_rects_entirely_outside() {
        let mut canvas = canvas(frame("abc"));
        let mut scratch = blank_scratch(3, 1);
        scratch.set_string(0, 0, "XYZ", Style::default());
        canvas.overwrite(&[Rect::new(2, 0, 9, 4), Rect::new(7, 7, 2, 2)], &scratch);
        assert_eq!(text(&canvas), "abZ");
    }

    #[test]
    fn overwrite_treats_adjacent_rects_as_one_replacement() {
        // A wide glyph in the scratch spanning two adjacent rects is not split.
        let mut canvas = canvas(frame("abcd"));
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(1, 0, "漢", Style::default());
        canvas.overwrite(&[Rect::new(1, 0, 1, 1), Rect::new(2, 0, 1, 1)], &scratch);
        assert_eq!(canvas.cells()[1].symbol, "漢");
        assert_eq!(text(&canvas), "a漢 d");
    }

    #[test]
    fn overwrite_blanks_the_uncovered_part_of_a_split_underlying_glyph() {
        // Lead covered, tail (empty symbol) not.
        let mut source = frame("a漢~d");
        source
            .set_hyperlinks(vec!["https://a.example".into()])
            .expect("no cell links yet");
        source.cells_mut()[1].bg = WireColor::Green;
        source.cells_mut()[2].bg = WireColor::Green;
        source.cells_mut()[2].hyperlink = Some(0);
        let mut left = canvas(source);
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(1, 0, "#", Style::default());
        left.overwrite(&[Rect::new(1, 0, 1, 1)], &scratch);
        assert_eq!(text(&left), "a# d");
        // The blanked tail keeps its own style but not the link.
        assert_eq!(left.cells()[2].bg, WireColor::Green);
        assert_eq!(left.cells()[2].hyperlink, None);

        // Tail covered, lead not: the lead is blanked, also with space continuations.
        let mut right = canvas(frame("a漢 d"));
        right.overwrite(&[Rect::new(2, 0, 1, 1)], &{
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
        let mut canvas = canvas(frame("ab~d"));
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(1, 0, "#", Style::default());
        canvas.overwrite(&[Rect::new(1, 0, 1, 1)], &scratch);
        assert_eq!(text(&canvas), "a# d");
    }

    #[test]
    fn overlay_edge_after_narrow_vs16_cell_keeps_the_pane_glyph() {
        let mut source = frame("  ");
        source.cells_mut()[0].symbol = "\u{26a0}\u{fe0f}".into();
        source.cells_mut()[0].grid_width = GridCellWidth::One;
        source.cells_mut()[1].grid_width = GridCellWidth::One;
        let mut canvas = canvas(source);
        let mut scratch = blank_scratch(2, 1);
        scratch.set_string(1, 0, "x", Style::default());

        canvas.overwrite(&[Rect::new(1, 0, 1, 1)], &scratch);

        assert_eq!(canvas.cells()[0].symbol, "\u{26a0}\u{fe0f}");
        assert_eq!(canvas.cells()[0].grid_width, GridCellWidth::One);
        assert_eq!(canvas.cells()[1].symbol, "x");
    }

    #[test]
    fn overwrite_blanks_a_scratch_glyph_that_would_cross_the_boundary() {
        let mut canvas = canvas(frame("abcd"));
        let mut scratch = blank_scratch(4, 1);
        scratch.set_string(1, 0, "漢", Style::default().fg(Color::Red));
        // The union ends inside the glyph: its covered half becomes a blank.
        canvas.overwrite(&[Rect::new(0, 0, 2, 1)], &scratch);
        assert_eq!(text(&canvas), "  cd");
        assert_eq!(canvas.cells()[1].fg, WireColor::Red);
    }

    #[test]
    fn overwrite_uses_the_output_width_rule_for_halfwidth_katakana() {
        let voiced = "\u{ff76}\u{ff9e}";
        assert_eq!(shepr_term::width::text_width(voiced), 2);
        let mut source = frame("a~");
        source.cells_mut()[0].symbol = voiced.into();
        let mut canvas = canvas(source);
        let mut scratch = blank_scratch(2, 1);
        scratch.set_string(0, 0, "#", Style::default());
        canvas.overwrite(&[Rect::new(0, 0, 1, 1)], &scratch);
        assert_eq!(text(&canvas), "# ");
    }

    #[test]
    fn overwrite_repairs_a_split_emoji_variation_glyph() {
        let mut canvas = canvas(
            FrameData::new(
                vec![cell("\u{2764}\u{fe0f}"), cell(""), cell("z")],
                3,
                1,
                None,
                Vec::new(),
            )
            .expect("test frame"),
        );
        let mut scratch = blank_scratch(3, 1);
        scratch.set_string(0, 0, "#", Style::default());

        assert_eq!(shepr_term::width::text_width("\u{2764}\u{fe0f}"), 2);
        canvas.overwrite(&[Rect::new(0, 0, 1, 1)], &scratch);

        assert_eq!(text(&canvas), "# z");
    }

    #[test]
    fn hint_row_style_patch_plus_prefix_overwrite_blanks_a_wide_glyph_at_the_boundary() {
        // The too-small hint restyles its whole row, then overwrites the prefix its text
        // actually wrote; a wide glyph the prefix boundary splits is blanked.
        let mut source = frame("ab漢~cd");
        source
            .set_hyperlinks(vec!["https://a.example".into()])
            .expect("no cell links yet");
        source.cells_mut()[3].hyperlink = Some(0);
        source.cells_mut()[4].hyperlink = Some(0);
        let mut canvas = canvas(source);
        let hint = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        canvas.patch_rect(Rect::new(0, 0, 6, 1), StylePatch::from_style(hint));
        let mut scratch = blank_scratch(6, 1);
        let (written_to, _) = scratch.set_stringn(0, 0, "hi!", 6, hint);
        assert_eq!(written_to, 3);
        canvas.overwrite(&[Rect::new(0, 0, written_to, 1)], &scratch);
        assert_eq!(text(&canvas), "hi! cd");
        // The row keeps the hint style beyond the prefix, links included, and the blanked
        // tail carries it too.
        assert!(canvas.cells().iter().all(|cell| cell.bg == WireColor::Cyan));
        assert_eq!(canvas.cells()[4].hyperlink, Some(0));
        assert_eq!(canvas.cells()[3].hyperlink, None);
    }

    #[test]
    fn a_buffer_past_the_surface_budget_has_no_canvas() {
        let wide = shepr_protocol::MAX_SURFACE_DIMENSION + 1;
        assert_eq!(
            Canvas::from_buffer(&blank_scratch(wide, 1)),
            Err(FrameGridError::InvalidDimensions)
        );
    }

    #[test]
    fn compose_drops_links_the_table_has_no_room_for() {
        let mut target = frame("..");
        target
            .set_hyperlinks(vec![String::new(); shepr_protocol::MAX_SURFACE_HYPERLINKS])
            .expect("table at its budget");
        let mut target = canvas(target);
        let mut source = frame("X");
        source
            .set_hyperlinks(vec!["https://new.example".into()])
            .expect("no cell links yet");
        source.cells_mut()[0].hyperlink = Some(0);
        target.compose_pane(&source, Rect::new(0, 0, 1, 1));
        assert_eq!(text(&target), "X.");
        assert_eq!(target.cells()[0].hyperlink, None);
        assert_eq!(
            target.hyperlinks().len(),
            shepr_protocol::MAX_SURFACE_HYPERLINKS
        );
    }

    #[test]
    fn compose_keeps_shapes_remaps_links_and_repairs_a_split_target_glyph() {
        let mut target = frame("|漢~|");
        target
            .set_hyperlinks(vec!["https://old.example".into()])
            .expect("no cell links yet");
        target.cells_mut()[3].hyperlink = Some(0);
        let mut target = canvas(target);
        let mut source = frame("XY");
        source
            .set_hyperlinks(vec!["https://new.example".into()])
            .expect("no cell links yet");
        source.cells_mut()[0].hyperlink = Some(0);
        source.cells_mut()[1].style.underline = UnderlineStyle::Dotted;
        source.cells_mut()[1].hyperlink = Some(7);
        // Pasting over the tail only must not leave the target's lead as half a glyph.
        target.compose_pane(&source, Rect::new(2, 0, 1, 1));
        assert_eq!(text(&target), "| X|");
        assert_eq!(target.hyperlinks().len(), 2);
        assert_eq!(target.cells()[2].hyperlink, Some(1));
        assert_eq!(target.cells()[3].hyperlink, Some(0));

        let mut target = canvas(frame("...."));
        target.compose_pane(&source, Rect::new(1, 0, 2, 1));
        assert_eq!(target.cells()[2].style.underline, UnderlineStyle::Dotted);
        assert_eq!(
            target.cells()[2].hyperlink,
            None,
            "dangling link is dropped"
        );
    }

    #[test]
    fn compose_blanks_a_source_glyph_cut_by_the_clip() {
        let mut target = canvas(frame("...."));
        let source = frame("a漢~");
        target.compose_pane(&source, Rect::new(0, 0, 2, 1));
        assert_eq!(text(&target), "a ..");
    }

    #[test]
    fn compose_turns_a_cut_pane_glyph_into_the_servers_pane_blank() {
        let mut target = canvas(frame("...."));
        let mut source = frame("a\u{754c}~");
        source.cells_mut()[0].grid_width = GridCellWidth::One;
        source.cells_mut()[1].grid_width = GridCellWidth::WideLead;
        source.cells_mut()[2].grid_width = GridCellWidth::WideTail;
        source.cells_mut()[1].style.underline = UnderlineStyle::Single;
        target.compose_pane(&source, Rect::new(0, 0, 2, 1));
        assert_eq!(text(&target), "a ..");
        assert_eq!(target.cells()[1].grid_width, GridCellWidth::One);
        assert_eq!(target.cells()[1].style.underline, UnderlineStyle::None);
    }

    #[test]
    fn compose_clip_edge_uses_grid_width_for_narrow_vs16_cells() {
        let mut target = canvas(frame(".."));
        let mut source = frame("  ");
        source.cells_mut()[0].symbol = "\u{26a0}\u{fe0f}".into();
        source.cells_mut()[0].grid_width = GridCellWidth::One;
        source.cells_mut()[1].grid_width = GridCellWidth::One;

        target.compose_pane(&source, Rect::new(0, 0, 1, 1));

        assert_eq!(target.cells()[0].symbol, "\u{26a0}\u{fe0f}");
        assert_eq!(target.cells()[0].grid_width, GridCellWidth::One);
        assert_eq!(target.cells()[1].symbol, ".");
    }

    #[test]
    fn compose_clips_to_the_target_and_takes_the_cursor_from_the_visible_part() {
        let mut target = canvas(frame("...."));
        let mut source = frame("abcd");
        source.set_cursor(Some(CursorState {
            x: 3,
            y: 0,
            visible: true,
            shape: CursorShapeParam::Default,
        }));
        target.compose_pane(&source, Rect::new(3, 0, 4, 1));
        assert_eq!(text(&target), "...a");
        assert_eq!(target.cursor(), None);
    }
}
