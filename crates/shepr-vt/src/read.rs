use super::*;

#[derive(Clone, Copy)]
#[expect(
    variant_size_differences,
    reason = "a Copy row coordinate of sixteen bytes, passed by value"
)]
enum Coordinates {
    Screen(ScreenRow),
    Viewport(ViewportRow),
}

impl Terminal {
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
        mut visit: impl FnMut(u16, CellWide, &str),
    ) -> Option<RowWrap> {
        let line = self.screen_line(y)?;
        let grid = self.term.grid();
        let columns = grid.columns();
        let row = &grid[line];
        for (x, cell) in row[..].iter().take(columns).enumerate() {
            let Ok(x) = u16::try_from(x) else {
                break;
            };
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

    pub fn screen_cell(&self, x: u16, y: ScreenRow) -> Result<(CellWide, Vec<u32>), Error> {
        let line = self
            .screen_line(y)
            .ok_or(Error("screen row out of range"))?;
        let column = usize::from(x);
        if column >= self.term.columns() {
            return Err(Error("screen column out of range"));
        }
        let cell = &self.term.grid()[line][Column(column)];
        Ok((cell_wide(cell), cell_graphemes(cell)))
    }

    pub fn screen_text_rows(&self) -> Vec<ScreenTextRow> {
        self.screen_text_rows_range(ScreenRow(0), ScreenRow(usize::MAX))
    }

    pub fn screen_text_rows_range(
        &self,
        start_row: ScreenRow,
        end_row_exclusive: ScreenRow,
    ) -> Vec<ScreenTextRow> {
        let total_rows = self.term.total_lines();
        let start_row = start_row.0.min(total_rows);
        let end_row_exclusive = end_row_exclusive.0.min(total_rows).max(start_row);
        let grid = self.term.grid();
        let columns = grid.columns();
        let mut rows = Vec::with_capacity(end_row_exclusive - start_row);
        for y in start_row..end_row_exclusive {
            let Some(line) = self.screen_line(ScreenRow(y)) else {
                break;
            };
            let row = &grid[line];
            let cells = (0..columns)
                .map(|x| {
                    let cell = &row[Column(x)];
                    ScreenTextCell {
                        wide: cell_wide(cell),
                        graphemes: cell_graphemes(cell),
                    }
                })
                .collect();
            let wrap = self.row_wrap(line);
            rows.push(ScreenTextRow { cells, wrap });
        }
        rows
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

    pub fn read_text_viewport(
        &self,
        start: Point<ViewportRow>,
        end: Point<ViewportRow>,
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start.map_row(Coordinates::Viewport),
            end.map_row(Coordinates::Viewport),
            rectangle,
            Format::Plain,
            true,
        )
    }

    pub fn read_ansi_viewport(
        &self,
        start: Point<ViewportRow>,
        end: Point<ViewportRow>,
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start.map_row(Coordinates::Viewport),
            end.map_row(Coordinates::Viewport),
            rectangle,
            Format::Vt,
            false,
        )
    }

    pub fn read_text_screen(
        &self,
        start: Point<ScreenRow>,
        end: Point<ScreenRow>,
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start.map_row(Coordinates::Screen),
            end.map_row(Coordinates::Screen),
            rectangle,
            Format::Plain,
            true,
        )
    }

    pub fn read_ansi_screen(
        &self,
        start: Point<ScreenRow>,
        end: Point<ScreenRow>,
        rectangle: bool,
        unwrap: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start.map_row(Coordinates::Screen),
            end.map_row(Coordinates::Screen),
            rectangle,
            Format::Vt,
            unwrap,
        )
    }

    fn read_range(
        &self,
        start: Point<Coordinates>,
        end: Point<Coordinates>,
        rectangle: bool,
        format: Format,
        unwrap: bool,
    ) -> Result<String, Error> {
        let to_line = |coordinate| match coordinate {
            Coordinates::Screen(y) => self.screen_line(y),
            Coordinates::Viewport(y) => self.viewport_line(y),
        };
        let grid = self.term.grid();
        let start = to_line(start.row)
            .and_then(|line| format::grid_point(grid, line, start.col))
            .ok_or(Error("selection start out of range"))?;
        let end = to_line(end.row)
            .and_then(|line| format::grid_point(grid, line, end.col))
            .ok_or(Error("selection end out of range"))?;
        Ok(format::format_range(
            grid,
            start,
            end,
            format::RangeOptions {
                rectangle,
                format,
                unwrap,
                trim: true,
            },
        ))
    }

    /// [`read_ansi_screen`] (unwrapped) for a range that lies inside one
    /// logical line's continuation or ends inside a logical line, so a long
    /// line can be read a few rows at a time. The read starts from the state
    /// `carry` holds (default at a logical line start). With `open_end` the
    /// last row must be a soft-wrapped row whose line continues in the next
    /// row: every cell of it is emitted, nothing is closed or trimmed, and
    /// `carry` holds the state the read of the next rows starts from. Joining
    /// such reads with no separator gives exactly the bytes one read of the
    /// whole line gives. Without `open_end` the range ends its line like any
    /// read, and `carry` is left at the default. `carry` is unchanged on
    /// error.
    ///
    /// Unlike [`read_ansi_screen`] the text is not cut back to its last
    /// content: trailing blank lines stay in it. The second value is the byte
    /// length the text has when cut there, `None` when the range has no
    /// content at all, `Some(0)` when it only finishes a line that began
    /// before it (a carry that has started) without emitting a cell, and the
    /// whole length with `open_end`. The caller that joins reads picks the
    /// end of the whole read from the last read that has content.
    ///
    /// [`read_ansi_screen`]: Self::read_ansi_screen
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
            format::RangeOptions {
                rectangle: false,
                format: Format::Vt,
                unwrap: true,
                trim: true,
            },
            carry,
            open_end,
        ))
    }
}
