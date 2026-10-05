//! Plain-text formatting over an alacritty grid.

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};

use super::{CellText, cell_text};

/// Format the cells from `start` to `end` (inclusive, reading order) of `grid`.
/// Both points must be valid grid points with `start <= end`.
///
/// Soft-wrapped rows are joined into their logical line, hard line breaks are
/// `\n`, and trailing blanks are trimmed from a logical line's last row only;
/// rows before it in a soft-wrapped line keep every cell. Trailing blank lines
/// are dropped.
pub(super) fn format_range(grid: &Grid<Cell>, start: Point, end: Point) -> String {
    let mut out = String::new();
    if grid.columns() == 0 || start > end {
        return out;
    }
    let last_col = grid.columns() - 1;
    let mut pending: Vec<&Cell> = Vec::new();
    // Byte length of `out` after the last emitted line that had content, so
    // trailing blank lines can be dropped.
    let mut content_end = None;

    let mut line = start.line;
    while line <= end.line {
        let row = &grid[line];
        let first_col = if line == start.line {
            start.column.0
        } else {
            0
        };
        let last_in_range = if line == end.line {
            end.column.0
        } else {
            last_col
        }
        .min(last_col);
        // Where this row's cells begin: trimming stops at the row's start.
        let row_start = pending.len();
        if first_col <= last_in_range {
            let mut col = first_col;
            // A range starting on a wide character's spacer includes the character.
            if col > 0 && row[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) {
                col -= 1;
            }
            while col <= last_in_range {
                let cell = &row[Column(col)];
                if !cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    pending.push(cell);
                }
                col += 1;
            }
        }

        let is_last = line == end.line;
        let soft_wrapped = row[Column(last_col)].flags.contains(Flags::WRAPLINE);
        let join = soft_wrapped && !is_last && last_in_range == last_col;
        if !join {
            let had_content = emit_line(&mut out, &pending, row_start);
            pending.clear();
            if had_content {
                content_end = Some(out.len());
            }
            if !is_last {
                out.push('\n');
            }
        }
        line = Line(line.0 + 1);
    }

    out.truncate(content_end.unwrap_or(0));
    out
}

/// Emits one logical line whose last row's cells start at `last_row_start`;
/// only that row's trailing blanks are trimmed. Returns whether anything
/// visible was written.
fn emit_line(out: &mut String, cells: &[&Cell], last_row_start: usize) -> bool {
    let keep = cells[last_row_start..]
        .iter()
        .rposition(|cell| !matches!(cell_text(cell), CellText::Empty))
        .map_or(last_row_start, |index| last_row_start + index + 1);
    for cell in &cells[..keep] {
        push_cell_text(out, cell);
    }
    keep > 0
}

fn push_cell_text(out: &mut String, cell: &Cell) {
    match cell_text(cell) {
        CellText::Empty => out.push(' '),
        CellText::Grapheme { base, zerowidth } => {
            out.push(base);
            out.extend(zerowidth.iter().copied());
        }
    }
}

/// Clamp a (column, line) pair to the grid, returning `None` for lines that
/// do not exist.
pub(super) fn grid_point(grid: &Grid<Cell>, line: Line, column: u16) -> Option<Point> {
    if line < grid.topmost_line() || line > grid.bottommost_line() || grid.columns() == 0 {
        return None;
    }
    Some(Point::new(
        line,
        Column(usize::from(column).min(grid.columns() - 1)),
    ))
}
