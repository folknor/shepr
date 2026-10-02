//! Frame blitting - renders FrameData to the terminal using diff-based updates.
//!
//! The blitting strategy:
//! 1. On the first frame, write the entire buffer (full redraw).
//! 2. On subsequent frames, diff against the last frame and only write
//!    the cells that changed.
//! 3. Wrap each frame in synchronized output so terminals that support it do
//!    not expose intermediate cursor positions while the frame is painted.
//! 4. Before writing any cells, hide the cursor to avoid stray cursor
//!    artifacts on terminals that render the hardware cursor at intermediate
//!    `CUP` positions during the frame stream.
//! 5. After writing all changed cells, restore the final cursor visibility
//!    and position from `frame.cursor`.
//! 6. Repeat the final cursor anchor after ending synchronized output so
//!    external IMEs, which may not observe cursor moves made inside a
//!    synchronized block, can place candidate windows at the real input
//!    position.
//!
//! Escape sequences used:
//! - `CSI H` (CUP) - move cursor to (row, col)
//! - `CSI m` (SGR) - set graphic rendition (colors, bold, etc.)
//! - `CSI ? 2026 h/l` - begin/end synchronized output
//! - `CSI ? 25 h/l` - show/hide cursor
//! - `CSI Ps SP q` - DECSCUSR cursor shape
//! - `CSI 2 J` - clear screen before the first full redraw
//! - `OSC 8 ; ; <uri> ST` - hyperlinks
//!
//! Clipboard (OSC 52) output is not written here; it travels as its own
//! `ServerMessage::Clipboard`.
//!
//! The goal is minimal output: skip unchanged cells, batch adjacent changes,
//! and minimize cursor movement.

use std::cmp;
use std::io::{self, Write};

use unicode_width::UnicodeWidthStr;

use shepr_protocol::{
    CellData, CursorState, FrameData, GridCellWidth, PaneSurfacePatchRow, WireColor, WireStyle,
    WireStyleFlags,
};
use shepr_vt::UnderlineStyle;

/// Bytes produced by a [`BlitEncoder`] for one terminal frame.
pub struct EncodedBlit {
    /// Terminal escape bytes ready to write to the host terminal.
    pub bytes: Vec<u8>,
    next_last_visible_cursor: Option<(u16, u16)>,
    next_last_cursor_shape: u8,
}

/// Stateful encoder that diffs semantic frames into terminal ANSI bytes.
#[derive(Default)]
pub struct BlitEncoder {
    last_frame: Option<FrameData>,
    last_visible_cursor: Option<(u16, u16)>,
    last_cursor_shape: u8,
}

impl BlitEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn encode(&self, frame: &FrameData, repaint: bool) -> EncodedBlit {
        self.encode_inner(frame, repaint, false)
    }

    pub fn encode_with_suppressed_visible_cursor(
        &self,
        frame: &FrameData,
        repaint: bool,
    ) -> EncodedBlit {
        self.encode_inner(frame, repaint, true)
    }

    fn encode_inner(
        &self,
        frame: &FrameData,
        repaint: bool,
        suppress_visible_cursor: bool,
    ) -> EncodedBlit {
        if !frame_cell_count_matches(frame) {
            tracing::warn!(
                event = "blit.invalid_frame",
                width = frame.width,
                height = frame.height,
                cells = frame.cells.len(),
                "refusing to encode frame with a mismatched cell count"
            );
            return EncodedBlit {
                bytes: Vec::new(),
                next_last_visible_cursor: self.last_visible_cursor,
                next_last_cursor_shape: self.last_cursor_shape,
            };
        }
        let previous_frame = self.last_frame.as_ref();
        let prev = if repaint { None } else { previous_frame };
        let clear_before_full_redraw = previous_frame.is_none();
        let mut bytes = Vec::new();
        let mut next_last_visible_cursor = self.last_visible_cursor;
        let mut next_last_cursor_shape = self.last_cursor_shape;
        // Vec writes cannot fail. The helper also guards against a malformed
        // previous frame; commits store only validated frames, so that check
        // is defensive unless the encoder's state invariant changes.
        if let Err(error) = blit_frame_to_with_cursor_memory_and_clear_policy(
            &mut bytes,
            frame,
            prev,
            &mut next_last_visible_cursor,
            &mut next_last_cursor_shape,
            clear_before_full_redraw,
            suppress_visible_cursor,
        ) {
            tracing::warn!(
                event = "blit.frame_encode_failed",
                error = %error,
                "could not encode terminal frame"
            );
            return EncodedBlit {
                bytes: Vec::new(),
                next_last_visible_cursor: self.last_visible_cursor,
                next_last_cursor_shape: self.last_cursor_shape,
            };
        }
        EncodedBlit {
            bytes,
            next_last_visible_cursor,
            next_last_cursor_shape,
        }
    }

    pub fn commit(&mut self, frame: FrameData, encoded: &EncodedBlit) {
        if !frame_cell_count_matches(&frame) {
            return;
        }
        self.last_visible_cursor = encoded.next_last_visible_cursor;
        self.last_cursor_shape = encoded.next_last_cursor_shape;
        self.last_frame = Some(frame);
    }

    pub fn is_current(&self, frame: &FrameData) -> bool {
        self.last_frame.as_ref() == Some(frame)
    }

    pub fn encode_patch(
        &self,
        rows: &[PaneSurfacePatchRow],
        cursor: Option<CursorState>,
        suppress_visible_cursor: bool,
    ) -> Option<EncodedBlit> {
        let frame = self.last_frame.as_ref()?;
        if !frame_cell_count_matches(frame) || !patch_rows_fit(frame, rows) {
            return None;
        }
        // Metadata revisions need no terminal output. Keep visible cursors on
        // the normal path because their suppression policy can change.
        if rows.is_empty()
            && cursor == frame.cursor
            && cursor.as_ref().is_none_or(|cursor| !cursor.visible)
        {
            return Some(EncodedBlit {
                bytes: Vec::new(),
                next_last_visible_cursor: self.last_visible_cursor,
                next_last_cursor_shape: self.last_cursor_shape,
            });
        }
        let mut bytes = Vec::new();
        let mut next_last_visible_cursor = self.last_visible_cursor;
        let mut next_last_cursor_shape = self.last_cursor_shape;
        // The sink is a Vec<u8>, whose io::Write impl never returns an error,
        // so there is no failure to act on here.
        drop(blit_patch_to(
            &mut bytes,
            frame,
            rows,
            cursor,
            &mut next_last_visible_cursor,
            &mut next_last_cursor_shape,
            suppress_visible_cursor,
        ));
        Some(EncodedBlit {
            bytes,
            next_last_visible_cursor,
            next_last_cursor_shape,
        })
    }

    pub fn patch_rows_with_drawn_cursor(
        &self,
        rows: &[PaneSurfacePatchRow],
        cursor: Option<&CursorState>,
    ) -> Option<Vec<PaneSurfacePatchRow>> {
        let frame = self.last_frame.as_ref()?;
        let mut rows = rows.to_vec();
        let previous = frame
            .cursor
            .as_ref()
            .filter(|cursor| cursor.visible)
            .map(|cursor| clamp_cursor_position(frame, cursor.x, cursor.y));
        let next = cursor
            .filter(|cursor| cursor.visible)
            .map(|cursor| clamp_cursor_position(frame, cursor.x, cursor.y));

        if let Some((x, y)) = previous.filter(|position| Some(*position) != next)
            && patch_cell_mut(&mut rows, x, y).is_none()
        {
            let mut cell = frame.cells.get(frame_cell_index(frame, x, y)?)?.clone();
            cell.style.flags.toggle(WireStyleFlags::REVERSED);
            rows.push(PaneSurfacePatchRow {
                x,
                y,
                cells: vec![cell],
            });
        }
        if let Some((x, y)) = next {
            if let Some(cell) = patch_cell_mut(&mut rows, x, y) {
                cell.style.flags.toggle(WireStyleFlags::REVERSED);
            } else if previous != next {
                let mut cell = frame.cells.get(frame_cell_index(frame, x, y)?)?.clone();
                cell.style.flags.toggle(WireStyleFlags::REVERSED);
                rows.push(PaneSurfacePatchRow {
                    x,
                    y,
                    cells: vec![cell],
                });
            }
        }
        // Cursor cells were appended; restore the row-major order spans require.
        shepr_protocol::sort_patch_rows(&mut rows);
        Some(rows)
    }

    pub fn commit_patch(
        &mut self,
        rows: &[PaneSurfacePatchRow],
        cursor: Option<CursorState>,
        encoded: &EncodedBlit,
    ) -> bool {
        let Some(frame) = self.last_frame.as_mut() else {
            return false;
        };
        // The client calls this only after encode_patch accepts the same rows
        // and after their encoded bytes are written successfully; the check
        // keeps a misuse from wrapping a span into the next row.
        if shepr_protocol::validate_patch_rows(frame.width, frame.height, rows).is_err() {
            return false;
        }
        for row in rows {
            let start = usize::from(row.y) * usize::from(frame.width) + usize::from(row.x);
            let end = start + row.cells.len();
            let Some(target) = frame.cells.get_mut(start..end) else {
                return false;
            };
            target.clone_from_slice(&row.cells);
        }
        frame.cursor = cursor;
        self.last_visible_cursor = encoded.next_last_visible_cursor;
        self.last_cursor_shape = encoded.next_last_cursor_shape;
        true
    }
}

pub fn frame_with_drawn_cursor(mut frame: FrameData) -> FrameData {
    if let Some(cursor) = frame.cursor.as_ref().filter(|cursor| cursor.visible) {
        let (x, y) = clamp_cursor_position(&frame, cursor.x, cursor.y);
        let idx = (y as usize)
            .saturating_mul(frame.width as usize)
            .saturating_add(x as usize);
        if let Some(cell) = frame.cells.get_mut(idx) {
            cell.style.flags.toggle(WireStyleFlags::REVERSED);
        }
    }
    frame
}

// ---------------------------------------------------------------------------
// Color → escape sequence
// ---------------------------------------------------------------------------

/// Returns a foreground SGR fragment for a typed wire color.
fn color_to_sgr_fg(color: WireColor) -> String {
    match color {
        WireColor::Reset => "39".to_owned(),
        WireColor::Black => "30".to_owned(),
        WireColor::Red => "31".to_owned(),
        WireColor::Green => "32".to_owned(),
        WireColor::Yellow => "33".to_owned(),
        WireColor::Blue => "34".to_owned(),
        WireColor::Magenta => "35".to_owned(),
        WireColor::Cyan => "36".to_owned(),
        WireColor::Gray => "37".to_owned(),
        WireColor::DarkGray => "90".to_owned(),
        WireColor::LightRed => "91".to_owned(),
        WireColor::LightGreen => "92".to_owned(),
        WireColor::LightYellow => "93".to_owned(),
        WireColor::LightBlue => "94".to_owned(),
        WireColor::LightMagenta => "95".to_owned(),
        WireColor::LightCyan => "96".to_owned(),
        WireColor::White => "97".to_owned(),
        WireColor::Indexed(index) => format!("38;5;{index}"),
        WireColor::Rgb(red, green, blue) => format!("38;2;{red};{green};{blue}"),
    }
}

/// Returns a background SGR fragment for a typed wire color.
fn color_to_sgr_bg(color: WireColor) -> String {
    match color {
        WireColor::Reset => "49".to_owned(),
        WireColor::Black => "40".to_owned(),
        WireColor::Red => "41".to_owned(),
        WireColor::Green => "42".to_owned(),
        WireColor::Yellow => "43".to_owned(),
        WireColor::Blue => "44".to_owned(),
        WireColor::Magenta => "45".to_owned(),
        WireColor::Cyan => "46".to_owned(),
        WireColor::Gray => "47".to_owned(),
        WireColor::DarkGray => "100".to_owned(),
        WireColor::LightRed => "101".to_owned(),
        WireColor::LightGreen => "102".to_owned(),
        WireColor::LightYellow => "103".to_owned(),
        WireColor::LightBlue => "104".to_owned(),
        WireColor::LightMagenta => "105".to_owned(),
        WireColor::LightCyan => "106".to_owned(),
        WireColor::White => "107".to_owned(),
        WireColor::Indexed(index) => format!("48;5;{index}"),
        WireColor::Rgb(red, green, blue) => format!("48;2;{red};{green};{blue}"),
    }
}

// ---------------------------------------------------------------------------
// Modifier → SGR
// ---------------------------------------------------------------------------

/// Converts semantic style flags to SGR escape sequence fragments.
fn style_to_sgr_parts(style: WireStyle) -> Vec<&'static str> {
    let mut parts = Vec::new();

    if style.flags.contains(WireStyleFlags::BOLD) {
        parts.push("1");
    }
    if style.flags.contains(WireStyleFlags::DIM) {
        parts.push("2");
    }
    if style.flags.contains(WireStyleFlags::ITALIC) {
        parts.push("3");
    }
    match style.underline {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => parts.push("4"),
        UnderlineStyle::Double => parts.push("4:2"),
        UnderlineStyle::Curly => parts.push("4:3"),
        UnderlineStyle::Dotted => parts.push("4:4"),
        UnderlineStyle::Dashed => parts.push("4:5"),
    }
    if style.flags.contains(WireStyleFlags::SLOW_BLINK) {
        parts.push("5");
    }
    if style.flags.contains(WireStyleFlags::RAPID_BLINK) {
        parts.push("6");
    }
    if style.flags.contains(WireStyleFlags::REVERSED) {
        parts.push("7");
    }
    if style.flags.contains(WireStyleFlags::HIDDEN) {
        parts.push("8");
    }
    if style.flags.contains(WireStyleFlags::CROSSED_OUT) {
        parts.push("9");
    }

    parts
}

/// Builds a complete SGR escape sequence for a cell's style.
fn build_sgr(fg: WireColor, bg: WireColor, style: WireStyle) -> String {
    let mut parts = vec!["0".to_owned()];
    parts.extend(style_to_sgr_parts(style).into_iter().map(str::to_owned));
    parts.push(color_to_sgr_fg(fg));
    parts.push(color_to_sgr_bg(bg));
    format!("\x1b[{}m", parts.join(";"))
}

// ---------------------------------------------------------------------------
// Cell comparison
// ---------------------------------------------------------------------------

fn frame_cell_index(frame: &FrameData, x: u16, y: u16) -> Option<usize> {
    (x < frame.width && y < frame.height)
        .then(|| usize::from(y) * usize::from(frame.width) + usize::from(x))
}

/// `FrameData` is a mutable protocol struct, so check its grid at the terminal output boundary.
fn frame_cell_count_matches(frame: &FrameData) -> bool {
    usize::from(frame.width).checked_mul(usize::from(frame.height)) == Some(frame.cells.len())
}

fn patch_cell_mut(rows: &mut [PaneSurfacePatchRow], x: u16, y: u16) -> Option<&mut CellData> {
    rows.iter_mut().rev().find_map(|row| {
        if row.y != y || x < row.x {
            return None;
        }
        row.cells.get_mut(usize::from(x - row.x))
    })
}

fn patch_cell_at(rows: &[PaneSurfacePatchRow], x: u16, y: u16) -> Option<&CellData> {
    rows.iter().find_map(|row| {
        if row.y != y || x < row.x {
            return None;
        }
        row.cells.get(usize::from(x - row.x))
    })
}

/// Whether `rows` may be drawn over `frame`: they obey the shared span rule and
/// neither the new cells nor the cells they replace carry a hyperlink.
fn patch_rows_fit(frame: &FrameData, rows: &[PaneSurfacePatchRow]) -> bool {
    shepr_protocol::validate_patch_rows(frame.width, frame.height, rows).is_ok()
        && rows
            .iter()
            .all(|row| patch_row_has_no_hyperlinks(frame, row))
}

fn patch_row_has_no_hyperlinks(frame: &FrameData, row: &PaneSurfacePatchRow) -> bool {
    if row.cells.iter().any(|cell| cell.hyperlink.is_some()) {
        return false;
    }
    let start = usize::from(row.y) * usize::from(frame.width) + usize::from(row.x);
    let end = start + row.cells.len();
    frame
        .cells
        .get(start..end)
        .is_some_and(|cells| cells.iter().all(|cell| cell.hyperlink.is_none()))
}

fn blit_patch_to(
    mut writer: impl Write,
    frame: &FrameData,
    rows: &[PaneSurfacePatchRow],
    cursor: Option<CursorState>,
    last_visible_cursor: &mut Option<(u16, u16)>,
    last_cursor_shape: &mut u8,
    suppress_visible_cursor: bool,
) -> io::Result<()> {
    writer.write_all(b"\x1b[?2026h\x1b[?25l\x1b]8;;\x1b\\")?;
    let mut state = CellWriterState::default();
    let source = CellPaintSource {
        frame,
        current_hyperlinks: &[],
        previous: Some(PreviousFrame {
            frame,
            sanitized_hyperlinks: &[],
        }),
        patch_rows: Some(rows),
    };
    for row in rows {
        paint_row_cells(&mut writer, &source, row.y, row.x, &row.cells, &mut state)?;
    }
    close_hyperlink(&mut writer, &mut state.active_hyperlink)?;
    if !state.last_sgr.is_empty() {
        writer.write_all(b"\x1b[0m")?;
    }

    let cursor_frame = FrameData {
        cells: Vec::new(),
        width: frame.width,
        height: frame.height,
        cursor,
        hyperlinks: Vec::new(),
    };
    let mut host_cursor = resolve_host_cursor_state(&cursor_frame, last_visible_cursor);
    if suppress_visible_cursor && host_cursor.visible {
        host_cursor.visible = false;
    }
    write_host_cursor_state(&mut writer, host_cursor, last_cursor_shape)?;
    writer.write_all(b"\x1b[?2026l")?;
    write_ime_anchor_cursor_state(&mut writer, host_cursor)?;
    writer.flush()
}

fn blit_frame_to_with_cursor_memory_and_clear_policy(
    mut writer: impl Write,
    frame: &FrameData,
    prev: Option<&FrameData>,
    last_visible_cursor: &mut Option<(u16, u16)>,
    last_cursor_shape: &mut u8,
    clear_before_full_redraw: bool,
    suppress_visible_cursor: bool,
) -> io::Result<()> {
    if !frame_cell_count_matches(frame)
        || prev.is_some_and(|previous| {
            previous.width == frame.width
                && previous.height == frame.height
                && !frame_cell_count_matches(previous)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame cell count does not match its dimensions",
        ));
    }
    // On first frame or size change, do a full redraw; otherwise diff against
    // the previous frame.
    let diff_base = prev.filter(|p| p.width == frame.width && p.height == frame.height);

    // Ask terminals that support synchronized output to apply the whole frame
    // atomically. This keeps IMEs and cursor trackers from observing the
    // intermediate CUP positions used while painting changed cells.
    writer.write_all(b"\x1b[?2026h")?;

    // Hide cursor before any cell writes to avoid stray cursor artifacts
    // on terminals that render the hardware cursor at intermediate CUP positions.
    writer.write_all(b"\x1b[?25l")?;

    // Start each frame from a known OSC 8 state. If a previous write was
    // interrupted or the outer terminal had an active hyperlink, unlinked cells
    // must not inherit it.
    writer.write_all(b"\x1b]8;;\x1b\\")?;

    if diff_base.is_none() && clear_before_full_redraw {
        writer.write_all(b"\x1b[2J")?;
    }
    write_frame_cells(&mut writer, frame, diff_base)?;

    // Position the cursor while it is still hidden, then restore visibility.
    // Showing before moving makes slow terminals and IMEs briefly observe the
    // cursor at the last painted cell, which can be an animated sidebar/status
    // cell rather than the focused pane's input position. When the focused pane
    // hides its cursor, still park the host cursor intentionally so IMEs do not
    // anchor to whichever cell happened to be painted last.
    let mut host_cursor = resolve_host_cursor_state(frame, last_visible_cursor);
    if suppress_visible_cursor && host_cursor.visible {
        host_cursor.visible = false;
    }
    write_host_cursor_state(&mut writer, host_cursor, last_cursor_shape)?;

    // End the synchronized output block immediately after the final cursor
    // state is emitted so supporting terminals can present the frame atomically.
    writer.write_all(b"\x1b[?2026l")?;

    // Some native IMEs track candidate-window placement from normal terminal
    // cursor updates and may not observe cursor moves emitted inside synchronized
    // output. Re-emit only the resolved final cursor anchor after the sync block.
    write_ime_anchor_cursor_state(&mut writer, host_cursor)?;
    writer.flush()
}

/// Terminal column width of text under Ratatui's grapheme width rule, including the
/// halfwidth voiced marks terminals display in their own cells.
pub fn text_width(text: &str) -> usize {
    text.width().saturating_add(
        text.chars()
            .filter(|character| matches!(character, '\u{ff9e}' | '\u{ff9f}'))
            .count(),
    )
}

/// Grapheme width of a cell's symbol. Client composition uses [`text_width`]
/// directly for chrome and the explicit grid width for pane cells.
pub fn cell_width(cell: &CellData) -> usize {
    symbol_width(&cell.symbol)
}

fn cell_grid_width(cell: &CellData) -> usize {
    match cell.grid_width {
        GridCellWidth::Grapheme => text_width(&cell.symbol),
        GridCellWidth::One => 1,
        GridCellWidth::Two => 2,
    }
}

/// Terminal column width of `symbol`; see [`text_width`] and [`cell_width`].
pub fn symbol_width(symbol: &str) -> usize {
    text_width(symbol)
}

#[derive(Clone, Copy)]
struct HostCursorState {
    position: (u16, u16),
    visible: bool,
    /// DECSCUSR parameter (0-6). 0 means terminal default.
    shape: u8,
}

fn resolve_host_cursor_state(
    frame: &FrameData,
    last_visible_cursor: &mut Option<(u16, u16)>,
) -> HostCursorState {
    if let Some(cursor) = &frame.cursor {
        if cursor.visible {
            let position = clamp_cursor_position(frame, cursor.x, cursor.y);
            *last_visible_cursor = Some(position);
            return HostCursorState {
                position,
                visible: true,
                shape: cursor.shape as u8,
            };
        }

        let position = clamp_cursor_position(frame, cursor.x, cursor.y);
        return HostCursorState {
            position,
            visible: false,
            shape: cursor.shape as u8,
        };
    }

    let position = (*last_visible_cursor).map_or_else(
        || default_hidden_cursor_position(frame),
        |(x, y)| clamp_cursor_position(frame, x, y),
    );
    HostCursorState {
        position,
        visible: false,
        shape: 0,
    }
}

fn default_hidden_cursor_position(frame: &FrameData) -> (u16, u16) {
    (
        frame.width.saturating_sub(1),
        frame.height.saturating_sub(1),
    )
}

fn clamp_cursor_position(frame: &FrameData, x: u16, y: u16) -> (u16, u16) {
    (
        x.min(frame.width.saturating_sub(1)),
        y.min(frame.height.saturating_sub(1)),
    )
}

fn write_cursor_position(writer: &mut impl Write, (x, y): (u16, u16)) -> io::Result<()> {
    // CUP: move cursor to (row+1, col+1) - 1-based.
    write!(writer, "\x1b[{};{}H", y + 1, x + 1)
}

fn write_host_cursor_state(
    writer: &mut impl Write,
    cursor: HostCursorState,
    last_shape: &mut u8,
) -> io::Result<()> {
    write_cursor_position(writer, cursor.position)?;
    if cursor.shape != *last_shape {
        write!(writer, "\x1b[{} q", cursor.shape)?;
        *last_shape = cursor.shape;
    }
    if cursor.visible {
        // Show cursor only after it is already at the final position.
        writer.write_all(b"\x1b[?25h")
    } else {
        writer.write_all(b"\x1b[?25l")
    }
}

fn write_ime_anchor_cursor_state(
    writer: &mut impl Write,
    cursor: HostCursorState,
) -> io::Result<()> {
    write_cursor_position(writer, cursor.position)?;
    if cursor.visible {
        writer.write_all(b"\x1b[?25h")
    } else {
        writer.write_all(b"\x1b[?25l")
    }
}

fn cell_hyperlink_uri<'a>(frame: &'a FrameData, cell: &CellData) -> Option<&'a str> {
    let index = cell.hyperlink? as usize;
    frame.hyperlinks.get(index).map(String::as_str)
}

fn sanitized_hyperlink_uri(uri: &str) -> Option<String> {
    let sanitized: String = uri
        .chars()
        .filter(|ch| *ch != '\x1b' && *ch != '\x07' && !ch.is_control())
        .collect();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn sanitized_frame_hyperlinks(frame: &FrameData) -> Vec<Option<String>> {
    frame
        .hyperlinks
        .iter()
        .map(|uri| sanitized_hyperlink_uri(uri))
        .collect()
}

fn sanitized_cell_hyperlink_uri<'a>(
    sanitized_hyperlinks: &'a [Option<String>],
    cell: &CellData,
) -> Option<&'a str> {
    let index = cell.hyperlink? as usize;
    sanitized_hyperlinks.get(index)?.as_deref()
}

fn write_hyperlink_if_changed(
    writer: &mut impl Write,
    active: &mut Option<String>,
    requested: Option<&str>,
) -> io::Result<()> {
    let requested = requested.and_then(sanitized_hyperlink_uri);
    if active.as_deref() == requested.as_deref() {
        return Ok(());
    }

    if active.is_some() {
        writer.write_all(b"\x1b]8;;\x1b\\")?;
    }
    *active = requested;
    if let Some(uri) = active.as_deref() {
        write!(writer, "\x1b]8;;{uri}\x1b\\")?;
    }
    Ok(())
}

fn close_hyperlink(writer: &mut impl Write, active: &mut Option<String>) -> io::Result<()> {
    if active.take().is_some() {
        writer.write_all(b"\x1b]8;;\x1b\\")?;
    }
    Ok(())
}

fn write_cell(
    writer: &mut impl Write,
    cursor_position: Option<(u16, u16)>,
    cell: &CellData,
    last_sgr: &mut String,
    last_style: &mut Option<(WireColor, WireColor, WireStyle)>,
    active_hyperlink: &mut Option<String>,
    frame: &FrameData,
) -> io::Result<()> {
    if cell.skip {
        return Ok(());
    }

    if let Some(position) = cursor_position {
        write_cursor_position(writer, position)?;
    }

    let style = (cell.fg, cell.bg, cell.style);
    if *last_style != Some(style) {
        let sgr = build_sgr(cell.fg, cell.bg, cell.style);
        if sgr != *last_sgr {
            writer.write_all(sgr.as_bytes())?;
            *last_sgr = sgr;
        }
        *last_style = Some(style);
    }

    write_hyperlink_if_changed(writer, active_hyperlink, cell_hyperlink_uri(frame, cell))?;
    writer.write_all(cell.symbol.as_bytes())
}

/// Checks whether two cells have the same rendered content, resolving links by URI.
fn cells_visually_equal(
    sanitized_hyperlinks: &[Option<String>],
    cell: &CellData,
    prev_sanitized_hyperlinks: &[Option<String>],
    prev_cell: &CellData,
) -> bool {
    cell.symbol == prev_cell.symbol
        && cell.grid_width == prev_cell.grid_width
        && cell.fg == prev_cell.fg
        && cell.bg == prev_cell.bg
        && cell.style == prev_cell.style
        && sanitized_cell_hyperlink_uri(sanitized_hyperlinks, cell)
            == sanitized_cell_hyperlink_uri(prev_sanitized_hyperlinks, prev_cell)
    // Skip flag is only for ratatui internal use, not visual.
}

#[derive(Default)]
struct CellWriterState {
    last_sgr: String,
    last_style: Option<(WireColor, WireColor, WireStyle)>,
    active_hyperlink: Option<String>,
}

#[derive(Clone, Copy)]
struct PreviousFrame<'a> {
    frame: &'a FrameData,
    sanitized_hyperlinks: &'a [Option<String>],
}

#[derive(Clone, Copy)]
struct CellPaintSource<'a> {
    frame: &'a FrameData,
    current_hyperlinks: &'a [Option<String>],
    previous: Option<PreviousFrame<'a>>,
    patch_rows: Option<&'a [PaneSurfacePatchRow]>,
}

/// Paints full frames, frame diffs, and retained patches through one cell walker.
fn write_frame_cells(
    writer: &mut impl Write,
    frame: &FrameData,
    previous: Option<&FrameData>,
) -> io::Result<()> {
    let current_hyperlinks = previous.map(|_| sanitized_frame_hyperlinks(frame));
    let previous_hyperlinks = previous.map(sanitized_frame_hyperlinks);
    let mut state = CellWriterState::default();
    let source = CellPaintSource {
        frame,
        current_hyperlinks: current_hyperlinks.as_deref().unwrap_or(&[]),
        previous: previous
            .zip(previous_hyperlinks.as_deref())
            .map(|(frame, links)| PreviousFrame {
                frame,
                sanitized_hyperlinks: links,
            }),
        patch_rows: None,
    };
    for row in 0..frame.height {
        let start = usize::from(row) * usize::from(frame.width);
        let end = start + usize::from(frame.width);
        paint_row_cells(
            writer,
            &source,
            row,
            0,
            &frame.cells[start..end],
            &mut state,
        )?;
    }

    close_hyperlink(writer, &mut state.active_hyperlink)?;

    // Full paints establish a known style even for an empty frame. Diffs reset
    // only when the painter emitted an SGR sequence.
    if previous.is_none() || !state.last_sgr.is_empty() {
        writer.write_all(b"\x1b[0m")?;
    }
    Ok(())
}

/// Paints a row slice using its previous cell at each screen position.
///
/// Frame paints cover the whole row. Patches cover only their supplied spans,
/// so a newly wide grapheme also restores an omitted successor from the frame.
fn paint_row_cells(
    writer: &mut impl Write,
    source: &CellPaintSource<'_>,
    row: u16,
    start_col: u16,
    cells: &[CellData],
    state: &mut CellWriterState,
) -> io::Result<()> {
    let mut invalidated = 0usize;
    let mut to_skip = 0usize;
    let mut next_inline_col = None;
    let full_paint = source.previous.is_none();
    let previous_hyperlinks = source
        .previous
        .map_or(&[][..], |previous| previous.sanitized_hyperlinks);

    for (offset, cell) in cells.iter().enumerate() {
        // The frame or patch validator guarantees that this position fits.
        let col = start_col + u16::try_from(offset).unwrap_or(u16::MAX);
        if full_paint && to_skip > 0 {
            to_skip -= 1;
            continue;
        }
        if full_paint && cell.skip {
            next_inline_col = None;
            continue;
        }
        let previous_cell = source.previous.and_then(|previous| {
            frame_cell_index(previous.frame, col, row)
                .and_then(|index| previous.frame.cells.get(index))
        });
        let same = previous_cell.is_some_and(|previous_cell| {
            cells_visually_equal(
                source.current_hyperlinks,
                cell,
                previous_hyperlinks,
                previous_cell,
            )
        });
        let grid_width = cell_grid_width(cell);
        let previous_width = previous_cell.map_or(0, cell_width);
        let affected_width = cmp::max(cell_width(cell), previous_width);

        if !cell.skip && (!same || invalidated > 0) && to_skip == 0 {
            let cursor_position = (next_inline_col != Some(col)
                || (!full_paint && invalidated > 0))
                .then_some((col, row));
            write_cell(
                writer,
                cursor_position,
                cell,
                &mut state.last_sgr,
                &mut state.last_style,
                &mut state.active_hyperlink,
                source.frame,
            )?;

            if let Some(patch_rows) = source.patch_rows
                && affected_width > grid_width
                && let Some(next_col) = col
                    .checked_add(1)
                    .filter(|next_col| *next_col < source.frame.width)
                && patch_cell_at(patch_rows, next_col, row).is_none()
                && let Some(next_index) = frame_cell_index(source.frame, next_col, row)
                && let Some(next_cell) = source.frame.cells.get(next_index)
            {
                // A wide grapheme can cover the next host column beyond its pane grid cell.
                write_cell(
                    writer,
                    Some((next_col, row)),
                    next_cell,
                    &mut state.last_sgr,
                    &mut state.last_style,
                    &mut state.active_hyperlink,
                    source.frame,
                )?;
            }

            next_inline_col =
                (cell.symbol.is_ascii() && grid_width == 1).then_some(col.saturating_add(1));
        }

        to_skip = grid_width.saturating_sub(1);
        invalidated = cmp::max(affected_width, invalidated).saturating_sub(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Blitting
// ---------------------------------------------------------------------------

/// Output-frame placement width comes from the pane's grid or, for chrome,
/// from Ratatui's grapheme convention.
#[cfg(test)]
fn frame_cell_width(frame: &FrameData, col: u16, row: u16) -> usize {
    frame_cell_index(frame, col, row)
        .and_then(|index| frame.cells.get(index))
        .map_or(0, cell_grid_width)
}

/// Blits a frame to a writer, diffing against the previous frame.
#[cfg(test)]
fn blit_frame_to(writer: impl Write, frame: &FrameData, prev: Option<&FrameData>) {
    let mut last_visible_cursor = None;
    let mut last_cursor_shape = 0;
    blit_frame_to_with_cursor_memory(
        writer,
        frame,
        prev,
        &mut last_visible_cursor,
        &mut last_cursor_shape,
        false,
    );
}

#[cfg(test)]
fn blit_frame_to_with_cursor_memory(
    writer: impl Write,
    frame: &FrameData,
    prev: Option<&FrameData>,
    last_visible_cursor: &mut Option<(u16, u16)>,
    last_cursor_shape: &mut u8,
    suppress_visible_cursor: bool,
) {
    blit_frame_to_with_cursor_memory_and_clear_policy(
        writer,
        frame,
        prev,
        last_visible_cursor,
        last_cursor_shape,
        true,
        suppress_visible_cursor,
    )
    .expect("tests blit into a Vec, which cannot fail to write");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{CellData, CursorState};

    const WIDE_GRAPHEME: &str = "\u{1F4A1}";
    const HALFWIDTH_VOICED_KANA: &str = "ｶ\u{ff9e}";

    fn make_cell(symbol: &str, fg: WireColor, bg: WireColor, style: WireStyle) -> CellData {
        CellData {
            symbol: symbol.to_owned(),
            grid_width: GridCellWidth::Grapheme,
            fg,
            bg,
            style,
            skip: false,
            hyperlink: None,
        }
    }

    fn default_cell(symbol: &str) -> CellData {
        make_cell(
            symbol,
            WireColor::Reset,
            WireColor::Reset,
            WireStyle::default(),
        )
    }

    fn pane_cell(symbol: &str, grid_width: GridCellWidth) -> CellData {
        let mut cell = default_cell(symbol);
        cell.grid_width = grid_width;
        cell
    }

    fn make_skip_cell(symbol: &str) -> CellData {
        let mut cell = default_cell(symbol);
        cell.skip = true;
        cell
    }

    fn make_frame(width: u16, height: u16, cells: Vec<CellData>) -> FrameData {
        FrameData {
            cells,
            width,
            height,
            cursor: None,
            hyperlinks: Vec::new(),
        }
    }

    #[test]
    fn text_width_matches_unicode_graphemes_and_terminal_voiced_marks() {
        assert_eq!(text_width("\u{2764}\u{fe0f}agent"), 7);
        assert_eq!(text_width("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"), 2);
        assert_eq!(text_width("ｶﾞx"), 3);
        assert_eq!(text_width("aﾞ"), 2);
    }

    #[test]
    fn full_diff_and_patch_keep_a_space_after_a_narrow_vs16_pane_cell() {
        let frame = make_frame(
            2,
            1,
            vec![
                pane_cell("\u{26a0}\u{fe0f}", GridCellWidth::One),
                pane_cell(" ", GridCellWidth::One),
            ],
        );
        let mut full = Vec::new();
        blit_frame_to(&mut full, &frame, None);
        let full = String::from_utf8(full).expect("test precondition");
        assert!(full.contains("\u{26a0}\u{fe0f}\x1b[1;2H "));

        let previous = make_frame(
            2,
            1,
            vec![
                pane_cell("A", GridCellWidth::One),
                pane_cell(" ", GridCellWidth::One),
            ],
        );
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&previous, false);
        encoder.commit(previous, &initial);
        let diff = encoder.encode(&frame, false);
        let diff_text = String::from_utf8(diff.bytes.clone()).expect("test precondition");
        assert!(diff_text.contains("\u{26a0}\u{fe0f}\x1b[1;2H "));

        let rows = [PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: vec![pane_cell("\u{26a0}\u{fe0f}", GridCellWidth::One)],
        }];
        let patch = encoder
            .encode_patch(&rows, None, false)
            .expect("single-cell pane patch is valid");
        assert_eq!(patch.bytes, diff.bytes);
    }

    #[test]
    fn narrow_vs16_pane_cell_keeps_one_column_width_at_the_row_edge() {
        let frame = make_frame(
            1,
            1,
            vec![pane_cell("\u{26a0}\u{fe0f}", GridCellWidth::One)],
        );
        assert_eq!(text_width("\u{26a0}\u{fe0f}"), 2);
        assert_eq!(frame_cell_width(&frame, 0, 0), 1);

        let mut full = Vec::new();
        blit_frame_to(&mut full, &frame, None);
        assert!(
            String::from_utf8(full)
                .expect("test precondition")
                .contains("\u{26a0}\u{fe0f}")
        );
    }

    #[test]
    fn pane_wide_cells_use_their_empty_spacer_for_placement() {
        let frame = make_frame(
            3,
            1,
            vec![
                default_cell(WIDE_GRAPHEME),
                default_cell(""),
                default_cell("Z"),
            ],
        );
        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);
        let output = String::from_utf8(output).expect("test precondition");

        assert!(output.contains("\x1b[1;1H"));
        assert!(!output.contains("\x1b[1;2H"));
        assert!(output.contains("\x1b[1;3H"));
    }

    fn linked_cell(symbol: &str, index: u32) -> CellData {
        let mut cell = default_cell(symbol);
        cell.hyperlink = Some(index);
        cell
    }

    #[test]
    fn color_to_sgr_fg_named_colors() {
        assert_eq!(color_to_sgr_fg(WireColor::Reset), "39");
        assert_eq!(color_to_sgr_fg(WireColor::Black), "30");
        assert_eq!(color_to_sgr_fg(WireColor::Red), "31");
        assert_eq!(color_to_sgr_fg(WireColor::White), "97");
    }

    #[test]
    fn color_to_sgr_fg_indexed() {
        assert_eq!(color_to_sgr_fg(WireColor::Indexed(171)), "38;5;171");
    }

    #[test]
    fn color_to_sgr_fg_rgb() {
        assert_eq!(
            color_to_sgr_fg(WireColor::Rgb(255, 128, 64)),
            "38;2;255;128;64"
        );
    }

    #[test]
    fn color_to_sgr_bg_named_colors() {
        assert_eq!(color_to_sgr_bg(WireColor::Reset), "49");
        assert_eq!(color_to_sgr_bg(WireColor::Black), "40");
        assert_eq!(color_to_sgr_bg(WireColor::White), "107");
    }

    #[test]
    fn color_to_sgr_bg_rgb() {
        assert_eq!(
            color_to_sgr_bg(WireColor::Rgb(255, 128, 64)),
            "48;2;255;128;64"
        );
    }

    #[test]
    fn style_to_sgr_parts_bold() {
        let parts = style_to_sgr_parts(WireStyle {
            flags: WireStyleFlags::BOLD,
            ..WireStyle::default()
        });
        assert!(parts.contains(&"1"));
    }

    #[test]
    fn style_to_sgr_parts_italic() {
        let parts = style_to_sgr_parts(WireStyle {
            flags: WireStyleFlags::ITALIC,
            ..WireStyle::default()
        });
        assert!(parts.contains(&"3"));
    }

    #[test]
    fn style_to_sgr_parts_empty() {
        let parts = style_to_sgr_parts(WireStyle::default());
        assert!(parts.is_empty());
    }

    #[test]
    fn build_sgr_produces_valid_sequence() {
        let sgr = build_sgr(
            WireColor::Red,
            WireColor::Black,
            WireStyle {
                flags: WireStyleFlags::BOLD,
                ..WireStyle::default()
            },
        );
        assert!(sgr.starts_with("\x1b["));
        assert!(sgr.ends_with("m"));
        assert!(sgr.contains("0")); // reset existing style first
        assert!(sgr.contains("1")); // bold
        assert!(sgr.contains("31")); // fg red
        assert!(sgr.contains("40")); // bg black
    }

    #[test]
    fn build_sgr_resets_previous_modifiers_when_cell_is_plain() {
        assert_eq!(
            build_sgr(WireColor::Reset, WireColor::Reset, WireStyle::default()),
            "\x1b[0;39;49m"
        );
    }

    #[test]
    fn repeated_and_equivalent_styles_keep_text_and_link_changes() {
        let mut cells = vec![
            default_cell("a"),
            default_cell("b"),
            default_cell("c"),
            make_cell("d", WireColor::Red, WireColor::Reset, WireStyle::default()),
            default_cell("e"),
        ];
        cells[1].hyperlink = Some(0);
        let mut frame = make_frame(5, 1, cells);
        frame.hyperlinks.push("https://example.com".into());
        let mut output = Vec::new();
        write_frame_cells(&mut output, &frame, None).expect("writing into a Vec cannot fail");
        assert_eq!(
            String::from_utf8(output).expect("test precondition"),
            "\x1b[1;1H\x1b[0;39;49ma\x1b]8;;https://example.com\x1b\\b\x1b]8;;\x1b\\c\x1b[0;31;49md\x1b[0;39;49me\x1b[0m"
        );
    }

    #[test]
    fn build_sgr_preserves_curly_underline_style() {
        assert_eq!(
            build_sgr(
                WireColor::Reset,
                WireColor::Reset,
                WireStyle {
                    underline: UnderlineStyle::Curly,
                    ..WireStyle::default()
                }
            ),
            "\x1b[0;4:3;39;49m"
        );
    }

    #[test]
    fn visually_equal_cells_match() {
        let a = make_cell("A", WireColor::Red, WireColor::Black, WireStyle::default());
        let b = make_cell("A", WireColor::Red, WireColor::Black, WireStyle::default());
        assert!(cells_visually_equal(&[], &a, &[], &b));
    }

    #[test]
    fn visually_different_symbols_do_not_match() {
        let a = make_cell("A", WireColor::Red, WireColor::Black, WireStyle::default());
        let b = make_cell("B", WireColor::Red, WireColor::Black, WireStyle::default());
        assert!(!cells_visually_equal(&[], &a, &[], &b));
    }

    #[test]
    fn visually_different_colors_do_not_match() {
        let a = make_cell("A", WireColor::Red, WireColor::Black, WireStyle::default());
        let b = make_cell(
            "A",
            WireColor::Green,
            WireColor::Black,
            WireStyle::default(),
        );
        assert!(!cells_visually_equal(&[], &a, &[], &b));
    }

    #[test]
    fn blit_frame_hides_cursor_before_full_redraw_writes() {
        let frame = make_frame(
            2,
            2,
            vec![
                default_cell("H"),
                default_cell("i"),
                default_cell("!"),
                default_cell(" "),
            ],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(
            output_str.starts_with("\x1b[?2026h\x1b[?25l"),
            "should hide cursor inside synchronized frame painting during full redraw"
        );
    }

    #[test]
    fn blit_frame_hides_cursor_before_diff_writes() {
        let prev = make_frame(
            2,
            2,
            vec![
                default_cell("H"),
                default_cell("i"),
                default_cell("!"),
                default_cell(" "),
            ],
        );

        let curr = make_frame(
            2,
            2,
            vec![
                default_cell("X"), // Changed
                default_cell("i"), // Same
                default_cell("!"), // Same
                default_cell(" "), // Same
            ],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(
            output_str.starts_with("\x1b[?2026h\x1b[?25l"),
            "should hide cursor inside synchronized frame painting during diff"
        );
    }

    #[test]
    fn blit_frame_wraps_frame_in_synchronized_output() {
        let frame = make_frame(1, 1, vec![default_cell("A")]);

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(
            output_str.starts_with("\x1b[?2026h\x1b[?25l"),
            "should begin synchronized output before frame writes"
        );
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output after frame writes");
        assert!(
            sync_end > 0,
            "should end synchronized output after frame writes"
        );
    }

    #[test]
    fn blit_frame_begins_sync_before_hiding_cursor_after_visible_cursor_repeat() {
        let visible = FrameData {
            cells: vec![default_cell("A"); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };
        let mut changed = visible.clone();
        changed.cells[0] = default_cell("B");

        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut first_output = Vec::new();
        blit_frame_to_with_cursor_memory(
            &mut first_output,
            &visible,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let mut second_output = Vec::new();
        blit_frame_to_with_cursor_memory(
            &mut second_output,
            &changed,
            Some(&visible),
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let second_output_str = std::str::from_utf8(&second_output).expect("test precondition");
        assert!(
            second_output_str.starts_with("\x1b[?2026h\x1b[?25l"),
            "next frame should enter synchronized output before hiding the cursor"
        );

        let hide = second_output_str
            .find("\x1b[?25l")
            .expect("second frame should hide cursor before painting");
        let first_paint = second_output_str
            .find("\x1b[1;1H")
            .expect("second frame should paint changed cell");
        assert!(
            hide < first_paint,
            "cursor should still hide before painting"
        );

        first_output.extend_from_slice(&second_output);
        let combined = String::from_utf8(first_output).expect("test precondition");
        assert!(
            combined.contains("\x1b[?2026l\x1b[2;3H\x1b[?25h\x1b[?2026h\x1b[?25l"),
            "post-sync cursor repeat should be followed by a synchronized cursor hide"
        );
    }

    #[test]
    fn blit_frame_can_repeat_final_cursor_state_after_synchronized_output() {
        let frame = FrameData {
            cells: vec![default_cell("A"); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };

        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();
        blit_frame_to_with_cursor_memory(
            &mut output,
            &frame,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let output_str = String::from_utf8(output).expect("test precondition");
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output");
        let trailing_cursor = &output_str[sync_end + "\x1b[?2026l".len()..];
        assert_eq!(
            trailing_cursor, "\x1b[2;3H\x1b[?25h",
            "should expose only the final cursor state after synchronized output"
        );
    }

    #[test]
    fn drawn_cursor_reverses_visible_cursor_cell() {
        let frame = FrameData {
            cells: vec![default_cell("A"); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::SteadyBar,
            }),
            hyperlinks: Vec::new(),
        };
        let drawn = frame_with_drawn_cursor(frame.clone());

        assert!(
            drawn.cells[5]
                .style
                .flags
                .contains(WireStyleFlags::REVERSED)
        );
        assert!(
            !frame.cells[5]
                .style
                .flags
                .contains(WireStyleFlags::REVERSED)
        );

        let encoded = BlitEncoder::new().encode_with_suppressed_visible_cursor(&drawn, false);
        let output_str = String::from_utf8(encoded.bytes).expect("test precondition");

        assert!(
            output_str.contains("\x1b[2;3H\x1b[6 q\x1b[?25l"),
            "drawn cursor mode should park the host cursor hidden at the focused cursor position"
        );
        assert!(
            !output_str.contains("\x1b[?25h"),
            "drawn cursor mode should not show the host cursor"
        );
        assert!(
            output_str.contains("\x1b[0;7;39;49mA"),
            "drawn cursor should be emitted as reverse-video cell content"
        );
    }

    #[test]
    fn drawn_cursor_ignores_hidden_cursor() {
        let frame = FrameData {
            cells: vec![default_cell("A")],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: false,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };

        assert_eq!(frame_with_drawn_cursor(frame.clone()), frame);
    }

    #[test]
    fn blit_frame_emits_cursor_shape_before_visibility_without_touching_ime_anchor() {
        let frame = FrameData {
            cells: vec![default_cell("A")],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::SteadyBar,
            }),
            hyperlinks: Vec::new(),
        };

        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();
        blit_frame_to_with_cursor_memory(
            &mut output,
            &frame,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let output_str = String::from_utf8(output).expect("test precondition");
        let final_cursor = output_str
            .find("\x1b[1;1H\x1b[6 q\x1b[?25h")
            .expect("should set cursor shape before showing cursor");
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output");
        assert!(
            final_cursor < sync_end,
            "shape should be part of the synchronized final cursor state"
        );
        let trailing_cursor = &output_str[sync_end + "\x1b[?2026l".len()..];
        assert_eq!(
            trailing_cursor, "\x1b[1;1H\x1b[?25h",
            "IME anchor update should preserve the existing position/visibility-only contract"
        );
    }

    #[test]
    fn blit_frame_repeats_explicit_hidden_cursor_anchor_after_synchronized_output() {
        let visible = FrameData {
            cells: vec![default_cell("A"); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };
        let hidden = FrameData {
            cells: vec![default_cell("B"); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: false,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };
        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();

        blit_frame_to_with_cursor_memory(
            &mut output,
            &visible,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );
        output.clear();
        blit_frame_to_with_cursor_memory(
            &mut output,
            &hidden,
            Some(&visible),
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let output_str = String::from_utf8(output).expect("test precondition");
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output");
        let trailing_cursor = &output_str[sync_end + "\x1b[?2026l".len()..];
        assert_eq!(
            trailing_cursor, "\x1b[2;3H\x1b[?25l",
            "should repeat the explicit hidden cursor position while preserving visibility"
        );
    }

    #[test]
    fn blit_frame_emits_osc8_for_linked_cells() {
        let mut frame = make_frame(
            3,
            1,
            vec![linked_cell("L", 0), linked_cell("i", 0), default_cell("!")],
        );
        frame.hyperlinks.push("https://example.com".to_owned());

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(output_str.contains("\x1b]8;;https://example.com\x1b\\L"));
        assert!(output_str.contains('i'));
        assert!(output_str.contains("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn blit_frame_sanitizes_hyperlink_uris() {
        let mut frame = make_frame(1, 1, vec![linked_cell("L", 0)]);
        frame
            .hyperlinks
            .push("https://exa\x1b\x07mple.com".to_owned());

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(output_str.contains("\x1b]8;;https://example.com\x1b\\L"));
    }

    #[test]
    fn blit_frame_first_frame_produces_output() {
        let frame = make_frame(
            2,
            2,
            vec![
                default_cell("H"),
                default_cell("i"),
                default_cell("!"),
                default_cell(" "),
            ],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).expect("test precondition");
        // Full redraw should start with clear screen.
        assert!(
            output_str.contains("\x1b[2J"),
            "full redraw should clear screen"
        );
        assert!(
            output_str.contains('H') || output_str.contains('i'),
            "should contain cell content"
        );
    }

    #[test]
    fn blit_frame_diff_only_writes_changed_cells() {
        let prev = make_frame(
            2,
            2,
            vec![
                default_cell("H"),
                default_cell("i"),
                default_cell("!"),
                default_cell(" "),
            ],
        );

        // Only the first cell changed.
        let curr = make_frame(
            2,
            2,
            vec![
                default_cell("X"), // Changed
                default_cell("i"), // Same
                default_cell("!"), // Same
                default_cell(" "), // Same
            ],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));

        let output_str = String::from_utf8(output).expect("test precondition");
        // A diff against the previous frame leaves the screen uncleared.
        assert!(
            !output_str.contains("\x1b[2J"),
            "diff should not clear screen"
        );
        // Should contain the changed cell content.
        assert!(output_str.contains('X'), "should contain changed cell 'X'");
    }

    #[test]
    fn scroll_sized_ascii_shift_batches_changed_cells_by_row() {
        const WIDTH: u16 = 140;
        const HEIGHT: u16 = 50;
        let prev = make_frame(
            WIDTH,
            HEIGHT,
            vec![default_cell("A"); usize::from(WIDTH) * usize::from(HEIGHT)],
        );
        let curr = make_frame(
            WIDTH,
            HEIGHT,
            vec![default_cell("B"); usize::from(WIDTH) * usize::from(HEIGHT)],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));

        let cup_count = output.iter().filter(|&&byte| byte == b'H').count();
        assert!(
            cup_count <= usize::from(HEIGHT) + 2,
            "one dense scroll frame should need at most one CUP per row plus cursor anchors, got {cup_count}"
        );
        assert!(
            output.len() <= 16_290,
            "one dense scroll frame should stay below 25% of the 65,161-byte live baseline, got {} bytes",
            output.len()
        );
    }

    #[test]
    fn batched_ascii_diff_replays_to_current_frame() {
        let prev = make_frame(4, 3, vec![default_cell("A"); 12]);
        let curr = make_frame(4, 3, vec![default_cell("B"); 12]);
        let mut terminal = shepr_vt::Terminal::new(4, 3, 0);

        let mut initial = Vec::new();
        blit_frame_to(&mut initial, &prev, None);
        terminal.write(&initial);

        let mut diff = Vec::new();
        blit_frame_to(&mut diff, &curr, Some(&prev));
        terminal.write(&diff);

        let mut scratch = String::new();
        for row in 0_usize..3 {
            let mut cells = Vec::new();
            terminal
                .visit_screen_row_text(shepr_vt::ScreenRow(row), &mut scratch, |_, _, text| {
                    cells.push(text.to_owned());
                })
                .expect("test precondition");
            assert_eq!(cells, vec!["B"; 4]);
        }
    }

    #[test]
    fn encoder_size_change_repaints_without_clearing() {
        let prev = make_frame(2, 2, vec![default_cell("A"); 4]);
        let curr = make_frame(3, 2, vec![default_cell("B"); 6]);
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&prev, false);
        encoder.commit(prev, &initial);

        let encoded = encoder.encode(&curr, false);
        let output = String::from_utf8(encoded.bytes).expect("test precondition");

        assert!(!output.contains("\x1b[2J"));
        assert!(output.bytes().filter(|byte| *byte == b'B').count() >= 6);
    }

    #[test]
    fn encoder_forced_repaint_writes_all_cells_without_clearing() {
        let frame = make_frame(3, 2, vec![default_cell("A"); 6]);
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&frame, false);
        encoder.commit(frame.clone(), &initial);

        let encoded = encoder.encode(&frame, true);
        let output = String::from_utf8(encoded.bytes).expect("test precondition");

        assert!(!output.contains("\x1b[2J"));
        assert!(output.bytes().filter(|byte| *byte == b'A').count() >= 6);
    }

    #[test]
    fn encoder_rejects_a_frame_with_a_cell_count_mismatch() {
        let malformed = make_frame(2, 1, vec![default_cell("x")]);
        let mut encoder = BlitEncoder::new();

        let encoded = encoder.encode(&malformed, false);

        assert!(encoded.bytes.is_empty());
        encoder.commit(malformed, &encoded);
        assert!(encoder.last_frame.is_none());
        assert!(encoder.encode_patch(&[], None, false).is_none());
    }

    #[test]
    fn retained_patch_matches_full_diff_and_updates_the_encoder_baseline() {
        let previous = make_frame(
            4,
            2,
            vec![
                default_cell("a"),
                default_cell("b"),
                default_cell("c"),
                default_cell("d"),
                default_cell("e"),
                default_cell("f"),
                default_cell("g"),
                default_cell("h"),
            ],
        );
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&previous, false);
        encoder.commit(previous.clone(), &initial);

        let rows = vec![PaneSurfacePatchRow {
            x: 0,
            y: 1,
            cells: vec![
                default_cell("E"),
                default_cell("f"),
                default_cell("G"),
                default_cell("h"),
            ],
        }];
        let cursor = Some(CursorState {
            x: 3,
            y: 1,
            visible: true,
            shape: shepr_protocol::CursorShapeParam::SteadyBlock,
        });
        let mut expected = previous;
        expected.cells[4..8].clone_from_slice(&rows[0].cells);
        expected.cursor = cursor.clone();

        let full_diff = encoder.encode(&expected, false);
        let patch = encoder
            .encode_patch(&rows, cursor.clone(), false)
            .expect("valid retained patch");
        assert_eq!(patch.bytes, full_diff.bytes);
        assert!(encoder.commit_patch(&rows, cursor, &patch));
        assert!(encoder.is_current(&expected));
    }

    #[test]
    fn retained_patch_width_transition_matches_full_diff_with_following_cell() {
        let previous = make_frame(
            3,
            1,
            vec![default_cell("界"), default_cell("z"), default_cell("q")],
        );
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&previous, false);
        encoder.commit(previous.clone(), &initial);

        let rows = vec![PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: vec![default_cell("x"), default_cell("z")],
        }];
        let mut expected = previous;
        expected.cells[0..2].clone_from_slice(&rows[0].cells);

        let full_diff = encoder.encode(&expected, false);
        let patch = encoder
            .encode_patch(&rows, None, false)
            .expect("valid retained patch");
        assert_eq!(patch.bytes, full_diff.bytes);
    }

    #[test]
    fn retained_patch_rejects_overlapping_unsorted_and_empty_rows() {
        let frame = make_frame(
            3,
            1,
            vec![default_cell("a"), default_cell("b"), default_cell("c")],
        );
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&frame, false);
        encoder.commit(frame, &initial);
        let rows = vec![
            PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: vec![default_cell("A"), default_cell("B")],
            },
            PaneSurfacePatchRow {
                x: 1,
                y: 0,
                cells: vec![default_cell("C")],
            },
        ];

        assert!(encoder.encode_patch(&rows, None, false).is_none());
        let mut reversed = rows.clone();
        reversed.reverse();
        assert!(encoder.encode_patch(&reversed, None, false).is_none());

        // Disjoint runs are still rejected out of row-major order.
        let tail = PaneSurfacePatchRow {
            x: 2,
            y: 0,
            cells: vec![default_cell("Z")],
        };
        let unsorted = vec![tail.clone(), rows[0].clone()];
        assert!(encoder.encode_patch(&unsorted, None, false).is_none());
        let empty = vec![PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: Vec::new(),
        }];
        assert!(encoder.encode_patch(&empty, None, false).is_none());

        // Touching runs in row-major order are disjoint.
        let sorted = vec![rows[0].clone(), tail];
        assert!(encoder.encode_patch(&sorted, None, false).is_some());
    }

    #[test]
    fn metadata_only_patches_do_not_write_but_cursor_changes_do() {
        let mut frame = make_frame(3, 1, vec![default_cell("a"); 3]);
        let mut encoder = BlitEncoder::new();
        for cursor in [
            None,
            Some(CursorState {
                x: 0,
                y: 0,
                visible: false,
                shape: shepr_protocol::CursorShapeParam::SteadyBlock,
            }),
        ] {
            frame.cursor = cursor.clone();
            let initial = encoder.encode(&frame, false);
            encoder.commit(frame.clone(), &initial);
            let encoded = encoder
                .encode_patch(&[], cursor.clone(), false)
                .expect("test precondition");
            assert!(encoded.bytes.is_empty());
            assert!(encoder.commit_patch(&[], cursor, &encoded));
            assert!(encoder.is_current(&frame));
        }
        for cursor in [
            CursorState {
                x: 2,
                y: 0,
                visible: false,
                shape: shepr_protocol::CursorShapeParam::SteadyBlock,
            },
            CursorState {
                x: 2,
                y: 0,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::SteadyBlock,
            },
        ] {
            let encoded = encoder
                .encode_patch(&[], Some(cursor.clone()), false)
                .expect("test precondition");
            assert!(String::from_utf8_lossy(&encoded.bytes).contains("\x1b[1;3H"));
            assert!(encoder.commit_patch(&[], Some(cursor), &encoded));
        }
        // Switching to a client-drawn cursor must still hide the visible host cursor.
        let encoded = encoder
            .encode_patch(
                &[],
                encoder
                    .last_frame
                    .as_ref()
                    .expect("test precondition")
                    .cursor
                    .clone(),
                true,
            )
            .expect("test precondition");
        assert!(String::from_utf8_lossy(&encoded.bytes).contains("\x1b[?25l"));
    }

    #[test]
    fn retained_patch_preserves_the_client_drawn_cursor_overlay() {
        let mut previous = make_frame(
            3,
            1,
            vec![default_cell("a"), default_cell("b"), default_cell("c")],
        );
        previous.cursor = Some(CursorState {
            x: 0,
            y: 0,
            visible: true,
            shape: shepr_protocol::CursorShapeParam::Default,
        });
        let previous_drawn = frame_with_drawn_cursor(previous.clone());
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode_with_suppressed_visible_cursor(&previous_drawn, false);
        encoder.commit(previous_drawn, &initial);

        let rows = vec![PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: vec![default_cell("A"), default_cell("b"), default_cell("c")],
        }];
        let cursor = Some(CursorState {
            x: 1,
            y: 0,
            visible: true,
            shape: shepr_protocol::CursorShapeParam::Default,
        });
        let drawn_rows = encoder
            .patch_rows_with_drawn_cursor(&rows, cursor.as_ref())
            .expect("drawn cursor patch rows");
        let mut expected = previous;
        expected.cells[0..3].clone_from_slice(&rows[0].cells);
        expected.cursor = cursor.clone();
        let expected = frame_with_drawn_cursor(expected);

        let full_diff = encoder.encode_with_suppressed_visible_cursor(&expected, false);
        let patch = encoder
            .encode_patch(&drawn_rows, cursor.clone(), true)
            .expect("valid drawn cursor patch");
        assert_eq!(patch.bytes, full_diff.bytes);
        assert!(encoder.commit_patch(&drawn_rows, cursor, &patch));
        assert!(encoder.is_current(&expected));
    }

    #[test]
    fn blit_frame_positions_cursor() {
        let frame = FrameData {
            cells: vec![default_cell("A")],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(
            output_str.contains("\x1b[1;1H"),
            "should position cursor at (1,1)"
        );
    }

    #[test]
    fn blit_frame_hides_cursor_when_invisible() {
        let frame = FrameData {
            cells: vec![default_cell("A")],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: false,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(
            output_str.contains("\x1b[?25l"),
            "should hide cursor when invisible"
        );
    }

    #[test]
    fn blit_frame_no_cursor_hides_cursor() {
        let frame = FrameData {
            cells: vec![default_cell("A")],
            width: 1,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(
            output_str.contains("\x1b[?25l"),
            "should hide cursor when no cursor state"
        );
    }

    #[test]
    fn blit_frame_restores_cursor_visibility() {
        // First frame: cursor hidden.
        let prev = FrameData {
            cells: vec![default_cell("A")],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: false,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &prev, None);
        assert!(
            String::from_utf8(output)
                .expect("test precondition")
                .contains("\x1b[?25l"),
            "first frame should hide cursor"
        );

        // Second frame: cursor visible - should restore visibility.
        let curr = FrameData {
            cells: vec![default_cell("B")],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(
            output_str.contains("\x1b[?25h"),
            "second frame should restore cursor visibility with ?25h"
        );
        assert!(
            output_str.contains("\x1b[1;1H"),
            "should position cursor before showing it"
        );
    }

    #[test]
    fn blit_frame_positions_cursor_before_showing_it() {
        let prev = FrameData {
            cells: vec![default_cell("A"); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };
        let mut curr = prev.clone();
        curr.cells[0] = default_cell("B");
        curr.cursor = Some(CursorState {
            x: 2,
            y: 2,
            visible: true,
            shape: shepr_protocol::CursorShapeParam::Default,
        });

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).expect("test precondition");
        let final_move = output_str
            .rfind("\x1b[3;3H")
            .expect("should move cursor to final position");
        let show = output_str
            .rfind("\x1b[?25h")
            .expect("should show cursor after positioning it");

        assert!(
            final_move < show,
            "should move cursor to final position before showing it"
        );
    }

    #[test]
    fn blit_frame_parks_hidden_cursor_at_last_visible_position() {
        let visible = FrameData {
            cells: vec![default_cell("A"); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 1,
                y: 1,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };
        let hidden = FrameData {
            cells: vec![default_cell("B"); 9],
            width: 3,
            height: 3,
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();

        blit_frame_to_with_cursor_memory(
            &mut output,
            &visible,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );
        output.clear();
        blit_frame_to_with_cursor_memory(
            &mut output,
            &hidden,
            Some(&visible),
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let output_str = String::from_utf8(output).expect("test precondition");
        let park = output_str
            .rfind("\x1b[2;2H")
            .expect("should park hidden cursor at last visible position");
        let hide = output_str
            .rfind("\x1b[?25l")
            .expect("should keep hidden cursor hidden");
        assert!(park < hide, "should park cursor before hiding it");
    }

    #[test]
    fn blit_frame_parks_hidden_cursor_at_bottom_right_without_history() {
        let frame = FrameData {
            cells: vec![default_cell("A"); 6],
            width: 3,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();

        blit_frame_to_with_cursor_memory(
            &mut output,
            &frame,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let output_str = String::from_utf8(output).expect("test precondition");
        assert!(
            output_str.contains("\x1b[2;3H\x1b[?25l"),
            "should park hidden cursor at bottom-right before ending the frame"
        );
    }

    #[test]
    fn blit_frame_hides_previous_visible_cursor_when_next_frame_has_none() {
        let prev = FrameData {
            cells: vec![default_cell("A")],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::Default,
            }),
            hyperlinks: Vec::new(),
        };
        let curr = FrameData {
            cells: vec![default_cell("B")],
            width: 1,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));

        assert!(
            String::from_utf8(output)
                .expect("test precondition")
                .contains("\x1b[?25l"),
            "diff redraw should hide a previously visible cursor when the next frame has none"
        );
    }

    #[test]
    fn full_redraw_skips_trailing_cells_covered_by_wide_graphemes() {
        let frame = FrameData {
            cells: vec![
                default_cell(WIDE_GRAPHEME),
                default_cell(" "),
                default_cell("Z"),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);
        let output_str = String::from_utf8(output).expect("test precondition");

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(!output_str.contains("\x1b[1;2H"));
        assert!(output_str.contains("\x1b[1;3H"));
    }

    #[test]
    fn full_redraw_skips_trailing_cells_covered_by_halfwidth_voiced_kana() {
        let frame = FrameData {
            cells: vec![
                default_cell(HALFWIDTH_VOICED_KANA),
                make_skip_cell(" "),
                default_cell("Z"),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);
        let output_str = String::from_utf8(output).expect("test precondition");

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(!output_str.contains("\x1b[1;2H"));
        assert!(output_str.contains("\x1b[1;3H"));
    }

    #[test]
    fn diff_redraw_reveals_cells_hidden_by_previous_wide_graphemes() {
        let prev = FrameData {
            cells: vec![
                default_cell(WIDE_GRAPHEME),
                default_cell(" "),
                default_cell("Z"),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let curr = FrameData {
            cells: vec![default_cell("A"), default_cell(" "), default_cell("Z")],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).expect("test precondition");

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(
            output_str.contains("\x1b[1;2H"),
            "cells hidden by a previous wide grapheme must be redrawn when they become visible"
        );
    }

    #[test]
    fn diff_redraw_skips_new_trailing_cells_covered_by_wide_graphemes() {
        let prev = FrameData {
            cells: vec![default_cell("A"), default_cell("B"), default_cell("Z")],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let curr = FrameData {
            cells: vec![
                default_cell(WIDE_GRAPHEME),
                default_cell(" "),
                default_cell("Z"),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).expect("test precondition");

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(!output_str.contains("\x1b[1;2H"));
    }

    #[test]
    fn diff_redraw_reveals_cells_hidden_by_previous_halfwidth_voiced_kana() {
        let prev = FrameData {
            cells: vec![
                default_cell(HALFWIDTH_VOICED_KANA),
                make_skip_cell(" "),
                default_cell("Z"),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let curr = FrameData {
            cells: vec![default_cell("A"), default_cell(" "), default_cell("Z")],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).expect("test precondition");

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(
            output_str.contains("\x1b[1;2H"),
            "cells hidden by a previous halfwidth voiced kana must be redrawn when visible"
        );
    }
}
