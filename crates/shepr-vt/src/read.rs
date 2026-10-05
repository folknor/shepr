use super::*;

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
        let grid = self.emu.term.grid();
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
        let grid = self.emu.term.grid();
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
        let history_size = i64::try_from(self.emu.term.history_size()).unwrap_or(i64::MAX);
        let line = i64::try_from(y.0).ok()? - history_size;
        let line = Line(i32::try_from(line).ok()?);
        (line >= self.emu.term.topmost_line() && line <= self.emu.term.bottommost_line())
            .then_some(line)
    }

    /// Converts a viewport row (0 = top of what is displayed) to an alacritty line.
    fn viewport_line(&self, y: ViewportRow) -> Option<Line> {
        let y = usize::from(y.0);
        if y >= self.emu.term.screen_lines() {
            return None;
        }
        let display_offset =
            i64::try_from(self.emu.term.grid().display_offset()).unwrap_or(i64::MAX);
        let line = i64::try_from(y).unwrap_or(i64::MAX) - display_offset;
        Some(Line(i32::try_from(line).ok()?))
    }

    pub fn viewport_hyperlink_uri(
        &self,
        x: u16,
        y: ViewportRow,
    ) -> Result<Option<String>, ReadError> {
        let line = self.viewport_line(y).ok_or(ReadError::RowNotRetained)?;
        let column = usize::from(x);
        if column >= self.emu.term.columns() {
            return Err(ReadError::ColumnOutOfRange);
        }
        Ok(self.emu.term.grid()[line][Column(column)]
            .hyperlink()
            .map(|link| link.uri().to_owned()))
    }

    pub fn read_text_screen(
        &self,
        start: Point<ScreenRow>,
        end: Point<ScreenRow>,
    ) -> Result<String, ReadError> {
        let grid = self.emu.term.grid();
        let start = self
            .screen_line(start.row)
            .and_then(|line| format::grid_point(grid, line, start.col))
            .ok_or(ReadError::RowNotRetained)?;
        let end = self
            .screen_line(end.row)
            .and_then(|line| format::grid_point(grid, line, end.col))
            .ok_or(ReadError::RowNotRetained)?;
        Ok(format::format_range(grid, start, end))
    }
}
