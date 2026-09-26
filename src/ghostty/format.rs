//! Plain-text and VT formatters over an alacritty grid.
//!
//! The VT output is replayed into a fresh terminal when history is restored
//! (`recent_unwrapped_ansi_snapshot` -> `seed_history_ansi`) and after some resizes, so
//! it must round-trip through our own parser: every style change is written as
//! a full `SGR 0;...` reset, soft-wrapped rows are joined when unwrapping so the
//! replay reflows them, hard line breaks are `\r\n`, and no SGR or OSC 8 state
//! is left open at a line break or at the end.

use std::fmt::Write as _;

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use super::KITTY_UNICODE_PLACEHOLDER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    Plain,
    Vt,
}

/// Cell flags that change how a cell renders and therefore belong in SGR.
const STYLE_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StyleKey {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline_color: Option<Color>,
}

impl StyleKey {
    const DEFAULT: Self = Self {
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        flags: Flags::empty(),
        underline_color: None,
    };

    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & STYLE_FLAGS,
            underline_color: cell.underline_color(),
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::DEFAULT
    }
}

struct VtState {
    style: StyleKey,
    link: Option<(String, String)>,
}

impl VtState {
    fn new() -> Self {
        Self {
            style: StyleKey::DEFAULT,
            link: None,
        }
    }

    fn close(&mut self, out: &mut String) {
        if !self.style.is_default() {
            out.push_str("\x1b[0m");
            self.style = StyleKey::DEFAULT;
        }
        if self.link.take().is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
    }
}

/// Format the cells from `start` to `end` (inclusive, reading order) of `grid`.
/// Both points must be valid grid points with `start <= end`.
pub(super) fn format_range(
    grid: &Grid<Cell>,
    start: Point,
    end: Point,
    rectangle: bool,
    format: Format,
    unwrap: bool,
    trim: bool,
) -> String {
    let mut out = String::new();
    if grid.columns() == 0 || start > end {
        return out;
    }
    let last_col = grid.columns() - 1;
    let mut vt = VtState::new();
    let mut pending: Vec<&Cell> = Vec::new();
    // Byte length of `out` after the last emitted line that had content, so
    // trailing blank lines can be dropped when trimming.
    let mut content_end = 0usize;

    let mut line = start.line;
    while line <= end.line {
        let row = &grid[line];
        let (first_col, last_in_range) = if rectangle {
            (start.column.0, end.column.0)
        } else {
            (
                if line == start.line {
                    start.column.0
                } else {
                    0
                },
                if line == end.line {
                    end.column.0
                } else {
                    last_col
                },
            )
        };
        let last_in_range = last_in_range.min(last_col);
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
        let join = unwrap && soft_wrapped && !is_last && !rectangle && last_in_range == last_col;
        if !join {
            let had_content = emit_line(&mut out, &pending, format, trim, &mut vt);
            pending.clear();
            if format == Format::Vt {
                vt.close(&mut out);
            }
            if had_content || !trim {
                content_end = out.len();
            }
            if !is_last {
                out.push_str(match format {
                    Format::Plain => "\n",
                    Format::Vt => "\r\n",
                });
            }
        }
        line = Line(line.0 + 1);
    }

    if trim {
        out.truncate(content_end);
    }
    out
}

/// Emits one logical line. Returns whether anything visible was written.
fn emit_line(
    out: &mut String,
    cells: &[&Cell],
    format: Format,
    trim: bool,
    vt: &mut VtState,
) -> bool {
    let keep = if trim {
        cells
            .iter()
            .rposition(|cell| !is_trimmable(cell, format))
            .map_or(0, |index| index + 1)
    } else {
        cells.len()
    };
    for cell in &cells[..keep] {
        if format == Format::Vt {
            let style = StyleKey::of(cell);
            if style != vt.style {
                push_sgr(out, &style);
                vt.style = style;
            }
            let link = cell
                .hyperlink()
                .map(|link| (link.id().to_owned(), link.uri().to_owned()));
            if link != vt.link {
                if vt.link.is_some() {
                    out.push_str("\x1b]8;;\x1b\\");
                }
                if let Some((id, uri)) = &link {
                    // Auto-generated ids are local to this terminal; let the
                    // replaying terminal allocate its own.
                    if id.ends_with("_alacritty") {
                        let _ = write!(out, "\x1b]8;;{uri}\x1b\\");
                    } else {
                        let _ = write!(out, "\x1b]8;id={id};{uri}\x1b\\");
                    }
                }
                vt.link = link;
            }
        }
        push_cell_text(out, cell);
    }
    keep > 0
}

fn is_blank_text(cell: &Cell) -> bool {
    let no_zerowidth = cell.zerowidth().is_none_or(<[char]>::is_empty);
    ((cell.c == ' ' || cell.c == '\t') && no_zerowidth)
        || u32::from(cell.c) == KITTY_UNICODE_PLACEHOLDER
}

fn is_trimmable(cell: &Cell, format: Format) -> bool {
    if !is_blank_text(cell) {
        return false;
    }
    match format {
        Format::Plain => true,
        // Keep blanks that still paint something (background fills, inverse,
        // underlines, strikethrough) or carry a link.
        Format::Vt => {
            cell.bg == Color::Named(NamedColor::Background)
                && !cell
                    .flags
                    .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
                && cell.hyperlink().is_none()
        }
    }
}

fn push_cell_text(out: &mut String, cell: &Cell) {
    if u32::from(cell.c) == KITTY_UNICODE_PLACEHOLDER {
        out.push(' ');
        return;
    }
    out.push(if cell.c == '\t' { ' ' } else { cell.c });
    if let Some(zerowidth) = cell.zerowidth() {
        out.extend(zerowidth.iter().copied());
    }
}

fn push_sgr(out: &mut String, style: &StyleKey) {
    out.push_str("\x1b[0");
    let flags = style.flags;
    if flags.contains(Flags::BOLD) {
        out.push_str(";1");
    }
    if flags.contains(Flags::DIM) {
        out.push_str(";2");
    }
    if flags.contains(Flags::ITALIC) {
        out.push_str(";3");
    }
    if flags.contains(Flags::UNDERLINE) {
        out.push_str(";4");
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        out.push_str(";4:2");
    } else if flags.contains(Flags::UNDERCURL) {
        out.push_str(";4:3");
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        out.push_str(";4:4");
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        out.push_str(";4:5");
    }
    if flags.contains(Flags::INVERSE) {
        out.push_str(";7");
    }
    if flags.contains(Flags::HIDDEN) {
        out.push_str(";8");
    }
    if flags.contains(Flags::STRIKEOUT) {
        out.push_str(";9");
    }
    push_color(out, style.fg, ColorSlot::Foreground);
    push_color(out, style.bg, ColorSlot::Background);
    if let Some(color) = style.underline_color {
        push_color(out, color, ColorSlot::Underline);
    }
    out.push('m');
}

#[derive(Clone, Copy)]
enum ColorSlot {
    Foreground,
    Background,
    Underline,
}

fn push_color(out: &mut String, color: Color, slot: ColorSlot) {
    match color {
        Color::Named(named) => {
            let index = named as usize;
            if index >= 16 {
                // Foreground/Background and the renderer-only dim/bright
                // variants all mean "default" here.
                return;
            }
            match slot {
                ColorSlot::Foreground if index < 8 => {
                    let _ = write!(out, ";{}", 30 + index);
                }
                ColorSlot::Foreground => {
                    let _ = write!(out, ";{}", 90 + index - 8);
                }
                ColorSlot::Background if index < 8 => {
                    let _ = write!(out, ";{}", 40 + index);
                }
                ColorSlot::Background => {
                    let _ = write!(out, ";{}", 100 + index - 8);
                }
                ColorSlot::Underline => {
                    let _ = write!(out, ";58;5;{index}");
                }
            }
        }
        Color::Indexed(index) => {
            let prefix = match slot {
                ColorSlot::Foreground => 38,
                ColorSlot::Background => 48,
                ColorSlot::Underline => 58,
            };
            let _ = write!(out, ";{prefix};5;{index}");
        }
        Color::Spec(rgb) => {
            let prefix = match slot {
                ColorSlot::Foreground => 38,
                ColorSlot::Background => 48,
                ColorSlot::Underline => 58,
            };
            let _ = write!(out, ";{prefix};2;{};{};{}", rgb.r, rgb.g, rgb.b);
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
