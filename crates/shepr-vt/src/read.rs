use super::*;
use alacritty_terminal::grid::GridCell;

// A cell flag bit alacritty leaves unused, marking restored history. Its
// scroll and reflow code moves Cell values, erases and recycled rows reset
// them to a fresh cell, and text writes replace the flags from the cursor
// template, which only SGR sets; so the bit follows seeded cells, is dropped
// by any live change, and adds no work to parser input. Nothing else
// interprets it: style conversion, the formatters and alacritty's selection
// test named flags only.
const SEEDED_ROW: Flags = Flags::from_bits_retain(1 << 15);
// A bump of the pinned alacritty that claims this bit fails the build.
const _: () = assert!(Flags::all().bits() & SEEDED_ROW.bits() == 0);

impl Terminal {
    /// Marks every cell in retained screen row `y` as restored history.
    /// Marks stay with the cells when alacritty scrolls, evicts or reflows
    /// their rows. `false` means that the row is not retained.
    pub fn mark_screen_row_seeded(&mut self, y: ScreenRow) -> bool {
        let Some(line) = self.screen_line(y) else {
            return false;
        };
        // Borrowing the whole row mutably marks all of it occupied, so the
        // reset that recycles this row later clears every mark, not just the
        // cells written before.
        for cell in &mut self.term.grid_mut()[line][..] {
            cell.flags.insert(SEEDED_ROW);
        }
        true
    }

    /// Visits the cells of screen row `y` and reports whether its retained
    /// content is still seeded. Reflow may add empty cells at a row's end,
    /// which have no provenance bit; a live cell or an unmarked gap before
    /// later seeded cells makes the row live. `None` means that the row is
    /// not retained.
    pub fn visit_screen_row_text_with_seeded(
        &self,
        y: ScreenRow,
        scratch: &mut String,
        visit: impl FnMut(u16, CellWide, &str),
    ) -> Option<(RowWrap, bool)> {
        let mut saw_seeded = false;
        let mut saw_unseeded = false;
        let mut live = false;
        let wrap = self.visit_screen_row_text_inner(y, scratch, visit, |cell| {
            let is_seeded = cell.flags.contains(SEEDED_ROW);
            let reflow_spacer = cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);
            if is_seeded {
                if saw_unseeded {
                    live = true;
                }
                saw_seeded = true;
            } else if !reflow_spacer {
                saw_unseeded = true;
                live |= !cell.is_empty();
            }
        })?;
        Some((wrap, saw_seeded && !live))
    }

    /// Visits the cells of screen row `y` without allocating: `visit` gets
    /// each cell's column, width class and text. The text is what readers
    /// show for the cell: its grapheme, or a single space for blank cells,
    /// wide-character spacers and kitty placeholder cells; callers skip
    /// `SpacerTail` cells where a wide character's second column must not
    /// produce text. `scratch` holds the text between calls. `None` when the
    /// row is not retained.
    pub fn visit_screen_row_text(
        &self,
        y: ScreenRow,
        scratch: &mut String,
        visit: impl FnMut(u16, CellWide, &str),
    ) -> Option<RowWrap> {
        self.visit_screen_row_text_inner(y, scratch, visit, |_| {})
    }

    // Keep the per-cell hook statically dispatched. The ordinary row visitor
    // passes a known no-op, so optimized readers add no per-cell test or
    // dynamic dispatch on hot paths.
    #[inline(always)]
    fn visit_screen_row_text_inner(
        &self,
        y: ScreenRow,
        scratch: &mut String,
        mut visit: impl FnMut(u16, CellWide, &str),
        mut on_cell: impl FnMut(&Cell),
    ) -> Option<RowWrap> {
        let line = self.screen_line(y)?;
        let grid = self.term.grid();
        let columns = grid.columns();
        let row = &grid[line];
        for (x, cell) in row[..].iter().take(columns).enumerate() {
            let Ok(x) = u16::try_from(x) else {
                break;
            };
            on_cell(cell);
            cell_text_into(cell, scratch);
            visit(x, cell_wide(cell), scratch.as_str());
        }
        Some(self.row_wrap(line))
    }

    /// The wrap flags of screen row `y`, `None` when the row is not retained.
    pub fn screen_row_wrap(&self, y: ScreenRow) -> Option<RowWrap> {
        self.screen_line(y).map(|line| self.row_wrap(line))
    }

    /// The single rule for the two wrap flags exposed by row readers.
    fn row_wrap(&self, line: Line) -> RowWrap {
        let grid = self.term.grid();
        let last_column = Column(grid.columns() - 1);
        RowWrap {
            soft_wrapped: grid[line][last_column].flags.contains(Flags::WRAPLINE),
            wrap_continuation: line > grid.topmost_line()
                && grid[Line(line.0 - 1)][last_column]
                    .flags
                    .contains(Flags::WRAPLINE),
        }
    }

    /// Converts a screen row (0 = oldest retained line) to an alacritty line.
    fn screen_line(&self, y: ScreenRow) -> Option<Line> {
        let history_size = i64::try_from(self.term.history_size()).unwrap_or(i64::MAX);
        let line = i64::try_from(y.0).ok()? - history_size;
        let line = Line(i32::try_from(line).ok()?);
        (line >= self.term.topmost_line() && line <= self.term.bottommost_line()).then_some(line)
    }

    /// Converts a viewport row (0 = top of what is displayed) to an alacritty line.
    fn viewport_line(&self, y: ViewportRow) -> Option<Line> {
        let y = usize::from(y.0);
        if y >= self.term.screen_lines() {
            return None;
        }
        let display_offset = i64::try_from(self.term.grid().display_offset()).unwrap_or(i64::MAX);
        let line = i64::try_from(y).unwrap_or(i64::MAX) - display_offset;
        Some(Line(i32::try_from(line).ok()?))
    }

    pub fn viewport_hyperlink_uri(&self, x: u16, y: ViewportRow) -> Result<Option<String>, Error> {
        let line = self
            .viewport_line(y)
            .ok_or(Error("viewport row out of range"))?;
        let column = usize::from(x);
        if column >= self.term.columns() {
            return Err(Error("viewport column out of range"));
        }
        Ok(self.term.grid()[line][Column(column)]
            .hyperlink()
            .map(|link| link.uri().to_owned()))
    }

    pub fn read_text_screen(
        &self,
        start: Point<ScreenRow>,
        end: Point<ScreenRow>,
    ) -> Result<String, Error> {
        let grid = self.term.grid();
        let start = self
            .screen_line(start.row)
            .and_then(|line| format::grid_point(grid, line, start.col))
            .ok_or(Error("selection start out of range"))?;
        let end = self
            .screen_line(end.row)
            .and_then(|line| format::grid_point(grid, line, end.col))
            .ok_or(Error("selection end out of range"))?;
        Ok(format::format_range(grid, start, end, Format::Plain))
    }

    /// A VT read for a range that lies inside one logical line's continuation
    /// or ends inside a logical line, so a long line can be read a few rows
    /// at a time. The read starts from the state `carry` holds (default at a
    /// logical line start). With `open_end` the last row must be a
    /// soft-wrapped row whose line continues in the next row: every cell of it
    /// is emitted, nothing is closed or trimmed, and `carry` holds the state
    /// the read of the next rows starts from. Joining such reads with no
    /// separator gives exactly the bytes one read of the whole line gives.
    /// Without `open_end` the range ends its line like any read, and `carry`
    /// is left at the default. `carry` is unchanged on error.
    ///
    /// The text is not cut back to its last content: trailing blank lines stay
    /// in it. The second value is the byte length the text has when cut there,
    /// `None` when the range has no content at all, `Some(0)` when it only
    /// finishes a line that began before it (a carry that has started) without
    /// emitting a cell, and the whole length with `open_end`. The caller that
    /// joins reads picks the end of the whole read from the last read that has
    /// content.
    pub fn read_ansi_screen_carrying(
        &self,
        start: Point<ScreenRow>,
        end: Point<ScreenRow>,
        carry: &mut format::AnsiCarry,
        open_end: bool,
    ) -> Result<(String, Option<usize>), Error> {
        let grid = self.term.grid();
        let start = self
            .screen_line(start.row)
            .and_then(|line| format::grid_point(grid, line, start.col))
            .ok_or(Error("selection start out of range"))?;
        let end = self
            .screen_line(end.row)
            .and_then(|line| format::grid_point(grid, line, end.col))
            .ok_or(Error("selection end out of range"))?;
        Ok(format::format_range_carrying(
            grid,
            start,
            end,
            Format::Vt,
            carry,
            open_end,
        ))
    }
}
