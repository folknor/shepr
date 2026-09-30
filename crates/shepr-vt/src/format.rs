//! Plain-text and VT formatters over an alacritty grid.
//!
//! The VT output is replayed into a fresh terminal when history is restored
//! (`recent_unwrapped_ansi_snapshot` -> `seed_history_ansi`), so
//! it must round-trip through our own parser: every style change is written as
//! a full `SGR 0;...` reset, soft-wrapped rows are joined when unwrapping so the
//! replay reflows them, hard line breaks are `\r\n`, and no SGR or OSC 8 state
//! is left open at a line break or at the end.

use std::fmt::Write as _;

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use vte::ansi::{Color, NamedColor};

use super::{CellText, UnderlineStyle, cell_text};

const OSC8_CLOSE_SEQUENCE: &str = "\x1b]8;;\x1b\\";
const UNDERLINE_SGR: &[(UnderlineStyle, &str)] = &[
    (UnderlineStyle::Single, ";4"),
    (UnderlineStyle::Double, ";4:2"),
    (UnderlineStyle::Curly, ";4:3"),
    (UnderlineStyle::Dotted, ";4:4"),
    (UnderlineStyle::Dashed, ";4:5"),
];

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

#[derive(Clone, Debug, PartialEq, Eq)]
struct VtState {
    style: StyleKey,
    link: Option<(String, String)>,
}

/// What a VT read leaves open where it stops inside a logical line, and what
/// the read that continues that line must start from: the SGR style and
/// OSC 8 link in effect after the last cell, and whether the line has emitted
/// cells at all. A read that starts at a logical line start uses the default,
/// and every read that ends a line leaves the default (all state is closed at
/// a line end). Reading a long line in pieces with this carried from each
/// piece into the next produces exactly the bytes one read of the whole line
/// would.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnsiCarry {
    vt: VtState,
    started: bool,
}

impl Default for AnsiCarry {
    fn default() -> Self {
        Self {
            vt: VtState::new(),
            started: false,
        }
    }
}

impl AnsiCarry {
    /// Whether this is the state of a read that starts on a line start.
    pub fn is_fresh(&self) -> bool {
        *self == Self::default()
    }
}

/// How a range is read.
#[derive(Clone, Copy)]
pub(super) struct RangeOptions {
    pub(super) rectangle: bool,
    pub(super) format: Format,
    pub(super) unwrap: bool,
    pub(super) trim: bool,
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
            out.push_str(OSC8_CLOSE_SEQUENCE);
        }
    }
}

/// Format the cells from `start` to `end` (inclusive, reading order) of `grid`.
/// Both points must be valid grid points with `start <= end`.
///
/// Trailing blanks are trimmed from a logical line's last row only: the rows
/// of a soft-wrapped line before it keep every cell, so a line can be read in
/// pieces (see [`format_range_carrying`]) without a later piece changing what
/// an earlier one emitted.
pub(super) fn format_range(
    grid: &Grid<Cell>,
    start: Point,
    end: Point,
    options: RangeOptions,
) -> String {
    let (mut text, content_end) =
        format_range_carrying(grid, start, end, options, &mut AnsiCarry::default(), false);
    if options.trim {
        text.truncate(content_end.unwrap_or(0));
    }
    text
}

/// [`format_range`] for a range that starts inside a logical line, from the
/// state `carry` holds, and with `open_end` may stop inside one: the last row
/// is then a soft-wrapped row whose line continues in the next row. Such a
/// range emits every cell of its last row, trims nothing, closes nothing and
/// leaves its state in `carry` for the read of the rest of the line. Without
/// `open_end` the range ends its line and `carry` is left fresh.
///
/// The text is never truncated to its content: the second value is the byte
/// length that truncation would keep, so the caller decides where the whole
/// read ends. Per-line trailing trimming is unchanged; only trailing blank
/// lines (and the line breaks before them) are left in. `None` means the
/// range has no content. `Some(0)` is real: the range finishes a logical line
/// that began before it (`carry.started`) without emitting a cell. With
/// `open_end` the whole text is content, because the line it stops inside
/// has started. Without `trim` every line is content.
pub(super) fn format_range_carrying(
    grid: &Grid<Cell>,
    start: Point,
    end: Point,
    options: RangeOptions,
    carry: &mut AnsiCarry,
    open_end: bool,
) -> (String, Option<usize>) {
    let RangeOptions {
        rectangle,
        format,
        unwrap,
        trim,
    } = options;
    let mut out = String::new();
    if grid.columns() == 0 || start > end {
        return (out, None);
    }
    let last_col = grid.columns() - 1;
    let mut vt = carry.vt.clone();
    // Whether the first logical line of the range began before it.
    let mut continuing = carry.started;
    let mut pending: Vec<&Cell> = Vec::new();
    // Byte length of `out` after the last emitted line that had content, so
    // trailing blank lines can be dropped when trimming.
    let mut content_end: Option<usize> = None;

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
        let join = unwrap && soft_wrapped && !is_last && !rectangle && last_in_range == last_col;
        if !join {
            if is_last && open_end {
                emit_cells(&mut out, &pending, format, &mut vt);
                carry.vt = vt;
                carry.started = true;
                let len = out.len();
                return (out, Some(len));
            }
            let had_content =
                emit_line(&mut out, &pending, row_start, format, trim, &mut vt) || continuing;
            continuing = false;
            pending.clear();
            if format == Format::Vt {
                vt.close(&mut out);
            }
            if had_content || !trim {
                content_end = Some(out.len());
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

    *carry = AnsiCarry::default();
    (out, content_end)
}

/// Emits one logical line whose last row's cells start at `last_row_start`;
/// only that row's trailing blanks are trimmed. Returns whether anything
/// visible was written.
fn emit_line(
    out: &mut String,
    cells: &[&Cell],
    last_row_start: usize,
    format: Format,
    trim: bool,
    vt: &mut VtState,
) -> bool {
    let keep = if trim {
        cells[last_row_start..]
            .iter()
            .rposition(|cell| !is_trimmable(cell, format))
            .map_or(last_row_start, |index| last_row_start + index + 1)
    } else {
        cells.len()
    };
    emit_cells(out, &cells[..keep], format, vt);
    keep > 0
}

/// Writes `cells`, with the style and link changes between them, from the
/// state `vt` holds to the state after the last cell.
fn emit_cells(out: &mut String, cells: &[&Cell], format: Format, vt: &mut VtState) {
    for cell in cells {
        if format == Format::Vt {
            let style = StyleKey::of(cell);
            if style != vt.style {
                push_sgr(out, &style);
                vt.style = style;
            }
            let link = cell.hyperlink();
            let same_link = match (&link, &vt.link) {
                (None, None) => true,
                (Some(link), Some((id, uri))) => link.id() == id && link.uri() == uri,
                _ => false,
            };
            if !same_link {
                if vt.link.is_some() {
                    out.push_str(OSC8_CLOSE_SEQUENCE);
                }
                if let Some(link) = &link {
                    // vte splits OSC 8 parameters on ';', then splits fields
                    // in the params value on ':'. Neither delimiter can occur
                    // in an id by the time it reaches a terminal cell.
                    // Preserve every id because explicit child ids can share
                    // alacritty's generated-id suffix.
                    push_fmt(
                        out,
                        format_args!("\x1b]8;id={};{}\x1b\\", link.id(), link.uri()),
                    );
                    vt.link = Some((link.id().to_owned(), link.uri().to_owned()));
                } else {
                    vt.link = None;
                }
            }
        }
        push_cell_text(out, cell);
    }
}

fn is_blank_text(cell: &Cell) -> bool {
    matches!(cell_text(cell), CellText::Empty)
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
    match cell_text(cell) {
        CellText::Empty => out.push(' '),
        CellText::Grapheme { base, zerowidth } => {
            out.push(base);
            out.extend(zerowidth.iter().copied());
        }
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
    let underline = UnderlineStyle::from_flags(flags);
    if let Some(sgr) = UNDERLINE_SGR
        .iter()
        .find_map(|(style, sgr)| (*style == underline).then_some(*sgr))
    {
        out.push_str(sgr);
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
            if index >= super::color::NAMED_COLOR_COUNT {
                // Foreground/Background and the renderer-only dim/bright
                // variants all mean "default" here.
                return;
            }
            match slot {
                ColorSlot::Foreground if index < 8 => {
                    push_fmt(out, format_args!(";{}", 30 + index));
                }
                ColorSlot::Foreground => {
                    push_fmt(out, format_args!(";{}", 90 + index - 8));
                }
                ColorSlot::Background if index < 8 => {
                    push_fmt(out, format_args!(";{}", 40 + index));
                }
                ColorSlot::Background => {
                    push_fmt(out, format_args!(";{}", 100 + index - 8));
                }
                ColorSlot::Underline => {
                    push_fmt(out, format_args!(";58;5;{index}"));
                }
            }
        }
        Color::Indexed(index) => {
            let prefix = match slot {
                ColorSlot::Foreground => 38,
                ColorSlot::Background => 48,
                ColorSlot::Underline => 58,
            };
            push_fmt(out, format_args!(";{prefix};5;{index}"));
        }
        Color::Spec(rgb) => {
            let prefix = match slot {
                ColorSlot::Foreground => 38,
                ColorSlot::Background => 48,
                ColorSlot::Underline => 58,
            };
            push_fmt(
                out,
                format_args!(";{prefix};2;{};{};{}", rgb.r, rgb.g, rgb.b),
            );
        }
    }
}

/// Appends formatted text to `out`. `fmt::Write for String` only reports an
/// error a `Display` impl raises, and the arguments here are integers and
/// `&str`, which never do, so there is no failure to act on.
fn push_fmt(out: &mut String, args: std::fmt::Arguments<'_>) {
    out.write_fmt(args).ok();
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
