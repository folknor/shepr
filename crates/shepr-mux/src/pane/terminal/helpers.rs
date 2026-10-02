use super::*;
use shepr_core::limits::PALETTE_COLOR_COUNT;

/// What the core queued for the pane to deliver.
pub(super) struct CoreEffects {
    pub(super) terminal_title_changed: bool,
    pub(super) clipboard_writes: Vec<Vec<u8>>,
    pub(super) dropped_clipboard_store_bytes: Vec<usize>,
    pub(super) reported_cwd: Option<std::path::PathBuf>,
    pub(super) terminal_responses: Vec<Bytes>,
    /// The child set a default colour: the program that did it is to be
    /// looked up once the terminal, content and reply-order locks are released
    /// ([`PaneTerminal::resolve_default_color_owner`]).
    pub(super) default_color_owner_pending: bool,
}

/// Collects every effect the core has queued, whichever write or flush
/// produced it, and keeps the default-colour owner bookkeeping in step.
pub(super) fn collect_core_effects(core: &mut PaneTerminalCore) -> CoreEffects {
    let terminal_responses = drain_terminal_responses(core);
    let terminal_title_changed = core
        .agent_osc_state
        .apply_terminal_updates(&mut core.terminal);
    let clipboard_writes = core.terminal.take_clipboard_writes();
    let dropped_clipboard_store_bytes = core.terminal.take_dropped_clipboard_store_bytes();
    let reported_cwd = core
        .terminal
        .take_pwd_changes()
        .into_iter()
        .filter_map(|report| parse_reported_cwd(&report))
        .next_back();
    let default_color_owner_pending = note_default_color_change(core);
    CoreEffects {
        terminal_title_changed,
        clipboard_writes,
        dropped_clipboard_store_bytes,
        reported_cwd,
        terminal_responses,
        default_color_owner_pending,
    }
}

/// Drops queued effects that must never reach the live child or the app
/// (restored history).
pub(super) fn discard_core_effects(terminal: &mut shepr_vt::Terminal) {
    let _ = terminal.take_pty_responses();
    let _ = terminal.take_clipboard_writes();
    let _ = terminal.take_dropped_clipboard_store_bytes();
    let _ = terminal.take_pwd_changes();
    let _ = terminal.take_title_update();
    let _ = terminal.take_progress_update();
    let _ = terminal.take_default_color_set();
}

pub(super) fn has_default_color_override(terminal: &shepr_vt::Terminal) -> bool {
    terminal
        .default_color_override(shepr_vt::DefaultColor::Foreground)
        .is_some()
        || terminal
            .default_color_override(shepr_vt::DefaultColor::Background)
            .is_some()
}

/// Keeps the default-colour owner in step with the core: forgets it once no
/// override is left (the child reset it with OSC 110/111, or RIS), and
/// reports whether the child just set an override whose owner still has to
/// be looked up. The lookup scans `/proc`, so the caller does it after
/// releasing the terminal, content and reply-order locks
/// ([`PaneTerminal::resolve_default_color_owner`]); `shell_pid` 0 (no
/// child yet) is handled there.
pub(super) fn note_default_color_change(core: &mut PaneTerminalCore) -> bool {
    let set = core.terminal.take_default_color_set();
    if set {
        core.default_color_generation = core.default_color_generation.wrapping_add(1);
    }
    if !has_default_color_override(&core.terminal) {
        core.transient_default_color_owner_pgid = None;
        return false;
    }
    set
}

/// Collects the core's queued replies, answering OSC colour queries from the
/// host theme where the pane owns the answer.
pub(super) fn drain_terminal_responses(core: &mut PaneTerminalCore) -> Vec<Bytes> {
    let responses = core.terminal.take_pty_responses();
    let mut replies = Vec::with_capacity(responses.len());
    for response in responses {
        match response {
            shepr_vt::PtyResponse::Bytes(bytes) => replies.push(Bytes::from(bytes)),
            shepr_vt::PtyResponse::ColorQuery(query) => {
                replies.extend(color_query_response(&query));
            }
        }
    }
    replies
}

/// The core snapshots each colour when the query is dispatched, resolving
/// child state and host defaults or the built-in palette as appropriate. This
/// only picks the reply form. A default colour the child set itself is echoed in
/// the form it asked for; everything else is reported the way shepr reports
/// host colours, ST-terminated. No reply for a default colour nobody has set.
pub(super) fn color_query_response(query: &shepr_vt::ColorQuery) -> Option<Bytes> {
    let color = query.core_color()?;
    if query.child_override() {
        return Some(Bytes::from(query.encode(color)));
    }
    let command = match query.target() {
        shepr_vt::ColorQueryTarget::Foreground => "10".to_owned(),
        shepr_vt::ColorQueryTarget::Background => "11".to_owned(),
        shepr_vt::ColorQueryTarget::Cursor => "12".to_owned(),
        shepr_vt::ColorQueryTarget::Palette(index) => format!("4;{index}"),
    };
    Some(osc_rgb_response(&command, color.r, color.g, color.b))
}

pub(super) fn current_cursor_state(core: &mut PaneTerminalCore) -> Option<TerminalCursorState> {
    let PaneTerminalCore {
        terminal,
        render_state,
        ..
    } = core;
    let terminal: &shepr_vt::Terminal = terminal;
    render_state.update(terminal);
    cursor_state_from_render_state(render_state, terminal.cursor_shape_overridden())
}

pub(super) fn cursor_state_from_render_state(
    render_state: &mut shepr_vt::RenderState,
    cursor_shape_overridden: bool,
) -> Option<TerminalCursorState> {
    let cursor = render_state.cursor();
    let viewport = cursor.viewport?;
    let shape = if cursor_shape_overridden {
        decscusr_cursor_shape(cursor.visual_style, cursor.blinking)
    } else {
        shepr_protocol::CursorShapeParam::Default
    };
    Some(TerminalCursorState {
        x: viewport.x,
        y: viewport.y,
        visible: cursor.visible,
        shape,
    })
}

pub(super) fn terminal_collect_dirty_patch(
    core: &mut PaneTerminalCore,
    area_width: u16,
    area_height: u16,
) -> TerminalDirtyPatchCollection {
    macro_rules! finish {
        ($outcome:expr) => {{
            return TerminalDirtyPatchCollection {
                outcome: $outcome,
                fallback_reason: None,
            };
        }};
    }
    macro_rules! fallback {
        ($reason:literal) => {{
            return TerminalDirtyPatchCollection {
                outcome: TerminalDirtyPatchOutcome::Fallback,
                fallback_reason: Some($reason),
            };
        }};
    }

    let host_theme = core.host_terminal_theme;
    let initial_default_foreground = core.initial_default_foreground;
    let initial_default_background = core.initial_default_background;
    let PaneTerminalCore {
        terminal,
        render_state,
        ..
    } = core;
    let terminal: &shepr_vt::Terminal = terminal;
    render_state.update(terminal);
    match render_state.dirty() {
        shepr_vt::Dirty::Clean => finish!(TerminalDirtyPatchOutcome::Clean),
        shepr_vt::Dirty::Partial | shepr_vt::Dirty::Full => {}
    }

    let colors = render_state.colors();
    let default_bg = terminal_default_bg(colors.background, host_theme, initial_default_background);
    let default_fg = terminal_default_fg(colors.foreground, host_theme, initial_default_foreground);
    let resolved_fg = Some(terminal_color(colors.foreground));
    let resolved_bg = Some(terminal_color(colors.background));
    let default_palette = terminal.default_palette();
    let palette_overrides = PaletteOverrides::new(&colors.palette, &default_palette);

    let mut symbol_scratch = String::new();
    let mut patch_rows = Vec::new();
    for row in render_state.dirty_rows() {
        let y = row.y();
        if y >= area_height {
            break;
        }
        let mut patch_cells = Vec::with_capacity(usize::from(area_width));
        let mut x = 0u16;
        for cell_view in row.cells().take(usize::from(area_width)) {
            let basic = cell_view.basic_data();
            if basic.has_hyperlink {
                fallback!("hyperlink_present");
            }
            let paint = terminal_cell_paint(
                &cell_view,
                &basic,
                default_fg,
                default_bg,
                resolved_fg,
                resolved_bg,
                palette_overrides.as_ref(),
            );
            let symbol =
                terminal_buffer_symbol_into(&cell_view, basic.wide, &mut symbol_scratch).to_owned();
            patch_cells.push(paint.into_cell(symbol, terminal_grid_width(basic.wide)));
            x = x.saturating_add(1);
        }
        while x < area_width {
            patch_cells.push(
                CellPaint::blank(default_fg, default_bg)
                    .into_cell(" ".to_owned(), GridCellWidth::One),
            );
            x += 1;
        }
        // The same rule as a full render at this width; each recipient
        // narrower than it applies it again to its own cut.
        shepr_protocol::normalize_pane_row(&mut patch_cells);
        patch_rows.push((y, patch_cells));
    }

    // Nothing above mutates dirty state. Only clear it after every row has
    // been collected successfully, so a safety fallback leaves the next
    // collection with the same information. Rows below the area were not
    // collected: they stay dirty, and so does the overall state, so it only
    // reads Clean when no row is left to send.
    //
    // A collected row is cleared even when `area_width < cols`. The width is
    // the widest `inner_rect` among the clients receiving this patch, so the
    // columns past it are shown by no client holding a retained baseline. A
    // client that does show them (not a recipient, or whose view later widens)
    // is owed or promoted to a full render, which ignores dirty flags.
    let mut rows_left = false;
    for row in render_state.iter_rows() {
        if row.y() < area_height {
            row.clear_dirty();
        } else if row.is_dirty() {
            rows_left = true;
        }
    }
    let remaining = if rows_left {
        shepr_vt::Dirty::Partial
    } else {
        shepr_vt::Dirty::Clean
    };
    render_state.set_dirty(remaining);

    finish!(TerminalDirtyPatchOutcome::Patch(TerminalDirtyPatch {
        rows: patch_rows
    }));
}

/// The detector's snapshot: the active screen's rows up to the last content
/// (or cursor) row, never anything above the screen. After ED2, Ctrl-L or an
/// agent redrawing from the top, alacritty has pushed the previous frame into
/// history; reading a screen's worth of rows ending at the last content row
/// would hand the detector that stale frame (an old "proceed?" blocker, say).
/// Restored history seeded into the primary screen reads as blank rows: it is
/// display history, not evidence of what a later agent is doing.
pub(super) fn terminal_detection_text(
    terminal: &shepr_vt::Terminal,
) -> Result<String, shepr_vt::ReadError> {
    let screen_rows = usize::from(terminal.rows()).max(1);
    let Some((start, end, _)) = terminal_recent_read_range(terminal, screen_rows)? else {
        return Ok(String::new());
    };
    let screen_start = terminal.total_rows().saturating_sub(screen_rows);
    let primary = terminal.active_screen() == shepr_vt::ActiveScreen::Primary;
    let mut rows = Vec::with_capacity(screen_rows);
    let mut scratch = String::new();
    for row in start.max(screen_start)..=end {
        let mut text = String::new();
        let seeded =
            terminal_screen_row_into_with_seeded(terminal, ScreenRow(row), &mut scratch, &mut text);
        if primary && seeded {
            text.clear();
        }
        rows.push(text);
    }
    trim_trailing_blank_rows(&mut rows);
    Ok(recent_text_from_rows(&rows, screen_rows))
}

/// The screen rows a recent read covers, on the active screen: while the
/// alternate screen is active that is the full-screen program's frame, never
/// the primary history (alacritty offers no access to the inactive grid).
/// History persistence must not take that for history; it reads through
/// [`PaneHistorySource::refresh`], which says so instead.
pub(super) fn terminal_recent_read_range(
    terminal: &shepr_vt::Terminal,
    lines: usize,
) -> Result<Option<(usize, usize, u16)>, shepr_vt::ReadError> {
    let total_rows = terminal.total_rows();
    let cols = terminal.cols();
    if total_rows == 0 || cols == 0 || lines == 0 {
        return Ok(None);
    }

    let physical_end = total_rows.saturating_sub(1);
    if terminal.active_screen() != shepr_vt::ActiveScreen::Primary {
        let start = physical_end.saturating_add(1).saturating_sub(lines);
        return Ok(Some((start, physical_end, cols)));
    }

    let rows = usize::from(terminal.rows());
    if rows == 0 {
        return Ok(None);
    }
    let viewport_start = total_rows.saturating_sub(rows);
    let cursor_row = viewport_start
        .saturating_add(usize::from(terminal.cursor_y()))
        .min(total_rows.saturating_sub(1));
    let mut last_content_row = None;
    let mut scratch = String::new();
    let mut text = String::new();
    for row in (viewport_start..total_rows).rev() {
        terminal_screen_row_into(terminal, ScreenRow(row), &mut scratch, &mut text);
        if !text.trim().is_empty() {
            last_content_row = Some(row);
            break;
        }
    }
    let end =
        last_content_row.map_or_else(|| total_rows.saturating_sub(1), |row| row.max(cursor_row));
    let start = end.saturating_add(1).saturating_sub(lines);
    Ok(Some((start, end, cols)))
}

pub(super) fn terminal_scroll_metrics(terminal: &shepr_vt::Terminal) -> ScrollMetrics {
    let scrollbar = terminal.scrollbar();
    ScrollMetrics {
        offset_from_bottom: scrollbar
            .total
            .saturating_sub(scrollbar.offset + scrollbar.len),
        max_offset_from_bottom: scrollbar.total.saturating_sub(scrollbar.len),
        viewport_rows: scrollbar.len,
        history_origin: terminal.history_origin(),
    }
}

pub(super) fn terminal_set_scroll_offset_from_bottom(
    terminal: &mut shepr_vt::Terminal,
    offset_from_bottom: usize,
) {
    let scrollbar = terminal.scrollbar();
    let max_offset = scrollbar.total.saturating_sub(scrollbar.len);
    let offset_from_bottom = offset_from_bottom.min(max_offset);
    if offset_from_bottom == 0 {
        terminal.scroll_viewport_bottom();
    } else {
        terminal.scroll_viewport_row(ScreenRow(max_offset - offset_from_bottom));
    }
}

pub(super) fn terminal_extract_selection<P>(
    core: &mut PaneTerminalCore,
    selection: &shepr_vt::selection::Selection<P>,
) -> Option<String> {
    let (start, end) = selection.ordered_rows();
    let terminal = &core.terminal;
    let origin = terminal.history_origin();
    let start_row = start.row.screen_row(origin)?;
    let end_row = end.row.screen_row(origin)?;
    terminal
        .read_text_screen(
            Point::new(start_row, start.col),
            Point::new(end_row, end.col),
        )
        .ok()
}

/// Writes screen row `y`'s plain text into `line`, trailing blanks trimmed
/// (empty for a row that is not retained). Straight from the grid, with no
/// per-cell copies: this runs per detection tick for every agent pane.
pub(super) fn terminal_screen_row_into(
    terminal: &shepr_vt::Terminal,
    y: ScreenRow,
    scratch: &mut String,
    line: &mut String,
) {
    let _ = terminal_screen_row_into_inner::<false>(terminal, y, scratch, line);
}

pub(super) fn terminal_screen_row_into_with_seeded(
    terminal: &shepr_vt::Terminal,
    y: ScreenRow,
    scratch: &mut String,
    line: &mut String,
) -> bool {
    terminal_screen_row_into_inner::<true>(terminal, y, scratch, line)
}

// The const mode selects the VT visitor at compile time, so plain reads keep
// using the unseeded path without a per-cell provenance check.
#[inline(always)]
fn terminal_screen_row_into_inner<const TRACK_SEEDED: bool>(
    terminal: &shepr_vt::Terminal,
    y: ScreenRow,
    scratch: &mut String,
    line: &mut String,
) -> bool {
    line.clear();
    let seeded = {
        let mut visit = |_, wide, text: &str| {
            if wide != shepr_vt::CellWide::SpacerTail {
                line.push_str(text);
            }
        };
        if TRACK_SEEDED {
            terminal
                .visit_screen_row_text_with_seeded(y, scratch, &mut visit)
                .is_some_and(|(_, seeded)| seeded)
        } else {
            terminal.visit_screen_row_text(y, scratch, &mut visit);
            false
        }
    };
    line.truncate(line.trim_end().len());
    seeded
}

pub(super) fn terminal_blank_symbol_for_width(wide: shepr_vt::CellWide) -> &'static str {
    match wide {
        shepr_vt::CellWide::Wide => "  ",
        shepr_vt::CellWide::SpacerTail => "",
        shepr_vt::CellWide::Narrow | shepr_vt::CellWide::SpacerHead => " ",
    }
}

pub(super) fn terminal_buffer_symbol_into<'a>(
    cells: &shepr_vt::CellView<'_>,
    wide: shepr_vt::CellWide,
    symbol_scratch: &'a mut String,
) -> &'a str {
    symbol_scratch.clear();
    match wide {
        shepr_vt::CellWide::SpacerTail => {}
        shepr_vt::CellWide::SpacerHead => symbol_scratch.push(' '),
        shepr_vt::CellWide::Narrow | shepr_vt::CellWide::Wide => {
            cells.grapheme_text_into(symbol_scratch);
            if symbol_scratch.is_empty() {
                symbol_scratch.clear();
                symbol_scratch.push(' ');
            }
        }
    }

    let expected_width = match wide {
        shepr_vt::CellWide::Wide => 2,
        shepr_vt::CellWide::Narrow | shepr_vt::CellWide::SpacerHead => 1,
        shepr_vt::CellWide::SpacerTail => 0,
    };
    let actual_width = symbol_scratch.width();
    if actual_width != expected_width
        && !(wide == shepr_vt::CellWide::Narrow && actual_width == 2)
        && !(wide == shepr_vt::CellWide::Narrow
            && shepr_vt::is_halfwidth_katakana_voiced_mark(symbol_scratch))
        && !(wide == shepr_vt::CellWide::Wide
            && shepr_vt::is_halfwidth_katakana_voiced_grapheme(symbol_scratch))
    {
        symbol_scratch.clear();
        symbol_scratch.push_str(terminal_blank_symbol_for_width(wide));
    }

    symbol_scratch.as_str()
}

pub(super) fn terminal_grid_width(wide: shepr_vt::CellWide) -> GridCellWidth {
    match wide {
        shepr_vt::CellWide::Narrow
        | shepr_vt::CellWide::SpacerHead
        | shepr_vt::CellWide::SpacerTail => GridCellWidth::One,
        shepr_vt::CellWide::Wide => GridCellWidth::Two,
    }
}

/// The colours and text style of one pane cell, as the wire carries them.
/// Underline colour (SGR 58) has no wire form and is not read.
#[derive(Clone, Copy)]
pub(super) struct CellPaint {
    pub(super) fg: WireColor,
    pub(super) bg: WireColor,
    pub(super) style: WireStyle,
}

impl CellPaint {
    /// A blank cell: the terminal's default colours, `Reset` where the host's
    /// own default shows through.
    pub(super) fn blank(default_fg: Option<WireColor>, default_bg: Option<WireColor>) -> Self {
        Self {
            fg: default_fg.unwrap_or(WireColor::Reset),
            bg: default_bg.unwrap_or(WireColor::Reset),
            style: WireStyle::default(),
        }
    }

    pub(super) fn into_cell(self, symbol: String, grid_width: GridCellWidth) -> CellData {
        CellData {
            symbol,
            grid_width,
            fg: self.fg,
            bg: self.bg,
            style: self.style,
            skip: false,
            hyperlink: None,
        }
    }

    /// Overwrites `cell` completely, keeping its symbol allocation.
    pub(super) fn write_cell(
        self,
        cell: &mut CellData,
        symbol: &str,
        grid_width: GridCellWidth,
        hyperlink: Option<u32>,
    ) {
        cell.symbol.clear();
        cell.symbol.push_str(symbol);
        cell.grid_width = grid_width;
        cell.fg = self.fg;
        cell.bg = self.bg;
        cell.style = self.style;
        cell.skip = false;
        cell.hyperlink = hyperlink;
    }
}

pub(super) fn terminal_cell_paint(
    cells: &shepr_vt::CellView<'_>,
    basic: &shepr_vt::CellBasicData,
    default_fg: Option<WireColor>,
    default_bg: Option<WireColor>,
    resolved_fg: Option<WireColor>,
    resolved_bg: Option<WireColor>,
    palette_overrides: Option<&PaletteOverrides>,
) -> CellPaint {
    let mut fg = basic
        .style
        .fg_color
        .map(|color| terminal_cell_color(color, palette_overrides))
        .or_else(|| cells.fg_color().map(terminal_color))
        .or(default_fg);
    let mut bg = basic
        .style
        .bg_color
        .map(|color| terminal_cell_color(color, palette_overrides))
        .or_else(|| cells.bg_color().map(terminal_color))
        .or(default_bg);
    if basic.style.invisible {
        fg = bg.or(default_bg);
    }
    if basic.style.inverse {
        // When the background is transparent (None), resolve it to the
        // actual terminal background color before swapping.  Otherwise
        // the swapped fg becomes None (WireColor::Reset) which the host
        // terminal renders as its default foreground - the same hue as
        // the new bg, making inverse text invisible.
        if bg.is_none() {
            bg = resolved_bg;
        }
        if fg.is_none() {
            fg = resolved_fg;
        }
        std::mem::swap(&mut fg, &mut bg);
    }

    let mut flags = WireStyleFlags::default();
    if basic.style.bold {
        flags = flags.union(WireStyleFlags::BOLD);
    }
    if basic.style.faint {
        flags = flags.union(WireStyleFlags::DIM);
    }
    if basic.style.italic {
        flags = flags.union(WireStyleFlags::ITALIC);
    }
    if basic.style.strikethrough {
        flags = flags.union(WireStyleFlags::CROSSED_OUT);
    }
    CellPaint {
        fg: fg.unwrap_or(WireColor::Reset),
        bg: bg.unwrap_or(WireColor::Reset),
        style: WireStyle {
            flags,
            underline: basic.style.underline,
        },
    }
}

pub(super) fn osc_rgb_response(command: &str, r: u8, g: u8, b: u8) -> Bytes {
    let r = u16::from(r) * 257;
    let g = u16::from(g) * 257;
    let b = u16::from(b) * 257;
    Bytes::from(format!("\x1b]{command};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"))
}

pub(super) fn terminal_default_fg(
    color: shepr_vt::RgbColor,
    host_theme: shepr_termio::host_term::theme::TerminalTheme,
    initial_default_foreground: shepr_vt::RgbColor,
) -> Option<WireColor> {
    if let Some(host_foreground) = host_theme.foreground {
        if host_foreground == color {
            None
        } else {
            Some(terminal_color(color))
        }
    } else if initial_default_foreground != color {
        Some(terminal_color(color))
    } else {
        None
    }
}

pub(super) fn terminal_default_bg(
    color: shepr_vt::RgbColor,
    host_theme: shepr_termio::host_term::theme::TerminalTheme,
    initial_default_background: shepr_vt::RgbColor,
) -> Option<WireColor> {
    if let Some(host_background) = host_theme.background {
        if host_background == color {
            None
        } else {
            Some(terminal_color(color))
        }
    } else if initial_default_background != color {
        Some(terminal_color(color))
    } else {
        None
    }
}

// Palette entries the program redefined with OSC 4. Forwarding a palette index to the
// host makes it resolve against the host's own palette, discarding the redefinition.
// Only overridden entries become RGB; the rest stay indexed and keep following the
// host theme. None when nothing was redefined, which is the common case.
pub(super) struct PaletteOverrides([Option<shepr_vt::RgbColor>; PALETTE_COLOR_COUNT]);

impl PaletteOverrides {
    pub(super) fn new(
        active: &[shepr_vt::RgbColor; PALETTE_COLOR_COUNT],
        default: &[shepr_vt::RgbColor; PALETTE_COLOR_COUNT],
    ) -> Option<Self> {
        let mut overrides = [None; PALETTE_COLOR_COUNT];
        let mut any = false;
        for (index, (active, default)) in active.iter().zip(default.iter()).enumerate() {
            if active != default {
                overrides[index] = Some(*active);
                any = true;
            }
        }
        any.then_some(Self(overrides))
    }

    fn get(&self, index: u8) -> Option<shepr_vt::RgbColor> {
        self.0[usize::from(index)]
    }
}

pub(super) fn terminal_cell_color(
    color: shepr_vt::CellColor,
    palette_overrides: Option<&PaletteOverrides>,
) -> WireColor {
    match color {
        shepr_vt::CellColor::Palette(index) => {
            match palette_overrides.and_then(|overrides| overrides.get(index)) {
                Some(color) => terminal_color(color),
                None => WireColor::Indexed(index),
            }
        }
        shepr_vt::CellColor::Rgb(color) => terminal_color(color),
    }
}

pub(super) fn terminal_color(color: shepr_vt::RgbColor) -> WireColor {
    WireColor::Rgb(color.r, color.g, color.b)
}

pub(super) fn trim_trailing_blank_rows(rows: &mut Vec<String>) {
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
}

pub(super) fn recent_text_from_rows(rows: &[String], lines: usize) -> String {
    let start = rows.len().saturating_sub(lines);
    let text = rows[start..].join("\n");
    if text.is_empty() {
        text
    } else {
        format!("{text}\n")
    }
}

pub(super) fn should_probe_host_terminal_theme_restore(core: &PaneTerminalCore) -> bool {
    if core.transient_default_color_owner_pgid.is_none() || core.host_terminal_theme.is_empty() {
        return false;
    }

    core.terminal.active_screen() != shepr_vt::ActiveScreen::Alternate
}

#[cfg(test)]
pub(super) fn terminal_visible_ansi(
    core: &PaneTerminalCore,
) -> Result<String, shepr_vt::ReadError> {
    let rows = core.terminal.rows();
    let cols = core.terminal.cols();
    if rows == 0 || cols == 0 {
        return Ok(String::new());
    }
    let offset = core.terminal.scrollbar().offset;
    terminal_read_ansi_screen(
        &core.terminal,
        Point::new(ScreenRow(offset), 0),
        Point::new(
            ScreenRow(offset.saturating_add(usize::from(rows) - 1)),
            cols - 1,
        ),
    )
}

#[cfg(test)]
pub(super) fn terminal_recent_ansi(
    core: &mut PaneTerminalCore,
    lines: usize,
) -> Result<String, shepr_vt::ReadError> {
    let terminal = &core.terminal;
    let Some((start, end, cols)) = terminal_recent_read_range(terminal, lines)? else {
        return Ok(String::new());
    };
    let text = terminal_read_ansi_screen(
        terminal,
        Point::new(ScreenRow(start), 0),
        Point::new(ScreenRow(end), cols.saturating_sub(1)),
    )?;
    Ok(text)
}

#[cfg(test)]
fn terminal_read_ansi_screen(
    terminal: &shepr_vt::Terminal,
    start: Point<ScreenRow>,
    end: Point<ScreenRow>,
) -> Result<String, shepr_vt::ReadError> {
    let (mut text, content_end) = terminal.read_ansi_screen_carrying(
        start,
        end,
        &mut shepr_vt::AnsiCarry::default(),
        false,
    )?;
    text.truncate(content_end.unwrap_or(0));
    Ok(text)
}

#[cfg(test)]
fn terminal_text_rows(
    terminal: &shepr_vt::Terminal,
    start: usize,
    end: usize,
    lines: usize,
) -> Result<String, shepr_vt::ReadError> {
    let mut rows = Vec::with_capacity(end.saturating_sub(start).saturating_add(1));
    let mut scratch = String::new();
    for y in start..=end {
        let mut row = String::new();
        terminal_screen_row_into(terminal, ScreenRow(y), &mut scratch, &mut row);
        rows.push(row);
    }
    trim_trailing_blank_rows(&mut rows);
    Ok(recent_text_from_rows(&rows, lines))
}

#[cfg(test)]
pub(super) fn terminal_recent_text(
    core: &mut PaneTerminalCore,
    lines: usize,
) -> Result<String, shepr_vt::ReadError> {
    let terminal = &core.terminal;
    let Some((start, end, _)) = terminal_recent_read_range(terminal, lines)? else {
        return Ok(String::new());
    };
    let text = terminal_text_rows(terminal, start, end, lines)?;
    Ok(text)
}

#[cfg(test)]
pub(super) fn terminal_recent_text_unwrapped(
    core: &mut PaneTerminalCore,
    lines: usize,
) -> Result<String, shepr_vt::ReadError> {
    let terminal = &core.terminal;
    let Some((start, end, cols)) = terminal_recent_read_range(terminal, lines)? else {
        return Ok(String::new());
    };
    let text = terminal.read_text_screen(
        Point::new(ScreenRow(start), 0),
        Point::new(ScreenRow(end), cols.saturating_sub(1)),
    )?;
    Ok(text)
}

#[cfg(test)]
pub(super) fn terminal_normalize_buffer_symbol(symbol: &str, wide: shepr_vt::CellWide) -> String {
    let expected_width = match wide {
        shepr_vt::CellWide::Wide => 2,
        shepr_vt::CellWide::Narrow | shepr_vt::CellWide::SpacerHead => 1,
        shepr_vt::CellWide::SpacerTail => 0,
    };
    let actual_width = symbol.width();
    if actual_width == expected_width {
        return symbol.to_string();
    }

    if wide == shepr_vt::CellWide::Narrow && actual_width == 2 {
        return symbol.to_string();
    }
    if wide == shepr_vt::CellWide::Narrow && shepr_vt::is_halfwidth_katakana_voiced_mark(symbol) {
        return symbol.to_string();
    }
    if wide == shepr_vt::CellWide::Wide && shepr_vt::is_halfwidth_katakana_voiced_grapheme(symbol) {
        return symbol.to_string();
    }

    terminal_blank_symbol_for_width(wide).to_string()
}

#[cfg(test)]
pub(super) fn terminal_visible_text(core: &mut PaneTerminalCore) -> String {
    let PaneTerminalCore {
        terminal,
        render_state,
        ..
    } = core;
    render_state.update(terminal);
    let mut lines: Vec<_> = render_state
        .iter_rows()
        .map(|row| terminal_line_from_cells(row.cells()))
        .collect();
    trim_trailing_blank_rows(&mut lines);
    lines_to_text(&lines)
}

#[cfg(test)]
pub(super) fn terminal_line_from_cells<'a>(
    cells: impl Iterator<Item = shepr_vt::CellView<'a>>,
) -> String {
    let mut line = String::new();
    for cell in cells {
        line.push_str(&terminal_cell_symbol(&cell));
    }
    line.trim_end().to_string()
}

#[cfg(test)]
pub(super) fn terminal_cell_symbol(cells: &shepr_vt::CellView<'_>) -> String {
    if cells.wide() == shepr_vt::CellWide::SpacerTail {
        return String::new();
    }
    let text = cells.grapheme_text();
    if text.is_empty() {
        return " ".to_string();
    }
    text
}

#[cfg(test)]
pub(super) fn lines_to_text(lines: &[String]) -> String {
    let text = lines.join("\n");
    if text.is_empty() {
        text
    } else {
        format!("{text}\n")
    }
}
